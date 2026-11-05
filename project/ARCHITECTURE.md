# Decisões de Arquitetura

## Registro de Decisões

| Decisão | Opções | Status |
|---|---|---|
| Nome do projeto | TruthNet vs outros | **Kin** (provisório) ✓ — decidido na Sessão 1. "TruthNet" foi rejeitado. Pode mudar |
| Relação com o TruthID | Módulo dentro do TruthID vs projeto separado que o consome | **Projeto separado** ✓ — a ideia de módulo de comunicação dentro do TruthID já foi descartada antes |
| Blockchain no caminho das mensagens | Sim vs não | **Nunca** ✓ — blockchain é péssima como banco de mensagens em tempo real. Blockchain só na Fase 8 opcional, para registro/incentivos |
| Descoberta de peers | Pseudo-blockchain de entradas vs registros assinados + gossip + DHT | **Registros assinados + gossip + DHT** ✓ — sem consenso global; last-writer-wins pelo timestamp assinado |
| Protocolo de mídia (chamadas) | Próprio vs WebRTC | **WebRTC** ✓ — não inventar protocolo de mídia |
| Background no mobile | No MVP vs depois | **Depois** ✓ — MVP: "quando o app está ativo, participa plenamente" |
| D1 — Fallback/mailbox do dia zero | Ver D1 abaixo | **Outbox → Nostr → peers** ✓ — decidido na Sessão 2 (2026-10-06), por fases. Ver "D1 — decisão" abaixo |
| D2 — E2EE | MLS vs Double Ratchet + sender keys | **MLS (`openmls`)** ✓ — decidido na Sessão 2 (2026-10-06), para 1:1 e grupos. Ver "D2 — decisão" abaixo |
| D3 — Stack | `rust-libp2p` vs `iroh` | **`rust-libp2p`** ✓ — decidido na Sessão 2 (2026-10-06). Núcleo em Rust; Tauri (desktop) + Flutter via FFI (mobile) no padrão do TruthID. Ver "D3 — decisão" abaixo |
| D4 — Posicionamento do produto | WhatsApp-like vs Slack/Discord-like vs broadcast | **Híbrido** ✓ — decidido na Sessão 2 (2026-10-06): pessoal estilo WhatsApp + workspaces estilo Slack/Discord, com threads em todo o app. Ver "D4 — decisão" abaixo |
| D5 — Anti-spam / Sybil | Rate limit, PoW, convites, reputação TruthID, depósito | **Contato por consentimento (modelo de amizade)** ✓ — decidido na Sessão 2 (2026-10-06). Ver "D5 — decisão" abaixo |
| D6 — Metadados | Quanto de privacidade de metadados no MVP | **Em aberto** — provavelmente pouco no MVP; resto na Fase 7 |
| D7 — Identidade (provedores) | TruthID obrigatório vs keypair próprio vs abstração com os dois | **Abstração com dois provedores** ✓ — decidido na Sessão 3 (2026-10-06): standalone (padrão) + TruthID opcional. Ver "D7 — decisão" abaixo |

---

## Decisões em Aberto (detalhe)

> Antes de gerar código que dependa de uma delas, levantar as opções com trade-offs e pedir decisão.

### D1 — Transporte de fallback/mailbox no dia zero (a mais importante)

O objetivo é não depender de servidor próprio, nem com 2 usuários, e ainda assim entregar mensagem
offline.

| Opção | Prós | Contras |
|---|---|---|
| **a) Relays públicos libp2p (circuit relay v2)** | Nativo do libp2p; muitos nós IPFS públicos | Projetado para **coordenar hole punching**, não para carregar tráfego: reservas com limite de duração e de bytes. Não serve como mailbox. Útil só como degrau para conexão direta |
| **b) Relays Nostr como mailbox** | Milhares de relays públicos já existentes, muitos gratuitos; guardam eventos (store-and-forward pronto); protocolo simples; payload cifrado (dá para usar gift-wrap para esconder o remetente) | Relays veem metadados (chave de destino, timing, tamanho); políticas de retenção e anti-spam variam; alguns são pagos; dependência de ecossistema externo |
| **c) Modelo SimpleX (filas em relays sem IDs de usuário)** | Melhor privacidade de metadados do mercado; qualquer um hospeda | Os relays existentes são da rede SimpleX, então exigiria compatibilizar ou reimplementar o protocolo |
| **d) Outbox local, sem mailbox** | Zero infra, 100% puro | Entrega só com os dois online simultaneamente |
| **e) Nó próprio no homelab como um relay entre vários** | Controle total; custo zero | É exatamente o "container só pra isso" que se quer evitar. Aceitável só como mais um item da lista |

