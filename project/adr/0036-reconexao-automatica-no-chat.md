# ADR 0036 — Reconexão automática no `chat`, só em memória e só com mensagem na fila (P32)

- **Status**: **aceito e implementado** (Sessão 31, 2026-11-18, PR #231). Toca o filtro de endereços de terceiros (segurança, AGENTS.md §4): auditado pelo `security-auditor`
  (0 críticos, 6 importantes, 7 sugestões).
- **Relacionados**: ADR 0002 (outbox), ADR 0003 (caixa postal), P19 e P21 (mesmo relógio), P25 (teto de conexões por peer), P32 em `PENDING.md`.

## Contexto

Quando a conexão com um peer cai, nada rediscaa: o `chat` só deposita a fila na caixa postal (P21). Quem traz o peer de volta é o mDNS (só LAN) ou um humano. Na WAN, com NAT, o peer
fica fora até alguém agir, e a mensagem só sai pelo caminho direto depois disso. O P19 e o P21 resolvem "mandar enquanto o peer está fora"; faltava **trazer o peer de volta**.

## Decisão (dono, 2026-11-18)

1. **No `chat`, não no `Node`**: só o `chat` sabe quem tem conversa e fila. Reusa o relógio do P19/P21 (`wake_in`, `Next::Tick`), sem timer nem tarefa nova.
2. **Só com mensagem na fila** (`redial_want`): sem fila, só a caixa postal e o mDNS trazem o peer de volta. Poupa bateria e não vaza tentativas de IP a quem não tem o que receber.
3. **Só memória**: sem schema novo (nada de v8). Depois de reiniciar o processo o peer não tem endereço até novo convite, `identify` ou mDNS.
4. **Endereços** = os do `identify` (filtrados, mesclados, até 4; irmão até 16), os do convite aceito (`accept`) e os do handshake esperado. O filtro `dialable` virou **allowlist** de IPv6
   unicast global (`2000::/3`, sem 6to4 nem Teredo) e nega as faixas especiais do IPv4.
5. **Estados** `Waiting` → `InFlight` (60 s) → `Waiting` ou `Exhausted`. Espera de 2 s dobrando até 5 min, com jitter de 20%; orçamento de 100 tentativas (~8 h). Dial sem notícia no prazo conta
   como falha; sem endereço reagenda sem discar; mensagem nova rearma **uma** tentativa depois de esgotar. Conectar, esquecer o peer, fila vazia e `untrust` apagam o registro.
6. **Sem `ChatEvent::DialFailed` por tentativa de fundo** (um por tentativa seriam ~100 por peer e episódio, com endereços escolhidos pelo contato no texto).

## Alternativas descartadas

- **Backoff dentro do transporte**: o `Node` não conhece a outbox, e a política ficaria duplicada.
- **Rediscar sempre, para todo peer com conversa**: mantém a conexão viva, mas custa bateria, tráfego e dial constante a endereços possivelmente velhos. É o que a Fase 5 (presença) vai precisar,
  não este passo.
- **N tentativas fixas e desiste**: um peer que volta horas depois só reconecta por outro gatilho.
- **Persistir endereços (schema v8)**: migração de banco (§4), sem downgrade, e guarda IP de contato em claro em disco (P13/P26b). Fica como próximo passo possível.
- **Lista de negação no `dialable`**: já deixava passar `::ffff:10.0.0.1` (IPv4 privado embrulhado em IPv6); a allowlist fecha a classe.
- **Desempate por Peer ID (só o menor rediscaria)**: um peer atrás de NAT não é alcançável de um lado só, então os dois rediscam. Registrado como risco, não implementado (YAGNI).

## Consequências

- Rediscar **não** gasta geração MLS (o dial só abre transporte; é o `encrypt` que avança o ratchet), ao contrário do P19/P21. O orçamento protege rede e bateria, não criptografia.
- O envio offline passou a agendar uma rediscagem de 2 s: quem checa `wake_in` num teste com peer desconectado precisa contar com ela.
- Um contato escolhe até 4 endereços globais de terceiros por dial (~1 lote por 1,6 s, sem amplificação em bytes). Risco aceito.
- Em aberto (`PENDING.md`, P32 a a j): reiniciar sem endereço, dials simultâneos dos dois lados (`max_established_per_peer = 4`), avalanche global após perder a rede, endereços de documentação
  ainda passam no filtro, cenário de lab, `V6ONLY` do transporte.
