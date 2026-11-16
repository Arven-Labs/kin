# ADR 0029 — A caixa postal esquece o peer revogado e o desenho do `chat` que o derruba (P30 fatia 3d-ii-b)

- **Status**: b1 **aceito e implementado** (Sessão 26, 2026-11-15); b2 **desenho aceito, ainda não implementado**. Área de segurança
  (revogação de acesso, AGENTS.md §4): a b1 foi auditada pelo `security-auditor` (0 críticos, 3 importantes; tabela no fim) e tem 5
  mutações (5 pegas). A **3d-ii-b2** e a **3d-ii-c** são **bloqueadoras do P30**. Nada chama `forget_peer` ainda.
- **Relacionados**: ADR 0004 (backend Nostr), 0025, 0027, 0028, P30, P31 (am).
- **Nota de numeração**: o código da 3c-ii-b cita um "ADR 0026" que nunca foi escrito; este é o 0029.

## Contexto

A 3d-i recusa device revogado na entrada e a 3d-ii-a deu ao `crypto` o que o `chat` precisa. O `chat` ainda não derruba quem já está na
conversa. O corte do dono: **b1 (caixa postal)** e **b2 (chat)**, em PRs separados, e **c (reabertura)** depois.

## Decisão — b1 (feita)

1. **`Mailbox::forget_peer(&mut self, peer: PeerId)`**, com **default vazio** no trait (como `learn_peer` e `learn_relays`: quem não guarda
   estado por peer, como o `MemoryMailbox`, ignora; nenhuma implementação existente quebra). Idempotente; peer desconhecido não é erro.
2. **`NostrMailbox::forget_peer`** remove, sob o mesmo `lock()`: `peers[peer]` (endereçar), `peer_relays[peer]` e a reversa `by_nostr` **por
   valor** (`retain(|_, owner| *owner != peer)`). Por valor, e não pela chave atual (auditoria I1): `learn_peer` nunca apaga a chave antiga, então
   um peer que mudou de chave Nostr deixa uma entrada órfã apontando para ele. Uma chave que outro peer reaprendeu aponta para o outro e fica.

## Decisão — b2 (desenho; plano detalhado e testes na própria fatia)

Um método único `Chat::drop_revoked_peers()`: para cada conversa, para cada certificado de `peer_certificates()`, `device.is_revoked(&cert)?` derruba.
Roda em `on_revocation` **depois** do `store_revocation` (em `Ok(true)` **e** em `Ok(false)`, para uma falha transitória do disco não deixar o par
vivo) e **antes** do repasse; a 3d-ii-c o reaproveita na reabertura.

Derrubar um peer, **nesta ordem** e em melhor esforço (cada erro vira `Dropped`; tudo idempotente):
1. Tirar de `conversations`, `Conversation::delete()` (grupo) e `forget_conversation` (label) (ADR 0028).
2. Limpar a memória por Peer ID (`expected`, `future`, `handshake_retries`, `announced`, `devices_announced`, `lists_acted`, `last_sibling_announce`,
   `revocations_checked`, `sibling_pending`, `history_*`, `in_flight`, `depositing`, `household`, `contact_devices`). `connected` fica (o `Node` não
   desconecta); `trusted`/`distrusted` e `kin_history` ficam (são por identidade). `pending` sai por `MessageId`; um `Welcome` em voo só é reenviado
   se a conversa existe.
3. `Outbox::forget_peer` (uma transação nas 7 tabelas por peer) **antes** de `mailbox.forget_peer`.
4. `ChatEvent::PeerRevoked { peer, identity }` uma vez e um `SendFailed { peer, message: Some(id), .. }` por mensagem apagada (entradas espelho não geram
   evento); braço novo no `match` exaustivo da CLI.
5. `on_key_package`: se `invite` falhar, `conversation.delete()` antes de devolver o erro (fecha o P31 (am); cobre todos os erros do `invite`).

**Requisitos que a auditoria da b1 deixou para a b2:**
- **Ordem**: remover a conversa, apagar `outbox.peer_relays` do peer e **só então** `mailbox.forget_peer`; senão o `set_mailbox` (que reensina tudo a partir das
  conversas e de `outbox.peer_relays`, persistido) repovoa o que foi esquecido.
- `teach_mailbox` e `on_relay_list` **ignoram peer derrubado**: uma lista de relays que chegue depois recriaria `peer_relays[peer]`.
- Um `Retrieved` que já estava no canal quando o `forget_peer` rodou chega depois; o `Chat` já devolve `Dropped(NoConversation)`, e a b2 ganha um teste disso.

## Alternativas descartadas

- **Método obrigatório no trait**: quebraria quem implementa o trait fora do repo; o default vazio é coerente com `learn_peer`.
- **Guarda `by_nostr.get(&chave) == Some(&peer)`** (a primeira versão): deixava a chave antiga órfã (auditoria I1). Trocada por `retain` por valor.
- **Cancelar depósitos já em voo** para o peer esquecido: fora do escopo; `ack` trata `id` ausente.

## Consequências

- Se dois peers aprendem a mesma chave Nostr, `by_nostr` aponta para o último. Esquecer o último deixa o outro sem atribuição de entrada: falha fechada,
  e raro (uma chave Nostr nasce de uma `DeviceKey`).
- O relay não filtra por autor (`relay.rs`, filtro `kind GiftWrap` + `#p` próprio), então não há filtro por chave para limpar. O relay continua entregando
  eventos da chave esquecida; `on_event` os descarta sem marcá-los como vistos, ao custo de decifrar um evento por reconexão (já existia para estranhos).

## Lacunas conhecidas

- Sem teste de depósito em voo seguido de `forget_peer`.
- O teste "evento de chave esquecida" é negativo (`quiet`, 600 ms); o da chave compartilhada cobre o mesmo caminho com controle positivo.
- Um `forget_peer` que chegue depois da decifragem e antes do segundo `lock()` descarta o evento (desejado); se chegar depois, o `Retrieved` já está no canal.

## Verificação (resultado real)

| Etapa | Run | Resultado |
|---|---|---|
| Testes com esqueleto | 43784070943 | 3 de 4 falharam pela asserção; o 4º (caracterização) passou |
| Implementação | 43784953079 | verde |
| Teste da chave compartilhada | 43785321081 | verde |
| Teste da chave rotacionada com a guarda antiga | 43786379958 | 1 de 1 falhou (a chave órfã existia) |
| Depois da auditoria (`retain` por valor) | 43786692119 | verde |

**Mutações** (branches descartáveis, 5 de 5 pegas): não remove `by_nostr` (43785348315, 1 teste); não remove `peers` (43785354949, 1); não remove
`peer_relays` (43785361916, 1); remove `by_nostr` sem conferir o dono (43785368645, 1); `retain` apagando todas as reversas, sobre o código pós-auditoria
(43787028107, 1).

**Auditoria** (`security-auditor`, somente leitura): sem críticos. I1 (chave rotacionada) corrigido; I2 (corrida com `on_event`) e I3 (repovoamento pelo
`Chat`) viraram os requisitos da b2 acima. Sugestões: documentar o caso da chave compartilhada (feito) e o default vazio (coerente). O auditor executou
`git checkout` no repo, contra o "somente leitura"; a árvore e a branch estavam íntegras, e voltei à branch de trabalho.
