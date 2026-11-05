# ADR 0001 — Ordenação de commits MLS sem Delivery Service (P9)

- **Status**: aceito (Sessão 5, 2026-11-05) para 1:1. Multi-device e grupos seguem abertos.
- **Relacionados**: D1 e D2 em `ARCHITECTURE.md`; P9, P10 e P16 em `PENDING.md`.

## Contexto

O MLS (RFC 9420) pressupõe um Delivery Service que dá uma ordem total aos commits. O Kin não tem
servidor, e a mailbox da Fase 3 (D1) entrega at-least-once, sem ordem e com atraso. Dois problemas:

1. **Mensagem antes do commit.** Uma mensagem da época N+1 pode chegar antes do commit N→N+1. O `crypto`
   já a reporta como `UnknownEpoch`, mas o `chat` a descartava (`Dropped`).
2. **Commits concorrentes (fork).** Dois membros que commitam a partir da mesma época N criam dois "N+1"
   incompatíveis. O MLS não resolve isso sozinho; as chaves divergem e a conversa quebra.

## Decisão

1. **Committer designado.** Só o membro de menor índice de folha emite commits (`Conversation::is_committer`).
   Em 1:1 é quem criou a conversa, que também é o de menor Peer ID (mesma regra do `on_hello`). A regra é
   determinística e igual em todos os devices, então não exige combinar nada pela rede. `rotate_keys` e
   `commit_pending` retornam `NotCommitter` para os demais.
2. **Os demais propõem.** Um não-committer renova a própria chave com `propose_rotation` (Update proposal do
   MLS); o committer guarda a proposta e a efetiva com `commit_pending`. Só propostas `Update` são aceitas
   de pares: `Add`/`Remove` soltos continuam `Unsupported`, para um par não escolher quem entra ou sai.
3. **Buffer de época futura no `chat`.** Mensagem `UnknownEpoch` é guardada por peer (teto de 32, sem
   repetidas) e reprocessada, na ordem de chegada, a cada commit aplicado. Estourar o teto emite
   `ChatEvent::Stale` e descarta o buffer: o commit que falta provavelmente não virá. O buffer fica só em
   memória.

## Alternativas descartadas

- **Fork resolvido por regra + rollback** (vence o menor hash; o perdedor descarta o commit pendente). Com
  mailbox a detecção é tardia, e as mensagens enviadas na época perdedora ficam ilegíveis.
- **Aceitar o fork e curar por re-entrada** (External Commit ou novo Welcome). Perde chaves e depende de
  autenticar quem pode forçar o reset; fica para P16, não como mecanismo principal.
- **Buffer dentro do `crypto`.** Descartado para o `crypto` continuar puro e síncrono.

## Consequências

- Em 1:1 não há commits concorrentes por construção.
- Se o committer ficar offline, mudanças de chave dos demais esperam a volta dele (a conversa de mensagens
  continua funcionando; só a rotação adiada reduz a recuperação pós-comprometimento).
- O `Stale` é o único sinal de divergência: recuperar a conversa é **P16** (aberto).
- Um par pode forçar `Stale` enchendo o buffer. Em 1:1 o par é de identidade verificada; **em grupos
  precisa de revisão**.
- O teto de 32 foi um chute; medir com a mailbox real (Fase 3).

## O que continua aberto (P9)

- **Multi-device (3.5):** vários devices da mesma identidade na mesma conversa. A regra "menor índice de
  folha" continua válida, mas o que acontece quando o committer é um device que sumiu não está decidido.
- **Grupos (Fase 5) e P10:** troca de committer quando ele sai ou fica offline por muito tempo; proposals
  `Add`/`Remove` (hoje recusadas) e quem tem permissão de pedi-las.
- **Commit perdido além das épocas guardadas** (`max_past_epochs = 3`): fica no P16.