Hipótese a validar: **d + b como fallback**, com **a** só para hole punching. Transporte abstrato
para trocar o backend de mailbox sem mexer no resto.

#### D1 — decisão (Sessão 2): outbox local → relays Nostr → peers como mailbox

Escolhido conforme a recomendação do assistente. Entrega por fases, sempre atrás da abstração de
transporte de mailbox (Fase 3.1), para o backend ser trocável sem mexer no resto.

| Fase | Mailbox |
|---|---|
| 1–2 | **Só outbox local**: entrega quando os dois estão online (opção **d**). O foco é provar conexão direta, hole punching e relay de fallback sem misturar com mailbox |
| 3 | **Relays Nostr** como 1º backend (opção **b**): gift wrap NIP-17/59, publicação nos relays da lista de DMs do destinatário (kind 10050), **replicação em N relays com TTL** |
| 4 | **Peers como mailbox** (app-relay / desktop daemon com limites, opção **e** como item da lista): reduz a dependência de terceiros |

`libp2p` circuit relay v2 (opção **a**) continua só para hole punching, não como mailbox.
Opção **c** (SimpleX) descartada por ora: exigiria compatibilizar/reimplementar o protocolo.

**Custos assumidos**:
- Relays Nostr enxergam metadados (timing, tamanho, chave de destino); retenção e anti-spam variam
  por relay e alguns são pagos. Tratar relay como **não confiável e efêmero**: nada depende de um
  relay específico (daí N réplicas e TTL).
- Mensagens MLS (commits, welcomes) são maiores que um DM comum e podem bater em limites de tamanho
  de relay; medir na Fase 3 e fragmentar se preciso.
- Durante as Fases 1–2 a UX é **síncrona** (os dois online), como o Briar sem mailbox.
- Mailbox de relays públicos **não resolve histórico de workspace** (retenção curta). Isso fica em
  P10 e provavelmente é resolvido por sincronização entre membros, não por relay.

**Interação com P9** (ordenação de commits MLS): a mailbox entrega commits fora de ordem e com
atraso, então o desenho de P9 deve assumir entrega **at-least-once, sem ordem garantida**.

**Reabrir se**: relays Nostr se mostrarem hostis (retenção/anti-spam) a mensagens MLS ou se a
privacidade de metadados virar requisito do MVP (D6), caso em que a opção **c** volta à mesa.

### D2 — E2EE

- **MLS (RFC 9420)**, ex.: crate `openmls`: padrão para grupos, escala bem, multi-device nativo. Mais complexo.
- **Double Ratchet (estilo Signal)** para 1:1 + sender keys para grupos: maduro, porém menos elegante
  para grupos grandes e multi-device.
- Hipótese: MLS para tudo, para não ter dois sistemas.

#### D2 — decisão (Sessão 2): MLS via `openmls`

Escolhido conforme a recomendação do assistente: um único sistema para 1:1 e grupos, multi-device
nativo (Fase 3.5) e independente do transporte, o que importa porque a mailbox (D1) pode ser
qualquer backend.

**Custos assumidos**:
- `openmls` ainda está em 0.x (0.9.0-rc.x na pesquisa); fixar versão e acompanhar breaking changes.
  Uma dependência criptográfica (libcrux, para ML-KEM) é pré-1.0 e sem auditoria completa, então
  usar apenas as cipher suites clássicas por padrão. *(verificar na doc do `openmls`)*
- MLS pressupõe um **Delivery Service** que ordena commits. Sem servidor, será preciso definir como
  ordenar/resolver commits concorrentes entre peers e como um device volta de offline (aplicar
  commits perdidos). **Isso é o principal risco técnico e vai para a Fase 1/3** (ver P9).
- Mais complexo que Double Ratchet para o caso 1:1 simples; um grupo de 2 pessoas já usa a árvore MLS.

**Mitigações**: manter o E2EE atrás de um módulo `crypto` com interface própria (encrypt/decrypt por
conversa), sem vazar tipos do `openmls` para o resto; spike curto de 1:1 na Fase 1.5 antes de
construir sobre ele.

