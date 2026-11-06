# Fases Detalhadas — Planejamento Global

> **Nota**: Este é o planejamento inicial do projeto, criado a partir da spec original (Sessão 1).
> As etapas de cada fase serão detalhadas e ajustadas conforme as decisões D1–D6 (`ARCHITECTURE.md`)
> forem fechadas. Cada fase precisa ser testável isoladamente antes da próxima.
> **Nenhuma blockchain, token ou consenso global antes da Fase 7.**

---

### Fase 0 — Pré-projeto: estudo e decisões

**Objetivo**: Fechar as decisões que bloqueiam a Fase 1 antes de escrever código.

**Etapas**:
- [x] 0.1 — Estudar o prior art (`ROADMAP.md`), respondendo para cada um: *por que ele fez a escolha
  que fez para mensagens offline e NAT?* — feito na Sessão 2, ver `STUDY.md`
- [x] 0.2 — Decidir D3 (stack: `rust-libp2p` vs `iroh`) — **rust-libp2p** (Sessão 2)
- [x] 0.3 — Decidir D2 (E2EE: MLS vs Double Ratchet) — **MLS/openmls** (Sessão 2)
- [x] 0.4 — Decidir D4 (posicionamento do produto) — **híbrido pessoal + workspaces, threads em todo o app** (Sessão 2)
- [x] 0.5 — Decidir D1 (fallback/mailbox do dia zero) — **outbox → Nostr → peers** (Sessão 2). Necessário só a partir da Fase 3, mas molda a
  abstração de transporte desde a Fase 1

---

### Fase 1 — Identidade local + conexão direta + chat 1:1 E2EE

**Objetivo**: Fabio e um amigo conversam na mesma rede e pela internet quando há conectividade direta.

**Etapas** (preliminares):
- [x] 1.1 — Setup do núcleo Rust + estrutura de módulos (identity, transport, overlay, crypto, chat) — workspace com 1 crate por módulo + `kin-cli` (Sessão 3)
- [x] 1.2 — Identidade com provedores (D7): provedor standalone (keypair Ed25519) + interface pronta para o TruthID opcional; chave de device + Peer ID derivado — crate `identity` com `IdentityProvider`, `StandaloneIdentity`, `DeviceKey`, `DeviceCertificate` (Sessão 3)
- [x] 1.3 — Descoberta em LAN via mDNS — `Node` em `kin-transport` (TCP + Noise + Yamux, mDNS, auto-dial) (Sessão 3)
- [x] 1.4 — Conexão direta pela internet (IP alcançável) — `dial` por endereço com verificação do Peer ID, `identify`, mDNS opcional via `NodeConfig`; validado em loopback, não ainda entre redes distintas (Sessão 3)
- [x] 1.5 — E2EE 1:1 (conforme D2) — `kin-crypto` sobre `openmls` 0.9: `CryptoDevice`, `Conversation` (create/invite/join/encrypt/decrypt/rotate_keys), estado em memória (Sessão 3)
- [x] 1.6 — Chat 1:1 mínimo (desktop primeiro); formato de mensagem já com `parent_message_id` para threads (D4) — `Node::send` com ack no transporte, `kin-chat` (`Chat`, `Message`, handshake in-band) e CLI `kin` em modo terminal; estado ainda em memória (Sessão 3)
- [x] 1.6b — Persistência do estado MLS (P14): SQLite via `openmls_sqlite_storage`; conversas sobrevivem a reinício (Sessão 3)
- [x] 1.7 — Testes de integração com dois nós — `crates/chat/tests/two_nodes.rs` (6 cenários em loopback + mDNS: descoberta, peer offline, reinício de quem disca e de quem escuta, mensagens em voo, banco perdido) e helpers em `tests/common` (Sessão 3)

**Critério de pronto**: Fabio e amigo conversam na mesma rede e pela internet quando há conectividade
direta.

---

### Fase 2 — Hole punching + relay + convite por QR/link

**Objetivo**: Conversar atrás de CGNAT sem nenhum dos dois subir servidor.

**Etapas** (preliminares):
- [x] 2.1 — Hole punching (DCUtR + AutoNAT) — `kin-transport` escuta também em QUIC; `NodeEvent::HolePunch`, `NatStatus` e `PeerRoute { relayed }`; validado em loopback e no laboratório com NAT cone real (Sessão 3)
- [x] 2.2 — Relay público/de peer como fallback (circuit relay v2) — `NodeConfig::relays` / `relay_server` (`RelayLimits`, padrão sem limite de bytes para carregar conversas), CLI `--relay` / `--serve-relay`; conversa E2EE por relay testada, inclusive atrás de NAT simétrico (Sessão 3)
- [x] 2.3 — Lista de relays candidatos + medição/ranking local — `RelayBook` em `kin-transport`: pontuação por taxa de sucesso e latência, reserva nos N melhores (`relay_count`), backoff de 30 s e failover; `Node::relay_stats`, `RelayLost` (Sessão 3)
- [x] 2.4 — Convite por link/QR — `Invite` em `kin-chat` (certificado do device + endereços, `kin://invite/<base64url>`), `Chat::accept` fixa a identidade do convite no handshake; CLI `/invite` (link + QR), `/accept`, `--accept` (Sessão 3)

