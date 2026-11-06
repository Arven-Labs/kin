# ADR 0003 — Interface de mailbox (caixa postal de terceiros) e backend em memória (Fase 3.1)

- **Status**: aceito (Sessão 7, 2026-11-05).
- **Relacionados**: D1 em `ARCHITECTURE.md`; ADR 0001 (P9) e ADR 0002 (outbox); P15, P19, P20 e P21 em `PENDING.md`.

## Contexto

A outbox (ADR 0002) só entrega se o destinatário voltar a conectar com o remetente. D1 prevê, em seguida,
depositar a mensagem **já cifrada** numa caixa postal de terceiros (relays Nostr na 3.3, peers na Fase 4) para o
destinatário buscar sem o remetente estar online. A 3.1 define a interface que deixa esse backend trocável.
O `Chat` usa o `Node` para muito mais que mensagens (NAT, relays, hole punching), então o envio direto
(libp2p) não cabe numa interface de entrega e fica fora dela.

## Decisão

1. **Interface mínima, orientada a eventos.** `Mailbox::attach(events)` entrega ao backend um canal
   (`futures::channel::mpsc`) e `Mailbox::deposit(to: PeerId, blob) -> DepositId` não bloqueia. O backend
   responde pelo canal com `Deposited`, `DepositFailed` e `Retrieved { from, blob }`. O `Chat` escuta o canal
   no mesmo `next_event` que escuta a rede. Um backend Nostr (baseado em tarefas/websocket) só precisa empurrar
   eventos; o de memória responde na hora.
2. **Só depositar com o peer offline.** Com conexão, vale o envio direto. A mensagem **continua na outbox**
   depois de depositada: só o ack do envio direto a tira, e o receptor deduplica pelo `MessageId` (ADR 0002).
   `Deposited` significa "a caixa postal aceitou", não "entregue".
3. **Teto de 8 mensagens depositadas e não confirmadas por peer** (`MAILBOX_WINDOW`), somando as já aceitas e
   as pedidas e sem resposta. Sem ack do destinatário, o backend pode devolver as mensagens fora de ordem, e o
   MLS só tolera 10 gerações fora de ordem (`TooOld` é ignorado em silêncio).
4. **Só mensagem MLS vem pela caixa postal.** O handshake (`Hello`, `KeyPackage`, `Welcome`) exige o Peer ID da
   conexão (P15); pela caixa postal o `from` seria apenas o que o backend afirma. Qualquer outro envelope vira
   `Dropped`. A identidade real de quem enviou continua sendo a credencial MLS na decifragem.
5. **A marca "depositada" é persistente** (`kin_deposited`) e só é gravada quando o backend confirma, para um
   reinício entre o pedido e a confirmação não perder o depósito nem repeti-lo.
6. **Backend de referência público** (`MemoryRelay` + `MemoryMailbox`): serve aos testes dentro de `chat.rs` e
   de modelo para a 3.3. Isto altera o que o ADR 0002 previa (backend só em `tests/common`).

## Alternativas descartadas

- **`async fn next_event` no trait.** Não é compatível com `dyn` sem boxing, e obriga o backend a ser polado
  pelo `Chat` em vez de empurrar eventos.
- **Depositar sempre, em paralelo ao envio direto.** Vaza mais metadados a terceiros sem ganho.
- **Endereço do destinatário opaco no trait.** O mapeamento Peer ID → endereço do backend (no Nostr, uma chave
  secp256k1) fica dentro do backend, e a interface continua em `PeerId`.

## Consequências

- Mensagem escrita com o destinatário offline chega por ele buscar na caixa postal, sem duplicata se depois
  chegar também direto.
- O remetente não sabe se o destinatário já buscou: se ele buscou, o reenvio direto ao reconectar é um
  desperdício correto (a deduplicação o descarta). Recibo de entrega do destinatário fica fora de escopo.
- O backend vê o Peer ID do destinatário, o tamanho e o horário (metadados já aceitos no D1).

## Em aberto

- **P21**: ao ligar o mailbox, mensagens que já estavam na fila só são depositadas no próximo envio ou queda do
  peer (não há varredura inicial). Mesma família do P19 (gatilhos de reenvio); um depósito recusado também só é
  repetido no próximo gatilho.
- Backend assíncrono real: cancel-safety, ordem de entrega e starvation entre rede e mailbox só serão
  exercitados na 3.3. O laço lê o canal sem bloquear antes de esperar a rede, mas isso não foi medido sob carga.
- Como o backend Nostr descobre a chave do destinatário (provável extensão do certificado do device).
- Réplicas e TTL (3.4) e multi-device (3.5).
