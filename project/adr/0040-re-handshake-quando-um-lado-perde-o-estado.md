# ADR 0040 — Re-handshake quando um lado perde o estado da conversa (P16)

- **Status**: **aceito e implementado** (Sessão 36, 2026-11-19, PR #254). Área de segurança (criptografia, estado de sessão, formato de fio, AGENTS.md §4): auditado pelo `security-auditor` (0 críticos, 0 altos, 3 médios, 7 baixos; tabela no fim).
- **Relacionados**: ADR 0039 (admissão e `Hello` com certificado), ADR 0001 e 0002 (commits e outbox), P9 (épocas divergentes, fora daqui).

## Contexto

Se um lado (`b`) perdia o banco (mesma identidade e device, banco novo) e o outro (`a`) mantinha a conversa, o handshake nunca se refazia: `on_hello`, `on_key_package` e `on_welcome` retornam cedo quando já há conversa. O `a` seguia enviando; o transporte confirmava (`Delivered`); o `b` descartava (`Dropped`); e o `a` reanunciava uma conversa que o `b` não tinha.

## Decisão (dono, 2026-11-19)

1. **Refazer sozinho e avisar** (não pedir confirmação): o `a` troca a conversa e emite `ChatEvent::ConversationReset { peer, identity }` antes do `ConversationReady`. A CLI imprime a **identidade**, diz que o sinal não é verificável e pede para confirmar por outro canal.
2. **Detecção pelo `Hello`**: nova tag 5 com campo de bits (1 token, 2 "tenho conversa com você", 4 certificado). O encoder só emite a tag 5; as tags 0 e 4 ainda são lidas e **nunca** disparam reset (o bit é `None`). Motivo da tag nova: o `b` que perdeu o banco não conhece o `a` e manda `Hello` sem certificado (ADR 0039, privacidade), que não tem campo livre.
3. **A fila espera o `Hello`** (auditoria, médio 1): com conversa, a fila do peer só sai depois do `Hello` dele (`hello_pending`, marcado em `PeerConnected`). Sem isso o `a` despejava a fila no grupo antigo antes de ver o bit, com `Delivered` falso.

## Como funciona

- `on_hello` com bit 2 = `false`, eu tendo conversa **1:1** com o peer **e a conversa já existindo quando a conexão abriu** (o peer está em `hello_pending`; P35) → `lost_state` (em memória, teto 64, **um reset por conexão**: `reset_done`). Só marca; nada é apagado.
- Com `lost_state`, `on_key_package` (eu, de menor Peer ID) e `on_welcome` (eu, de maior) deixam de ignorar por "já há conversa". Criam ou entram numa conversa **nova** exigindo a **identidade da conversa antiga** e o Peer ID (`credential::verify`, que também confere revogação e regime v4).
- Só depois de verificada a nova, `replace_conversation` a põe no lugar: limpa os estados por peer (anúncios, sessões de histórico, época futura), avisa, e **só no fim** apaga o grupo antigo. Se o `ready` falhar, a conversa antiga volta. A outbox não é tocada: o que estava na fila sai pela conversa nova.
- Uma recusa (identidade diferente, revogado) consome o `lost_state` e deixa a conversa antiga intacta.
- `KeyPackage` que chega antes do `Hello` é guardado (`held_key_packages`, agora também com conversa existente) e reprocessado quando o `Hello` chega.

## Alternativas descartadas

- **Pedir confirmação (`/reset`)**: mais seguro contra chave roubada, mas a conversa fica parada e a mensagem do meio se perde do mesmo jeito.
- **Mensagem `Reset` do `b` ao falhar em decifrar**: só detecta depois do primeiro envio, já com `Delivered`.
- **Apagar a conversa antiga ao ver o bit**: o bit é só uma alegação do peer; qualquer erro custaria o estado.
- **Recolocar na fila o que ficou `Delivered` na conexão** (alternativa ao médio 1): mais código, e pode reenviar o que o `b` recebeu.

## Consequências e limites

- **Segurança, o que muda** (auditoria, baixo 1): quem tem só o `device.key` (sem o banco) **não** ganha nada: o `KeyPackage` precisa da chave MLS de assinatura coberta pelo certificado, que mora no `mls.sqlite`. Quem tem a **chave mestra** (ou a autoridade do primário) mais o `device.key`, sem o banco, passa a assumir sozinho as conversas 1:1 existentes com esse Peer ID, o que antes não conseguia. Já conseguia se passar pela identidade para contatos novos. É o custo da decisão de refazer sozinho; o contrapeso é o aviso com a identidade.
- **Device pareado não se recupera de um banco perdido** (não emite certificado): o P16 só vale para mestra e primário.
- **Mensagens perdidas**: o que o `a` enviou e deu `Delivered` **antes** de ver o `Hello` não existe mais (com a espera do `Hello`, isso só ocorre se o `b` perdeu o banco **com a conexão aberta**).
- **Conversas divergentes sem perda** (backup restaurado, época antiga): os dois dizem `true`, nada dispara. Fica para o P9.
- **Compatibilidade**: nó de build anterior recusa a tag 5 como `Malformed`; o handshake com ele não fecha (mesmo custo da tag 4; pré-lançamento).
- A fila de um peer com conversa espera o `Hello` dele a cada reconexão (pequeno atraso); um peer que nunca manda `Hello` fica sem fila direta (a caixa postal segue).
- O `lost_state` é consumido por qualquer recusa, inclusive erro transitório de leitura; a cura vem na próxima conexão (auditoria, baixo 5).
- **Adendo P35 (2026-11-20, decisão do dono)**: o `Hello` "não tenho conversa" só vale com a conversa já existente na abertura da conexão. Motivo: os substreams não têm ordem, e um `Hello` montado antes de a conversa existir chegava depois dela, marcava `lost_state` num peer que não perdeu nada e travava a fila dele até a conexão cair (o `revoke-device` do lab falhou 5 vezes em 9). **Custo aceito**: o transporte só emite `PeerConnected` na 1ª conexão (`node.rs:487`). Se o `b` perde o banco e reconecta enquanto o `a` ainda tem a conexão antiga de pé (queda sem FIN, troca de NAT), a nova conexão é só `PeerRoute`, não há `hello_pending`, e o `Hello(false)` do `b` é ignorado: o `a` segue no grupo antigo, com `Delivered` falso, até a conexão antiga cair (keepalive/idle). **Medido depois (PR #264)**: a janela existe e tem duas partes. Com o `a` de maior Peer ID ela é **efeito deste adendo** (sem o gate o `a` refaz a conversa); com o `a` de menor Peer ID ela é **anterior** ao P35 (o `a` não manda `Hello` na 2ª conexão, então o `b` novo nunca manda o `KeyPackage`). Não medi a duração da janela nem a frequência. Alternativas descartadas: `hello_pending` também em 2ª conexão (pode reabrir o P35) e conjunto "conversas formadas nesta conexão" (estado novo, refaz testes e lab). Registrado como P36.

## Provas

- Testes primeiro (CI): os 2 testes novos falharam (`peer_that_lost_its_database_is_rehandshaked`, nas duas ordens de Peer ID, e o unitário da identidade); o guarda `a_fresh_or_surviving_conversation_is_never_reported_as_reset` passou. Depois das correções da auditoria, os 3 novos testes falharam antes (fila, segundo reset, aviso) e passaram depois (CI verde, 20 checks, commit `3573c24`).
- Mutação de propósito (PRs descartáveis #255 e #256, fechados): a identidade da conversa antiga deixando de ser exigida derrubou 2 testes (a 1ª tentativa nem compilou, e foi refeita); o reset sem exigir `lost_state` derrubou o teste unitário.

## Auditoria (`security-auditor`, somente leitura)

| # | Achado | Gravidade | Destino |
|---|---|---|---|
| M1 | Fila despejada no grupo antigo antes do `Hello`, com `Delivered` falso | médio | **corrigido**: `hello_pending` segura a fila (decisão do dono) |
| M2 | Reset repetível sem limite (ciclo de custo e ruído) | médio | **corrigido**: um reset por conexão (`reset_done`) |
| M3 | Aviso da CLI fraco (sem identidade, afirma como fato) | médio | **corrigido**: `reset_notice` com identidade e pedido de confirmação |
| B1 | Quem tem as chaves de identidade assume as conversas 1:1 | baixo | **aceito**, registrado acima |
| B2 | `Hello(false)` obsoleto pode marcar `lost_state` de peer honesto | baixo | **reproduzido no lab (P35) e corrigido**: só vale com a conversa já existente na conexão; custo no adendo acima (P36) |
| B3 | Troca não atômica; grupo antigo órfão se o `delete` falhar | baixo | **corrigido em parte**: a antiga volta se o `ready` falhar; `delete` que falha vira `Dropped` |
| B4 | `Welcome` com o mesmo `group_id` do antigo | baixo | **não corrigido** (depende do openmls, só prejudica a própria conversa do atacante) |
| B5 | Suspeita consumida em falha transitória; bit só de existência | baixo | **aceito**, registrado |
| B6 | `held_key_packages` guarda `KeyPackage` com conversa (≈1 MiB no teto) | baixo | **aceito** |
| B7 | Compatibilidade (tag 5); `handshake_retries` não zera na troca | baixo | compatibilidade **aceita**; contagem não zerada, não corrigida |