**Critério de pronto**: conversam atrás de CGNAT sem nenhum dos dois subir servidor. ✅ provado no laboratório de NAT (`lab/`), com relay de terceiros; falta o teste em redes reais (P17).

---

### Fase 3 — Store-and-forward (mailbox) + multi-device

**Objetivo**: Mensagem chega mesmo com o destinatário offline no envio.

**Etapas** (preliminares):
- [x] 3.1 — Abstração de transporte de mailbox (trocar o backend sem mexer no resto) — Sessão 7, ADR 0003
- [x] 3.2 — Outbox local (entrega na janela simultânea) — Sessão 6, ADR 0002
- [x] 3.3 — Backend de mailbox de fallback: relays Nostr (NIP-17/59, conforme D1). Spikes de leitura e de escrita feitos e certificado v3 com a chave secp256k1 do device feito na Sessão 8 (ADR 0004, aceito); camada de protocolo sem rede feita (`kin_chat::nostr`, `Mailbox::learn_peer`); `NostrMailbox` com websocket, vários relays e NIP-42 feito contra relays falsos; CLI com `--nostr-relay` e roteiro com relay real (`nos.lol`, `relay.damus.io`) feitos na Sessão 9 (limites restantes no P23)
- [ ] 3.4 — Replicação em N mailboxes com TTL
- [ ] 3.5 — Multi-device (vários devices por identidade, sincronização)

**Critério de pronto**: mensagem chega mesmo com destinatário offline no envio.

---

### Fase 4 — Overlay: gossip + DHT + app-relay com limites

**Objetivo**: A rede se sustenta com 10–50 peers encaminhando tráfego entre si.

**Etapas** (preliminares):
- [ ] 4.1 — `ANNOUNCE` assinado + gossip
- [ ] 4.2 — DHT (Kademlia) para `peer_id → onde encontrar` e, no nível de descoberta "Público", pesquisa por identidade (D5)
- [ ] 4.3 — App como relay/mailbox (peers como mailbox, conforme D1) com limites configuráveis (`CONTEXT.md`, "App como relay")
- [ ] 4.4 — Desktop como daemon (relay natural da rede)

**Critério de pronto**: 10–50 peers; mensagem encaminhada por peers intermediários.

---

### Fase 5 — Grupos/canais, arquivos, presença, notificações

**Objetivo**: Uso diário real por um grupo pequeno.

**Etapas** (preliminares):
- [ ] 5.1 — Grupos/canais e workspaces (conforme D2/D4; ver P10)
- [ ] 5.2 — Arquivos (P2P direto ou chunks por relay, com limite de tamanho em app-relay)
- [ ] 5.3 — Presença
- [ ] 5.4 — Respostas/reactions
- [ ] 5.5 — Notificações
- [ ] 5.6 — App mobile (Flutter via FFI)

**Critério de pronto**: uso diário real por um grupo pequeno.

---

### Fase 6 — Chamadas WebRTC + TURN opt-in; relay dedicado empacotado

**Objetivo**: Pequena empresa usando com a própria infra de mídia.

**Etapas** (preliminares):
- [ ] 6.1 — Chamadas 1:1 via WebRTC (STUN público)
- [ ] 6.2 — TURN opt-in configurável
- [ ] 6.3 — Chamadas em grupo pequeno (mesh)
- [ ] 6.4 — Relay dedicado empacotado para empresas/entusiastas

**Critério de pronto**: pequena empresa usando com a própria infra de mídia.

---

### Definition of Done da primeira versão "completa" (Fases 1–6)

Duas pessoas conversam sem servidor central. Peers encaminham tráfego. Conexões impossíveis
diretamente usam relay. Mensagens são E2EE e chegam quando o destinatário volta. Chamadas funcionam
via WebRTC + TURN. Identidades são chaves criptográficas. Qualquer um pode hospedar um relay.

**Ao chegar aqui, parar de adicionar infraestrutura e avaliar.**

---

### Fase 7 — *(opcional)* Roteamento multi-hop / privado

**Critério de pronto**: remetente e destinatário não se veem diretamente.

---

### Fase 8 — *(opcional)* Blockchain: registro de identidade/devices, proof-of-relay/storage, incentivos

**Critério de pronto**: só se houver problema real de incentivo a infraestrutura. Ver
`ROADMAP.md`, "Onde blockchain entraria".
