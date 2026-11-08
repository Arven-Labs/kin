# ADR 0006 — Multi-device: pares device↔device, espelho e descoberta (Fase 3.5)

- **Status**: parcialmente aceito (Sessão 15, 2026-11-08). Modelo, espelho e limite de 2 membros aceitos e na `main`
  (PRs #26 a #30). A descoberta de devices (PR #31) está aberta; as três decisões de produto que a travavam
  (consentimento, filtro de endereços, o que anunciar) foram tomadas pelo dono e aplicadas no próprio PR (ver
  decisão 5). O pareamento (etapa 6) não foi feito.
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
5. **Descoberta `Payload::DeviceList`** (tag 4, até 8 convites com certificado e endereços). Decisões do dono, depois
   da auditoria do PR #31:
   - **Consentimento**: só se processa a lista de, e só se anuncia a, identidades aceitas (`accept(invite)` ou
     `Chat::trust`) e a irmãos (a minha identidade). Quem apenas conectou não me faz discar nem recebe lista. A confiança
     fica em memória (persistir é pendência) e é compartilhada pela família: um irmão que repassa o device de um contato
     faz este device confiar naquela identidade.
   - **O que anunciar** (privacidade): a **irmão**, a lista completa (o meu convite, os irmãos e os devices de contatos
     que conheço); a **contato aceito**, só o meu convite; a quem não aceitei, nada. Assim os endereços de todos os meus
     devices não vão ao contato, e os outros devices dele se acham pelos irmãos (cada device fala uma vez com o contato,
     e o irmão repassa o convite recebido).
   - **Filtro de endereços** (`dialable`): `/dnsaddr` nunca. De irmão, o resto vale. De contato, só IP global por
     tcp ou quic, ou circuito por relay de IP global; recusa loopback, rede privada, link-local, CGNAT e nomes DNS.
   - Quem recebe de contato exige que todo item seja da identidade do remetente autenticado (senão recusa a lista
     inteira); só a primeira lista por conexão age; no máximo 4 endereços por device; um item não sobrescreve a
     identidade já exigida para aquele Peer ID. De irmão, itens da minha identidade viram a "família" e itens de outra
     identidade viram `contact_devices`; tudo o que não tem conversa é discado, exigindo a identidade dona do item.
6. **`Chat::set_advertised_addrs`** (API pública): fixa os endereços anunciados (IP fixo, port-forward, testes); vazio usa
   `Node::shareable_addresses`, que nunca devolve loopback.
7. **Pareamento**: copiar a chave mestra por link/QR (`kin://pair/...`), sem cifra de transporte inventada. **Implementado
   na Sessão 16** (etapa 6): `StandaloneIdentity::to_pair_link`/`from_pair_link`; na CLI, `/pair` (só imprime depois de
   `/pair confirmo`) e `--pair ARQUIVO` (ou `-`), que importa na primeira execução e recusa se já existir
   `identity.key`. O link nunca vai por argv. É o segredo inteiro, em claro, sem revogação; a alternativa maior (enviar
   a chave pelo canal MLS depois de confirmar um código curto, sem o segredo aparecer em tela) ficou como evolução.

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
- **PR #31 (corrigido, com as decisões do dono):** (I3) um item da lista não sobrescreve a identidade já exigida para um
  Peer ID (o certificado não prova posse do device); (I1) uma lista por conexão e 4 endereços por device vindos de
  contato; (I2) filtro de endereços; (I4) só o meu convite a contato aceito, nada a quem não aceitei; e o consentimento
  (não havia lista de contatos: qualquer peer que conectasse tinha a lista processada). O resto está em "Em aberto".

## Consequências

- Mais banda e mais cifragem: uma cópia por device de destino, mais uma por device irmão (espelho); cada par gasta
  gerações MLS próprias (janela de 10 por par).
- A conversa passa a ser estritamente 1:1 no `crypto`. A Fase 5 (grupos) terá de transformar `MAX_MEMBERS` e a regra do
  `join` em política por conversa.
- Toda conexão com conversa já existente passa a emitir uma mensagem MLS de controle (a lista de devices), mesmo sem
  mailbox.
- P9(a) deixa de se aplicar: em par 1:1 não há commit concorrente por construção.
- Sem revogação de device: com a mestra copiada, revogar um device é trocar de identidade.

## Consequências da descoberta com consentimento

- Um contato na mesma LAN só se acha por mDNS (o filtro recusa IP privado vindo de contato), e relay com domínio vindo de
  contato não é discado automaticamente. Irmão não tem essas restrições.
- **Os dois lados precisam aceitar.** Quem compartilhou o convite tem de chamar `Chat::trust` na identidade que entrou
  (o aplicativo, ao ver `ConversationReady`); `accept` só cobre quem aceitou o convite. **A CLI ainda não chama `trust`
  para o lado de quem convidou**: sem isso a descoberta não age nesse lado. Pendência.
- A descoberta entre casas depende de cada device falar uma vez com o contato; o irmão repassa o que recebeu. O teto de
  8 itens por lista limita quantos devices de contatos um irmão repassa (excedente fica de fora, em silêncio).
- O consentimento é imposto dos dois lados (quem anuncia e quem recebe); cada lado tem teste unitário próprio, e o teste
  de integração sozinho não distingue qual lado barrou.

## Em aberto

1. **Persistir a confiança** (hoje `trusted` é só memória) e ter uma lista de contatos de verdade (liga com D5 e P11).
2. **Limites que sobraram:** quem controla uma identidade gera Peer IDs novos a cada mensagem (sem `ConnectionLimits` no
   swarm, `DialFailed` enche a fila de eventos, `Node::deferred` sem teto); `household` sem teto, frescor nem remoção;
   `expected` sem teto e só limpo em `ready`; re-anúncio sem limite de taxa; uma transmissão que falha não repete a lista
   na conexão.
3. **Espelho e recibo:** o `Delivered` do irmão usa o mesmo `MessageId` e a CLI imprime "entregue" mesmo com o device do
   contato offline; `mirror_target` confunde "sem linha" com "não é espelho", e um `to_identity` corrompido trava a fila
   daquele peer; o `ALTER TABLE` do schema v2 não é idempotente em banco híbrido, e versão de schema maior que a
   conhecida é aceita em silêncio.
4. **Certificado sem prova de posse do device** (a identidade assina as chaves, o device não assina nada): mudança de
   cripto, só com confirmação (AGENTS.md §4).
5. ~~Pareamento (etapa 6)~~ (feito na Sessão 16) e o roteiro manual com 2 processos da mesma identidade e relay real.
