# ADR 0041 — Recuperar a conversa depois de `Stale` (P9 c)

- **Status**: **proposto** (2026-11-21), para o dono confirmar antes de qualquer código. Área de segurança (criptografia, estado de sessão, AGENTS.md §4): vai para o `security-auditor` depois de implementado.
- **Relacionados**: ADR 0001 (ordenação de commits, buffer de época futura), ADR 0040 (re-handshake do P16), P9 (c), P35 e P36 (gate `hello_pending`, conexão antiga).

## Contexto

Quando um lado perde um commit MLS do outro (commit nunca entregue, ou além de `max_past_epochs` = 3, `crates/crypto/src/conversation.rs:18`), tudo o que vem depois chega numa época que ele não alcança. `Chat::hold_future` guarda até `FUTURE_LIMIT` (32) dessas mensagens e, ao estourar, descarta o buffer e emite `ChatEvent::Stale`. Depois disso **nada recupera a conversa**: os dois lados dizem "tenho conversa" no `Hello`, então o P16 não dispara (ADR 0040, "Conversas divergentes sem perda"), e os comentários do `Stale` diziam "recuperar é P16" (circular; corrigido nos docs no PR #280).

## Decisão (dono, 2026-11-21: opção A)

No `Stale`, o **lado atrasado** (o que recebeu a época futura) abandona a conversa e deixa o P16 refazê-la:

1. apaga a conversa e o grupo locais (mesmo padrão da revogação: `conversations.remove` + `Conversation::delete` + `device.forget_conversation`) e limpa os estados por peer como `replace_conversation`; a **outbox não é tocada**;
2. pede ao transporte para **fechar a conexão** com o peer (API nova `Node::disconnect`, sobre `Swarm::disconnect_peer_id`);
3. na reconexão, o `Hello` sai com "não tenho conversa"; o outro lado, que tinha a conversa na abertura da conexão (`hello_pending`, P35), marca `lost_state`, exige a **identidade da conversa antiga** e refaz com `ConversationReset`. Nada disso muda.

**Limite**: no máximo **uma recuperação automática por peer por janela** (proposta: 10 min, só em memória). Dentro da janela o `Stale` só avisa, como hoje. **Sem formato de fio novo e sem schema novo.**

## Alternativas descartadas

- **Mensagem `Reset` no fio (B)**: muda o formato (§4); o ADR 0040 já descartou uma versão; qualquer mensagem de época futura do par poderia forçá-la.
- **Só `/reset` manual pelo dono (C)**: sem risco de apagar sozinho, mas a conversa fica parada até o dono agir (o custo que o ADR 0040 recusou para o P16).
- **Reenviar o `Hello` na mesma conexão**: o gate do P35 só marca `lost_state` com o `Hello` da abertura da conexão; relaxá-lo é a alternativa que o PR #267 mostrou regredir 8 cenários do Lab. Fechar a conexão explicitamente também evita a janela do P36 (a conexão antiga não fica meio-aberta).

## Consequências e riscos

- **Perda de dado**: o buffer descartado e o que estava em voo se perdem; a outbox segue e sai pela conversa nova.
- **Quem pode forçar o `Stale`** (por leitura, **não medido**): `UnknownEpoch` vem de `ValidationError::WrongEpoch` do openmls (`conversation.rs:442`), levantado na validação do enquadramento, **antes de decifrar e de autenticar o remetente**. Então bastam 32 mensagens MLS bem formadas, com o `group id` certo e uma época futura, para disparar o `Stale`, sem chave nenhuma. Pela conexão direta isso só o próprio par consegue (Noise autentica o Peer ID, e `on_mls` é por peer). **Não li** se o caminho da caixa postal (`MailboxEvent::Retrieved`, `chat.rs:1641`) deixa um terceiro injetar com o `from` de outro; se deixar, o risco é maior e muda a decisão.
- **O que o par ganha** ao forçar: este lado refaz a conversa, mas o P16 só aceita a **mesma identidade** da conversa antiga (e revogação/regime v4 conferidos), então não há troca de interlocutor; o custo é a mensagem em voo. A janela de 1 por peer limita a repetição.
- **Device pareado** não se recupera de um banco perdido (ADR 0040, não emite certificado). **Decisão do dono (2026-11-21)**: no `Stale` ele **só avisa, como hoje** (não apaga a conversa), até se confirmar no código que o `Hello(false)` dele é aceito pelo outro lado. Apagar sem poder refazer o deixaria sem conversa.
- **Caixa postal**: **decisão do dono (2026-11-21)**: verificar na etapa de testes (ler `MailboxEvent::Retrieved`, `chat.rs:1641`, e como o `from` é autenticado) antes de implementar. Se um terceiro puder injetar, a recuperação automática vale só para `Stale` vindo de mensagens da **conexão direta**; o `Stale` por caixa postal só avisa.
- **Interação com o P36**: depende de a reconexão ocorrer; se o outro lado tiver a conexão antiga meio-aberta, vale o que já está registrado no P36.

## Não verificado

Nada foi rodado. O comportamento do `disconnect_peer_id` (fecha todas as conexões? emite `PeerDisconnected` em ambos os lados?), o caminho da caixa postal e o caso do device pareado ficam para a etapa de testes.
