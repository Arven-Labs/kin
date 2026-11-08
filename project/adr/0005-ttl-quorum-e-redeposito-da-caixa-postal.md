# ADR 0005 — Caixa postal Nostr: TTL, quórum de réplicas e re-depósito (Fase 3.4)

- **Status**: aceito (Sessão 14, 2026-11-08). Implementado nos PRs #23 (TTL, quórum e janela de leitura) e #24
  (re-depósito). O dono escolheu TTL de 7 dias, quórum de 2, re-depósito em PR separado e confirmou o schema.
- **Relacionados**: D1 em `ARCHITECTURE.md` ("replicação em N relays com TTL"); ADR 0002 (outbox); ADR 0003 (interface de
  mailbox); ADR 0004 (backend Nostr); P19/P21 (gatilhos de reenvio); P23 (limites do `NostrMailbox`).

## Contexto

Depois da 3.3, o `NostrMailbox` publicava o mesmo gift wrap em todos os relays do destinatário, mas a "replicação com
TTL" prevista em D1 só existia no nome:

1. O evento **não tinha expiração**: ficava nos relays para sempre, sem como apagar (a chave de wrap é descartável).
2. O NIP-59 sorteia o `created_at` do gift wrap até 2 dias para trás (`RANGE_RANDOM_TIMESTAMP_TWEAK = 0..172800`, lido
   no crate `nostr` 0.45.7). Com `since_window` de 3 dias, o destinatário só pedia a mensagem enquanto
   `created_at ≥ agora − 3 dias`, ou seja, por cerca de 1 dia no pior sorteio.
3. `Deposited` saía no **primeiro** relay que aceitava, então a réplica era sorte.
4. `kin_deposited` era permanente: se o relay descartasse o evento e o contato nunca aparecesse, a mensagem não era
   depositada de novo.

## Decisão

1. **TTL de 7 dias** (`NostrConfig::ttl`), por expiração NIP-40 no gift wrap. A expiração é ancorada no `created_at`
   sorteado, então se pede `ttl + 2 dias` ao crate (`TIMESTAMP_TWEAK`) para a vida real ser **≥ ttl**. `wrap` continua
   sem TTL; `wrap_with_ttl` é o caminho do backend.
2. **Janela de leitura ≥ `ttl + 2 dias`.** `since_window` padrão = 10 dias (7 + 2 + 1 de folga de relógio).
   `NostrMailbox::with_config` **recusa** configuração com `since_window < ttl + 2 dias` (e com `min_replicas = 0`),
   porque essa combinação perde mensagens em silêncio: o relay ainda tem o evento e o cliente não o pede.
3. **Quórum de 2 réplicas** (`min_replicas`): o depósito só vira `Deposited` com 2 aceites; com menos relays, vale o
   número de relays. Com aceites parciais o resultado é `DepositFailed` ("só N de M relays aceitaram (mínimo Q)"); a
   mensagem segue na outbox e tenta de novo no próximo gatilho, e o aceite parcial já está num relay.
4. **Descarte de vencidos no cliente**: `on_event` ignora gift wrap com `expiration` já vencida (relay sem NIP-40 ou
   hostil que reentrega).
5. **Re-depósito por idade**: o backend declara `Mailbox::retention()` (o `NostrMailbox` devolve o `ttl`; a memória,
   `None`). A cada `Chat::deposit`, o que foi depositado há mais de `retention/2` e segue sem ack volta para a fila de
   depósito (`Outbox::expire_deposits`). A hora fica em `kin_deposit_times (id, at_secs)`, tabela nova e aditiva.
   Depósitos de antes da 3.4 (sem hora, publicados sem expiração) **nunca** são re-depositados.

## Alternativas descartadas

- **TTL de 3 ou 14 dias.** 3 dias perde quem fica offline um fim de semana longo (a outbox ainda reenvia direto, mas o
  relay já esqueceu); 14 dias aumenta a retenção de metadados do destinatário nos relays que honram NIP-40.
- **Confirmar no primeiro aceite (como antes).** Um relay que perca o evento perde a mensagem; a réplica seria sorte.
- **Fixar `since_window` em 3 dias e só adicionar o TTL.** Reproduz o defeito do contexto (2): o cliente não pede o que o
  relay ainda guarda.
- **TTL obrigatório em `wrap`.** Quebraria dezenas de testes que injetam eventos sem expiração, sem ganho.
- **Re-depósito com segunda lista ou `ALTER TABLE` em `kin_deposited`.** Apagar o depósito expirado reaproveita o caminho
  normal (janela `MAILBOX_WINDOW` e ordem, mais antigas primeiro) sem código novo; `ALTER TABLE` mexe numa tabela
  existente e a hora só em memória se perderia ao reiniciar.
- **Re-depositar também os depósitos antigos, uma vez.** Eles não têm expiração no relay; gerariam até 8 gift wraps
  extras por contato sem necessidade.

## Consequências

- Um depósito vive no relay por no mínimo 7 dias (entre 7 e 9, pelo sorteio do `created_at`) em relays com NIP-40; o
  damus e outros sem NIP-40 ignoram a expiração e guardam como quiserem.
- Mais banda por conexão: o destinatário relê uma janela de 10 dias a cada conexão (`since` não é persistido, P23 b). O
  dedupe de 4096 ids pode esquecer ids numa janela com mais eventos que isso; o `Chat` ainda deduplica por `MessageId`.
- `Deposited` demora até o segundo relay mais lento a aceitar (o nos.lol levou 35 a 90 s nas medições da Sessão 11).
- Uma mensagem sem ack custa no máximo cerca de 2 gift wraps por TTL (metadados públicos a mais). Cada re-depósito
  consome gerações do MLS, que tolera 10 fora de ordem: uma cópia antiga que chegue atrasada pode virar `TooOld` e ser
  ignorada em silêncio, mas a cópia nova já entregou o mesmo `MessageId`, que o destinatário deduplica.
- **Sem timer**: a idade só é avaliada nos gatilhos que já chamam `deposit` (envio, conexão, queda, ack). Um processo
  parado sem evento algum não re-deposita (P19/P21).
- Schema: `kin_deposit_times` é aditiva e reversível; apagá-la só desliga o re-depósito.

## Em aberto

- **Roteiro real** da parte B e da expiração no relay: o roteiro da Sessão 13 mostrou que um relay real aceitou e
  devolveu o evento com TTL (2 amostras, uma recusa do `offchain.pub` por web of trust, intermitente), mas ninguém
  consultou o relay para ver a tag `expiration` no evento guardado, e o re-depósito só foi exercitado em testes.
- **Retenção real dos relays**: o NIP-40 foi medido só por 150 s (ADR 0004); o TTL de 7 dias não foi observado.
- **Timer periódico** de reenvio e re-depósito (P19/P21).
- `since` persistido (último evento visto) para não reler 10 dias a cada conexão (P23 b).
