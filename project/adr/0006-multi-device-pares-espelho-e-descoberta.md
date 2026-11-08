# ADR 0006 — Multi-device: pares device↔device, espelho e descoberta (Fase 3.5)

- **Status**: parcialmente aceito (Sessão 15, 2026-11-08). Modelo, espelho e limite de 2 membros aceitos e na `main`
  (PRs #26 a #30). A descoberta de devices (PR #31) está aberta e **não deve ser mesclada** antes das três decisões de
  produto em "Em aberto". O pareamento (etapa 6) não foi feito.
- **Relacionados**: ADR 0001 (committer designado); ADR 0002 (outbox); ADR 0005 (re-depósito); D2, D5 e D7 em
  `ARCHITECTURE.md`; P9(a), P11, P12 e P24 em `PENDING.md`.

## Contexto

Até a 3.4 o `Chat` mantinha uma conversa MLS por `PeerId` (um device), sem noção de "contato" nem de "meus outros
devices". A identidade (chave mestra) já certificava vários devices (`DeviceCertificate`), mas nada usava isso. A 3.5
precisa que uma mensagem a uma identidade chegue em todos os devices dela e apareça nos meus outros devices.

## Decisão

1. **Modelo de pares device↔device.** Cada par (device meu, device do contato) tem o próprio grupo MLS 1:1, como já era.
   Enviar a uma identidade é um fan-out: a mesma `Message` (mesmo `MessageId`, para `parent` e dedupe valerem entre
   devices) entra na outbox de cada device, cifrada na conversa daquele par (`Chat::devices_of`, `Chat::send_to`).
   `devices_of` sai dos certificados das conversas, sem lista paralela.
2. **Outbox por `(peer, id)`** (schema v1) e **coluna `to_identity`** (schema v2, aditiva) que marca o espelho.
3. **Espelho `Payload::Sent{to, message}`** (tag 3): ao enviar a um contato, cada device irmão recebe "enviei isto a
   `to`" e emite `ChatEvent::SentElsewhere`. Só vale se o **remetente autenticado** do MLS (`sender`) for da minha
   identidade. Não se espelha para a própria identidade (o irmão já recebe a mensagem comum).
4. **Conversa 1:1 imposta no `crypto`** (`MAX_MEMBERS = 2`): `decrypt` recusa commit e `join` recusa Welcome que levem o
   grupo a mais de 2 membros (`Error::TooManyMembers`). `invite` (quem convida) segue sem limite.
5. **Descoberta `Payload::DeviceList`** (tag 4, até 8 convites com certificado e endereços): cada device anuncia o seu
   convite mais os dos irmãos que conhece, uma vez por conexão e de novo quando a "família" muda. Quem recebe exige que
   todo item seja da identidade do remetente autenticado (senão recusa a lista inteira) e disca o que não conhece,
   exigindo a identidade no handshake. De contato: só a primeira lista por conexão age, no máximo 4 endereços por
   device, e um item não sobrescreve a identidade já exigida para aquele Peer ID.
6. **`Chat::set_advertised_addrs`** (API pública): fixa os endereços anunciados (IP fixo, port-forward, testes); vazio usa
   `Node::shareable_addresses`, que nunca devolve loopback.
7. **Pareamento**: copiar a chave mestra por link/QR (`kin://pair/...`), sem cifra de transporte inventada. **Não
   implementado** (etapa 6).

## Alternativas descartadas

- **Um grupo MLS por conversa com todos os devices.** É o "multi-device nativo" do MLS, mas reabre P9(a) (committer que
  some) e exige `Add`/`Remove` por proposta, que o ADR 0001 recusa. É o mesmo trabalho da Fase 5.
- **Grupo de sincronização só entre os meus devices, mais pares.** Só valeria com sincronização de histórico, fora do
  recorte.
- **Um `MessageId` por device.** Quebra `parent` (threads) e dedupe entre devices.
- **Autoria do espelho por `peers().first()`** (a primeira versão). Forjável: ver "O que a auditoria mostrou".
- **Certificado delegado pelo device antigo** (a mestra num device só). Muda o formato do certificado (v4) e cria ponto
  único; fica para o provedor TruthID (D7).
- **Anunciar também endereços de LAN/loopback** para o teste de integração. Vaza IPs privados a contatos; escolheu-se
  `set_advertised_addrs`.
- **Teto global de discagens automáticas.** Proposto e dispensado pelo dono nesta sessão.

## O que a auditoria mostrou (agente `security-auditor`, antes dos PRs #29 e #31)

- **PR #29 (corrigido):** o `crypto` aceitava commits de qualquer membro com `Add` de credencial válida. Um contato podia
  levar outras identidades ao grupo, fazer `peers().first()` ser o meu irmão e (a) forjar espelhos no meu histórico, (b)
  entrar no fan-out e receber texto em claro e o destino do que eu envio. Corrigido pela autoria via `sender` e por
  `devices_of` exigir exatamente 1 outro membro; a raiz foi fechada no PR #30 (limite de 2 membros no `crypto`).
- **PR #31 (parcialmente corrigido):** (I3) um item da lista não sobrescreve a identidade já exigida para um Peer ID (o
  certificado não prova posse do device); (I1) uma lista por conexão e 4 endereços por device vindos de contato. O resto
  está em "Em aberto".