**Implementação da 1.5 (Sessão 3)**: `openmls` 0.9.0 fixado (`=0.9.0`, API quebra a cada minor), provider
`openmls_rust_crypto` (só cipher suite clássica `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`).
- **Chave MLS por device**, gerada pelo `crypto` (não reaproveita a chave libp2p). A credencial MLS
  (`BasicCredential`) carrega um `DeviceCertificate` (D7) que certifica **a chave de assinatura MLS e a
  chave de rede (libp2p) do device** (P15). Ao aceitar KeyPackage, Welcome ou commit com `Add`, o `crypto`
  confere: assinatura do certificado, certificado cobre exatamente a chave do membro e (opcional) identidade
  e Peer ID esperados. O `chat` exige que o Peer ID da conexão seja o do certificado, então a identidade
  mostrada é a de quem está na linha.
- **Épocas/ordem (P9)**: `max_past_epochs = 3`, tolerância fora de ordem 10 gerações, avanço máx. 2000.
  Erros tipados: `Duplicate`, `TooOld`, `UnknownEpoch`, `WrongConversation`. Com 2 membros, só um lado
  deve emitir commits por vez; commits concorrentes **não** têm resolução automática no MLS
  (resolvido para 1:1 por committer designado: `project/adr/0001-ordenacao-de-commits-sem-delivery-service.md`).
- Estado do grupo e chaves só em memória (`MemoryStorage`); ver P14.

**Reabrir se**: o spike mostrar que ordenação de commits sem servidor é inviável ou frágil em P2P;
plano B: Double Ratchet (ex.: `vodozemac`; `libsignal` é AGPLv3) para 1:1 e sender keys para grupos.

### D3 — Stack

- Núcleo de rede em **Rust** (`rust-libp2p`), reaproveitável entre desktop (Tauri) e mobile (Flutter
  via FFI/`flutter_rust_bridge`), no mesmo padrão do TruthID.
- Alternativa a avaliar: `iroh` (Rust, QUIC, hole punching + relays próprios), mais simples que
  libp2p, porém com relay de modelo mais centralizado por padrão.

#### D3 — decisão (Sessão 2): `rust-libp2p`

Escolhido sobre `iroh` (recomendação do assistente era iroh; o usuário optou por libp2p).

**Ganhos**: mDNS, Kademlia, GossipSub, AutoNAT e DCUtR prontos (Fases 1 e 4 quase de graça);
ecossistema amplo e prior art (Berty) na mesma stack.

