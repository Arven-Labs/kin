# ADR 0044 — Mensagem perdida na janela do P36: ack de transporte não é entrega

- **Status**: **aceito e implementado** (2026-11-22, opção D, dono escolheu "reenvio silencioso"). Ver "Implementação e correções ao desenho" no fim. Desenho original por leitura de código; a implementação foi provada no CI (testes unitários, 5 mutações pegas, Lab verde, auditoria).
- **Relacionados**: ADR 0043 (`state_id` no `Hello`; deixou esta janela como limite aceito), ADR 0040 (re-handshake, P16), ADR 0041 (`Stale`), P19 (reenvio com espera crescente), P36, P33.
- **Área de segurança**: **não toca** criptografia nem autenticação (não muda `crypto`, `Hello` nem o fio na opção recomendada). Reenviar é cifrar de novo, na conversa nova.

## Contexto

Depois do ADR 0043, quando o `b` perde o banco e reconecta com a conexão antiga ainda de pé, o `a` refaz a conversa sozinho. Sobra uma janela: o que o `a` manda **depois** de o `b` reiniciar e **antes** de processar o `Hello` novo do `b` se perde, sem erro em nenhum lado.

Mecanismo, por leitura (confiança alta; o lab de NAT já mostrou o sintoma no #293, ainda sem a correção do 0043):

1. O `Node` do `b` responde o ack **imediatamente** ao receber os bytes (`crates/transport/src/node.rs`, `on_message_event`: `send_response(channel, ())`), antes de o `Chat` tentar decifrar. O tipo da resposta é `()` (`crates/transport/src/codec.rs:19`): o ack só diz "os bytes chegaram ao `Node`".
2. No `b` sem conversa, o `Chat` falha em `on_mls` (`chat.rs`, `Error::NoConversation`) e emite `Dropped` ("sem conversa com o peer").
3. No `a`, `MessageDelivered` (`chat.rs`, `NodeEvent::MessageDelivered`) remove a linha da outbox e emite `ChatEvent::Delivered`. A janela de envio é `SEND_WINDOW = 1`, então a fila inteira escoa mensagem a mensagem, cada uma acked e descartada, até o `Hello` chegar.
4. O comentário de `ChatEvent::ConversationReset` já admite: "o que eu enviei e deu `Delivered` antes de eu perceber a perda ficou perdido".

Ou seja, o `Delivered` de hoje prova que o **transporte** do outro lado recebeu, não que o **chat** aceitou. Em um chat, perder mensagem em silêncio é a pior falha; o custo aqui é uma janela curta, mas real.

## Critérios (em ordem)
Não perder mensagem em silêncio; compatibilidade de fio (o ADR 0043 já quebrou build antigo uma vez); risco de regredir o Lab (o #267 derrubou 8 cenários); simplicidade; custo de reverter; dificuldade de testar.

## Alternativas

| Opção | Resumo | Prós | Contras | Veredito |
|---|---|---|---|---|
| A. Aceitar | Manter o limite registrado até o P33 medir a frequência em rede real | Zero código | A perda segue silenciosa; o `ConversationReset` continua dizendo que ela existe | Válida se o dono preferir esperar |
| B. Segurar a fila na 2ª conexão | `hello_pending` também em conexão extra: só enviar depois de um `Hello` | Nenhuma mensagem sai para o `b` trocado | O `b` só manda `Hello` em `PeerConnected` (1ª conexão): num upgrade normal relay→direta nenhum `Hello` chega e a fila **trava**; a variante já regrediu 8 cenários no #267 | **Descartada** (medida) |
| C. Ack de entrega **no chat** | A resposta do `request-response` só sai depois do `on_mls` (ok ou nack); o `Delivered` passa a valer "o chat aceitou" | Cobre também a queda entre o ack e a gravação, e qualquer outra recusa do receptor; é a correção certa do conceito | Muda a API do `Node` (guardar o `channel` até o chat responder), o tipo da resposta (`()` vira um byte) e o fio; build antigo quebra de novo; nack exige reenvio com espera (P19) para não virar laço; mais superfície para testar | Evolução, se o P33 mostrar outras perdas |
| **D. Reenvio depois do reset** | O `a` guarda, em memória, as mensagens que receberam ack **depois** de aparecer uma conexão extra do mesmo peer (teto por peer e por tempo); quando a conversa é refeita (`ready` depois de `lost_state`), recoloca-as na outbox, cifradas de novo na conversa nova | Sem mudança de fio nem de transporte; só `chat.rs`; cobre exatamente esta janela; o `MessageId` deduplica no receptor | Não cobre outras perdas do receptor; o `Delivered` já foi mostrado à aplicação; mais um estado em memória | **Direção recomendada** |

## Decisão proposta (D, a confirmar com o dono)

- **Quando a janela abre**: no `PeerRoute` de um peer que já está em `connected` (conexão extra: `chat.rs`, `NodeEvent::PeerRoute`). Antes disso o `a` fala com a conexão de sempre e a janela não existe. O upgrade relay→direta também abre a janela, mas sem reset ela só expira.
- **O que se guarda**: ao receber `MessageDelivered` de uma mensagem comum (não espelho) com a janela aberta, copia-se a `Message` (id, pai, hora, texto) para um buffer em memória por peer. Mensagens acked **antes** da janela nunca entram, então a D **não** reenvia história antiga.
- **Teto**: por peer, o número de mensagens (proposta: 8, o mesmo `MAILBOX_WINDOW`, que já limita o quanto o `Chat` deixa sem ack) e a idade (proposta: 120 s). Antes de implementar, ler o limite do outro lado (memória `feedback-ler-o-limite-do-outro-lado-antes-de-medir`): o `SEND_WINDOW` é 1, então o que cabe na janela depende do ritmo da fila, não do teto da rede.
- **O que dispara**: o `ready` de uma conversa que **substitui** a anterior com `lost_state` (o mesmo ponto que emite `ConversationReset`, ADR 0040). Nesse instante o buffer do peer vai para a outbox (`enqueue`), com os mesmos `MessageId`, e o envio normal as cifra na conversa nova. Sem reset, o buffer expira ou some com `PeerDisconnected`.
- **Deduplicação**: o `b` novo perdeu o banco, então não viu esses ids; se algum já tivesse chegado, o `mark_seen` por `MessageId` descarta (`on_mls`).
- **Eventos**: a aplicação já viu `Delivered` para essas mensagens. Proposta: **não** emitir um segundo `Delivered`; o reenvio sai pelo caminho normal e o `Delivered` verdadeiro (agora de entrega real) é suprimido para ids reenviados, ou o `ConversationReset` passa a levar a contagem reenviada. **Em aberto, decisão de produto do dono.**
- **Irmãos (espelho `Payload::Sent`)**: fora desta decisão; o espelho tem tratamento próprio (ADR 0006).
- O comentário de `ChatEvent::ConversationReset` ("ficou perdido") passa a valer só para o que ficou fora do buffer.

### O que não sei
Por leitura, não rodei nada. A provar: (a) que `PeerRoute` de peer já conectado cobre o caso do lab (o `a` vê 2 `rota com` novas, medido no #293); (b) que o `ready` do reset roda depois de todos os `Delivered` da janela (ordem de eventos); (c) a ordem das mensagens reenviadas contra as novas que o usuário mandar durante o reset (o `seq` da outbox as põe no fim); (d) o custo de memória do buffer com muitos contatos.

## Limites (também depois da D)
- Mensagem acked **antes** de a conexão extra aparecer não é recuperada (não há como saber).
- Perda do receptor por outro motivo (queda entre o ack e a gravação, `TooOld`) segue fora; só a C cobre.
- Backup restaurado é `Stale` (ADR 0041).
- Relay real e NAT doméstico seguem no P33.

## Testes que vêm primeiro (quando houver código)
1. Integração (`crates/chat/tests/two_nodes.rs`, o caso `old_connection_still_open_case`): o `a` manda uma mensagem **antes** do reset (hoje o teste manda depois) e o `b` a recebe depois da troca, nas duas ordens de Peer ID. É o teste que falha hoje.
2. Unitário: mensagem acked fora da janela **não** é reenviada; acked na janela é; acima do teto, só as mais novas; passada a idade, nada; `PeerDisconnected` limpa.
3. Lab: um cenário `stale-connection-*` em que o `a` manda "no limbo" e o `b` passa a recebê-la (hoje só se mede).
4. Mutação lida (nome dos testes que falharam no log): remover o reenvio no `ready`; trocar o teto; abrir a janela também na 1ª conexão (deve reenviar história antiga e o teste 2 pega).

## Implementação e correções ao desenho (PR #300)

Só `crates/chat/src/chat.rs`; nada de fio, `Node` nem `crypto`.

- **Gatilho corrigido.** O `Node` emite um `PeerRoute` também na 1ª conexão (`node.rs`, logo depois do `PeerConnected`), então "PeerRoute de peer já conectado" abriria a janela em toda conexão normal. Regra implementada: `PeerConnected` marca `first_route_pending`; a 1ª `PeerRoute` seguinte o consome; as demais abrem a janela (`extra_connection`). Teste: `the_route_of_the_first_connection_does_not_open_the_window`.
- **Buffer** `replay` (por peer, máximo 8 = `MAX_REPLAY`, 120 s = `REPLAY_MAX_AGE`, poda por idade a cada entrada). Só entra mensagem **comum** (falha fechada: erro de leitura do banco não vira `Plain`).
- **Reenvio** em `replace_conversation`, antes do `ready` (que faz o `flush`): `outbox.enqueue` com o mesmo `MessageId`; o ack do reenvio não emite `Delivered` (`replayed`, por **(peer, mensagem)**, auditoria médio 1: um envio para vários devices da mesma identidade tem o mesmo `MessageId`).
- **Mensagem em voo no reset** (auditoria médio 2): cifrada na conversa antiga e sem ack quando a conversa é refeita, ela ainda pode ser confirmada por um `b` trocado. `late_ack` guarda esses ids; o ack tardio a recoloca na outbox e **não** emite `Delivered` (a aplicação nunca viu um), e o ack do reenvio emite.
- **Limpeza:** `PeerDisconnected` e `drop_peer` apagam janela, buffer e marcas.
- **Decisão de produto do dono:** reenvio silencioso (nenhum `Delivered` extra, nenhum campo novo no `ConversationReset`; só o comentário do evento mudou).

### Provas (CI, `kin` PR #300)
- Testes primeiro: 7 unitários falhavam pelo motivo certo (run 45076413920).
- Implementação `e28c898`: 422 testes e Lab verdes.
- Mutações (um PR rascunho cada, nomes lidos no log): M1 não recoloca nada (5 testes), M2 teto +1 (`only_the_newest_messages_up_to_the_cap_are_replayed`), M3 a rota da 1ª conexão abre a janela (3 testes), M4 o ack do reenvio emite `Delivered` (1 teste), M5 `PeerDisconnected` não limpa o buffer (1 teste).
- Auditoria (`security-auditor`, só leitura): sem crítico nem alto; os 2 médios e os baixos 3 e 6 foram corrigidos no mesmo PR.

### Limites que ficam (também depois da D)
- **Sem teste de integração pela rede real.** Tentei (2 testes em `two_nodes.rs`, uma por ordem de Peer ID) e **tirei**: a conexão antiga sem ninguém dirigindo engole pedidos alternados, e a mensagem extra desloca em quais conexões os pedidos de controle caem; a conversa não era refeita em 60 s. A janela é provada por eventos injetados (determinístico) e o Lab segue medindo o resto.
- **Ordem:** o reenviado volta no fim da outbox (`seq` novo), atrás do que o usuário mandou entre o `Hello` e o reset. `sent_at_ms` e `parent` permitem à aplicação ordenar. Preservar o `seq` exigiria API nova na `outbox`.
- **Falha do `ready` depois do reenvio:** se o `ready` falhar, a conversa antiga é restaurada e o reenvio sairia por ela (perdido de novo). Só falha de armazenamento.
- A janela `extra_connection` fica aberta pelo resto da conexão (upgrade relay→direta): o custo é copiar até 8 mensagens e ler a outbox do peer a cada ack, o mesmo tipo de custo do `send_direct`.
- `Delivered` repetido depois de reiniciar o processo no meio do reenvio (a marca é em memória).
- Reconexão para um `b` que também mudou de identidade não reenvia (o reset exige a mesma identidade).

## Consequências
- Fecha a janela do P36 sem tocar o fio nem o transporte; mantém a C como caminho para quando houver mais perdas.
- Custa um buffer em memória por peer, mais uma regra no `ready` e uma decisão de produto sobre o `Delivered` duplicado.
- Reverter depois de implementado é fácil (só `chat.rs`); **agora** é grátis (só este ADR).