## Consequências

- Mais banda e mais cifragem: uma cópia por device de destino, mais uma por device irmão (espelho); cada par gasta
  gerações MLS próprias (janela de 10 por par).
- A conversa passa a ser estritamente 1:1 no `crypto`. A Fase 5 (grupos) terá de transformar `MAX_MEMBERS` e a regra do
  `join` em política por conversa.
- Toda conexão com conversa já existente passa a emitir uma mensagem MLS de controle (a lista de devices), mesmo sem
  mailbox.
- P9(a) deixa de se aplicar: em par 1:1 não há commit concorrente por construção.
- Sem revogação de device: com a mestra copiada, revogar um device é trocar de identidade.

## Em aberto (decisões do dono; por isso o PR #31 não deve ser mesclado ainda)

1. **Consentimento.** Não existe lista de contatos: `on_hello`/`on_welcome` criam conversa com qualquer peer que conecte
   (`Expected.identity = None`). Hoje a descoberta vale para qualquer um que alcance o nó. Decidir se vale só para
   identidades aceitas por convite (liga com D5 e P11).
2. **Filtro de endereços (I2).** Endereços de terceiros sem filtro: loopback e rede privada (scan cego), `/dns4` e
   `/dnsaddr` (vaza o resolvedor, permite rebinding), e vazamento do meu IP direto a quem eu só falava por relay. Um
   filtro quebra LAN (IP privado) e relay com domínio (`/dns`).
3. **O que anunciar a estranhos (I4).** A lista entrega a qualquer par com conversa o Peer ID, os endereços e a chave
   Nostr de todos os devices da identidade.
4. **Limites que sobraram:** quem controla uma identidade gera Peer IDs novos a cada mensagem (sem `ConnectionLimits` no
   swarm, `DialFailed` enche a fila de eventos, `Node::deferred` sem teto); `household` sem teto, frescor nem remoção;
   `expected` sem teto e só limpo em `ready`; re-anúncio sem limite de taxa; uma transmissão que falha não repete a lista
   na conexão.
5. **Mirror e recibo:** o `Delivered` do irmão usa o mesmo `MessageId` e a CLI imprime "entregue" mesmo com o device do
   contato offline; `mirror_target` confunde "sem linha" com "não é espelho", e um `to_identity` corrompido trava a fila
   daquele peer; o `ALTER TABLE` do schema v2 não é idempotente em banco híbrido, e versão de schema maior que a
   conhecida é aceita em silêncio.
6. **Certificado sem prova de posse do device** (a identidade assina as chaves, o device não assina nada): mudança de
   cripto, só com confirmação (AGENTS.md §4).
7. **Pareamento (etapa 6)** e o roteiro manual com 2 processos da mesma identidade e relay real.
