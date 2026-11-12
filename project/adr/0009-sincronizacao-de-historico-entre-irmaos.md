# ADR 0009 — Sincronização do histórico entre irmãos (P10a, fatia 2)

- **Status**: aceito (Sessão 20, 2026-11-11). Implementado no PR #92 (fatia 2 de 3). A fatia 3 (CLI `/history` e cenário
  `history-sync` no laboratório) está no PR #102 (Sessão 21); o ADR não muda, só passa a ter consumidor na CLI.
- **Relacionados**: ADR 0008 (o motor guarda o histórico, fatia 1); ADR 0006 (multi-device: pares device↔device, espelho `Sent`,
  autoria pelo `sender` do MLS); P10(a), P26 e P27 em `PENDING.md`.

## Contexto

A fatia 1 fez o motor guardar o histórico por identidade de contato. Um device novo (mesma identidade, banco vazio) precisa
receber o que o irmão já tem. O MLS não dá histórico a quem entra depois, mas o 3.5 usa **um par MLS por device↔device**: dá para
o irmão antigo mandar o histórico dentro da conversa cifrada que os dois já têm. Esta fatia lê dado vindo da rede (de um irmão),
então formato estrito, tetos e autorização são parte da decisão.

## Decisão

1. **Pull simétrico entre irmãos, dentro do MLS do par.** Dois payloads novos:
   `HistoryRequest` (tag 5: `5 | cursor(8)`) e `HistoryChunk` (tag 6:
   `6 | done(1) | next(8) | n(2) | n × [len16 | contato | saiu(1) | len16 | Message]`). Cada device pede ao irmão ao conectar,
   então os históricos convergem para a união (a primeira gravação de um `(contato, id)` vence).
2. **O cursor é do respondente.** `seq` é local de cada banco; o pedinte devolve o `next` que o irmão mandou. O pedinte **persiste
   `next` por irmão** (`kin_history_cursor`, **schema v7**, aditivo, sem downgrade; decisão do dono), então a reconexão só traz o
   novo. Cursor maior que o maior `seq` do respondente vale 0 (banco recriado), e o `next` que volta pode ser menor que o cursor
   salvo: isso é aceito.
3. **Quando pedir**: uma vez por conexão, a irmão, no mesmo ponto em que se anuncia a lista de devices (só depois de o outro lado
   ter a conversa: o convidador do MLS só anuncia ao receber algo). Zera ao desconectar.
4. **Só irmão.** Pedido e página valem se o `sender` autenticado pelo MLS é da **minha identidade** (a lição do ADR 0006), senão
   `UnauthorizedHistory`. Uma página só entra se **eu a pedi** (sessão por irmão): sem pedido, `UnauthorizedHistory`.
5. **Tetos.** Página de até 256 itens e ~32 KiB de texto claro (`HISTORY_CHUNK_BUDGET`); item que sozinho não cabe é pulado e o
   cursor passa por ele; leitura estrita (flags só 0/1, n ≤ 256 antes de alocar, sem byte sobrando, UTF-8 e chave do contato
   conferidos). Uma página **não final sem itens** encerra a sessão, e passar de 100 000 linhas por sessão
   (`MAX_SYNC_ENTRIES`) também (`HistoryLimit`): é o que garante a terminação com um irmão comprometido.
6. **Ordem de gravação**: as linhas entram por `record_history` e o cursor só é salvo **depois**, para uma queda no meio repetir em
   vez de perder linhas.
7. **Evento** `ChatEvent::HistorySynced { peer, imported, done }` a cada página (só linhas novas contam); a CLI ganha uma linha
   informativa. O `/history` fica para a fatia 3.

## Alternativas descartadas

- **Push do irmão antigo.** Ele teria de saber quem é "novo"; o pull é simétrico e idempotente.
- **Exigir que o `next` sempre avance.** Foi a primeira ideia (anti-laço) e conflita com o banco recriado do irmão (o `next` volta
  a ser menor que o cursor salvo). O laço termina pelas duas regras do item 5, sem essa checagem.
- **Cursor só em memória.** Sem schema novo, mas reenvia o histórico inteiro a cada reconexão.
- **Sincronizar também com contatos.** Fora de escopo: exigiria consentimento e um formato que não vaze conversas de terceiros.

## Consequências

- **Banda e leitura**: cada reconexão de irmão custa uma leitura de página por pedido (um irmão não é limitado por taxa) e, na
  primeira vez, o histórico inteiro em páginas de ~32 KiB.
- **Sem downgrade** (schema v7), como nas v3 a v6.
- **Pedido perdido**: se o envio do `HistoryRequest` falhar com a conexão aberta, a sessão fica sem resposta até reconectar
  (o pedido é de controle e não vai à outbox). Ver P27.
- **A checagem de `sender` na página é defesa em profundidade**: para um contato a checagem de sessão já recusa (só há sessão
  para irmãos). A mutação que a remove **sobrevive** aos testes; só um grupo MLS com membro extra a distinguiria, e esse caso é
  barrado antes no `crypto` (limite de 2 membros, ADR 0006).
- "Notas para mim" continuam relativas ao device (primeira gravação vence), como no ADR 0008.