**Custos assumidos** (de `STUDY.md`):
- DCUtR mede ~70% de hole punching em escala (IMC '26), então ~30% dos pares precisam de relay que
  **carregue tráfego**. O circuit relay v2 padrão limita tempo e bytes e não serve para isso; o relay
  de fallback da Fase 2 terá de ser desenhado (relays de peers/dedicados, com limites configuráveis).
- Mais boilerplate (`NetworkBehaviour`) e crates com versões que quebram; fixar versões do `libp2p`.
- Sem caminho FFI oficial: `flutter_rust_bridge` por nossa conta (a validar na Fase 5.6).

**Mitigações**: manter o transporte atrás da abstração (`transport`), já exigida por D1, para que
`iroh` (ou o crate `libp2p-iroh`, WIP) possa ser avaliado depois sem reescrever o resto.

**Reabrir se**: o spike da Fase 1 mostrar que o relay de fallback custa mais do que o esperado.

#### D3 — como ficou a conectividade (Fase 2, Sessão 3)
- Escada: direto → circuit relay v2 → DCUtR troca o relay por conexão direta sozinho. O Noise do circuito
  autentica o peer de ponta a ponta, então o handshake MLS e a verificação do P15 valem igual por relay.
- O nó escuta em TCP **e** QUIC. Um nó serve de relay com `NodeConfig::relay_server`; o limite padrão do
  libp2p (128 KiB, 2 min) só serve para coordenar o furo, então `RelayLimits` tem padrão generoso
  (`max_circuit_bytes = 0` = sem limite) e é configurável.
- O cliente **recusa a reserva** se o relay não anunciar endereços externos: o relay registra os próprios
  endereços de escuta e aceita `external_addrs` (IP público atrás de port-forward).
- `PeerConnected` dispara só na 1ª conexão com o peer; `PeerRoute { relayed }` avisa cada conexão e a
  troca relay → direta (a conversa não refaz o handshake).
- Discar um endereço de circuito enquanto a conexão com o relay ainda abre para a reserva é cancelado pelo
  libp2p: o `Node` agora adia sozinho esse dial até a reserva (ou 10 s).

#### Convite por link (Fase 2.4) e relays candidatos (2.3)
- **Convite** = `kin://invite/<base64url>` com `versão | certificado do device | endereços`. O certificado
  (assinado pela identidade, cobre a chave de rede, P15) prova **quem** e **qual Peer ID**; os endereços são
  só dicas (o Noise confere o Peer ID ao discar). `Chat::accept` disca todos os endereços de uma vez e
  **exige** a identidade do convite no handshake MLS: um convite forjado que reivindica o Peer ID de
  outra pessoa é descartado. Gerar o link é o consentimento (D5, "só por link"): sem pedido de amizade,
  sem caixa de entrada (P11 segue aberta para o resto). Sem expiração nem revogação por enquanto (P18).
- Endereços compartilháveis (`Node::shareable_addresses`): circuito de relay (sem loopback), externos
  informados e endereços de escuta de IP **global**; LAN/loopback ficam de fora.
- **Relays**: `NodeConfig::relays` são candidatos; o nó mede cada um (sucesso e tempo até a reserva,
  EWMA), reserva nos `relay_count` melhores (padrão 2), dá 30 s de molho a quem falha e passa para o
  próximo. Sem relay oficial; o ranking é local e efêmero (não persiste).

### D4 — Posicionamento do produto

WhatsApp-like, Slack/Discord-like ou broadcast (Twitter-like)? Muda modelo de dados, grupos e
moderação. Broadcast 1→N é tecnicamente bem mais simples (gossip/pub-sub puro, sem E2EE de grupo
privado) e pode ser um MVP mais barato.

#### D4 — decisão (Sessão 2): híbrido pessoal + workspaces, threads em todo lugar

Decisão do usuário (não era nenhuma das opções puras). Em palavras do usuário: no pessoal é "tipo um
WhatsApp", mas dá para entrar em **workspaces** de trabalho ou de projetos de amigos; e detalhes como
**comentar uma mensagem em thread** (estilo Slack) devem existir **no app todo**.

**Modelo conceitual (proposta do assistente, a refinar):**
- **Espaço pessoal**: contatos, conversas 1:1 e grupos pequenos (convite por QR/link).
- **Workspace**: contêiner com membros e **canais** (privados ou abertos aos membros). Uma pessoa
  pertence a vários workspaces com a mesma identidade.
- **Thread**: qualquer mensagem, em qualquer conversa (1:1, grupo ou canal), pode ter respostas em
  thread. No modelo de dados isso é só `parent_message_id` + `thread_root_id` na mensagem.

**Consequências nas outras decisões:**
- **D2 (MLS)**: encaixa bem. Cada conversa/canal vira um grupo MLS. Threads não criam grupo novo,
  são mensagens dentro do mesmo grupo. Workspaces grandes pedem atenção ao custo de commits (P10).
- **D1 (mailbox)**: o volume e o histórico de canais é maior que o de 1:1, então a retenção
  curta de relays públicos (ex.: Nostr) não basta para histórico de workspace. Ver P10.
- **D5 (anti-spam)**: workspaces são fechados por convite, o que reduz a superfície de spam.
- **Fases**: workspaces e papéis ficam para a Fase 5/6; **threads entram no modelo de mensagem
  desde a Fase 1** (campo `parent_message_id` desde o primeiro formato), porque mudar formato
  de mensagem depois é caro. A UI de thread pode vir depois, mas o campo não.

**Fora do escopo por ora**: canais públicos abertos a estranhos (isso é broadcast e puxa D5).

### D5 — Anti-spam / Sybil

Criar 1 milhão de identidades é grátis. Candidatos: rate limiting por identidade, proof-of-work leve
no primeiro contato, convites/trust graph, reputação atrelada ao TruthID, depósito em casos
específicos. Relays públicos (principalmente Nostr) já têm as próprias políticas, e o app precisa
conviver com elas.

#### D5 — decisão (Sessão 2): contato por consentimento, no modelo de amizade

Decisão do usuário: funcionar **como amizade**. Ninguém te contata, entra em conversa ou workspace
sem consentimento; o que varia é o quão fácil é **te encontrar**. O usuário foi explícito: prefere
controle de consentimento a mecanismos técnicos de custo (PoW, depósito).

**Níveis de descoberta** (configurável por identidade, o usuário escolhe):
- **Público**: qualquer um com o seu link/QR **ou que te pesquise** te encontra e pode te enviar
  solicitação de amizade.
- **Só por link**: só quem tem o seu link/QR consegue enviar solicitação; não aparece em pesquisa.
- **Fechado**: ninguém envia solicitação; você é quem inicia contatos (e aceita convites que
  gerou).

**Regras (valem em qualquer nível):**
- Solicitação de amizade é a **única** coisa que um desconhecido pode enviar. Não abre conversa,
  não entra em grupo/workspace, não gera notificação de mensagem.
- Solicitações chegam numa **caixa de entrada própria** e **não exigem resposta**: você pode ignorar
  para sempre. Ignorar não avisa o remetente (sem recibo de rejeição).
- Só depois de **aceitar** o contato (ou de ter sido convidado por um link seu) há conversa 1:1.
- Entrar em **grupos e workspaces** exige convite e aceite explícitos de quem entra. Estar em um
  workspace não dá permissão de DM com os membros: contato continua sendo um consentimento à parte
  (configurável: "membros do mesmo workspace podem me enviar solicitação").
- **Bloquear** descarta tudo da identidade bloqueada sem notificá-la.

**Camada técnica mínima por baixo** (defesa em profundidade): rate limit por identidade no envio de
solicitações e tamanho máximo da solicitação (sem anexos/mídia), porque a solicitação é o único
vetor aberto. PoW, depósito e reputação do TruthID ficam **descartados por ora**; reputação do
TruthID pode voltar como filtro opcional ("só aceitar solicitações de identidades verificadas").

**Consequências:**
- **Fase 2 (convite por QR/link)** já é o mecanismo principal do nível "só por link". Pesquisa por
  nome/identidade só é possível com descoberta distribuída (**DHT, Fase 4**); até lá, só link/QR.
- A solicitação de amizade é um tipo de mensagem pré-contato: precisa de formato próprio, assinado,
  que **não exige sessão MLS** (a sessão nasce no aceite). Ver P11.
- **D1**: quem te envia solicitação precisa de um lugar para entregá-la mesmo você offline; o endpoint
  de "solicitações" é público por design e vira alvo de spam nos relays/mailboxes (P11).
- **D6**: o nível "Público" expõe que você existe e quando está acessível; o nível "Fechado" é o
  mais privado.

**Reabrir se**: a caixa de solicitações virar canal de spam na prática (caso em que entram PoW leve
ou filtro por reputação TruthID).

### D6 — Metadados

E2EE protege conteúdo, não quem fala com quem e quando. Definir quanto de privacidade de metadados
entra no MVP (provavelmente pouco) e quanto fica para a Fase 7.

### D7 — decisão: identidade com provedores (standalone + TruthID opcional)

**Decidido na Sessão 3 (2026-10-06)**, a pedido do usuário: o TruthID é a identidade do ecossistema,
mas o Kin tem de funcionar também para quem **não usa o TruthID**.

**Achado que motivou**: o TruthID real é on-chain (wallet/Ledger → smart account ERC-4337 na Base,
username, `identityId`, devices pareados) e **não tem hoje um "modo local" reutilizável como crate**
(desktop em Tauri/TS; único crate Rust fica em `desktop/src-tauri`). O `CONTEXT.md` assumia o contrário.

**Decisão**: o módulo `identity` define uma abstração, e o resto do Kin (transport, crypto, chat)
só enxerga ela:
- ID estável da identidade (chave pública);
- capacidade de **assinar**;
- **autorização de devices** ("este device pertence a esta identidade").

Provedores:
1. **Standalone (padrão)** — keypair Ed25519 gerado pelo Kin; sem conta, sem blockchain. É o que a
   Fase 1 implementa.
2. **TruthID (opcional)** — o usuário vincula a identidade TruthID. O Kin guarda uma **prova assinada**
   "esta chave Kin pertence a esta identidade TruthID", que viaja no `ANNOUNCE` e no convite.

**Promoção sem perda**: a chave de mensagens é a mesma nos dois casos; vincular o TruthID só adiciona
a prova. Contatos e conversas sobrevivem.

**Consequências**:
- Fase 1.2 implementa só o provedor standalone, mas com a interface pronta para o TruthID.
- Raiz da hierarquia de chaves: standalone = Master Identity Key gerada pelo Kin; TruthID = raiz na
  wallet, o Kin recebe uma delegação (P12).
- O vínculo é só uma alegação até ser verificado (consulta à chain/RPC da Base ou prova off-chain) — P12.

**Reabrir se**: o TruthID ganhar um modo local/SDK Rust reutilizável (aí o provedor standalone pode
passar a ser esse modo).
