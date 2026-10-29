# Log de Sessões

> **Nota**: Este log foi criado junto com o projeto. As sessões serão registradas aqui conforme o trabalho avança.
>
> Última atualização: 2026-10-29 (Sessão 4)

---

### 2026-09-27 — Sessão 1

- **Objetivo**: registrar o projeto a partir da spec `SPEC-rede-comunicacao-p2p.md` (rede de
  comunicação P2P descentralizada, mensageiro como primeira aplicação).
- **Decisões do usuário**:
  - nome **Kin** ("meio fixo, mas podemos mudar depois"); "TruthNet" já tinha sido rejeitado;
  - estrutura `project/` igual à dos outros projetos do workspace (TruthID, Warden, Anchor);
  - repositório público no GitHub (`masterlxz/kin`), como os outros.
- **Feito**:
  - conteúdo da spec distribuído em `project/` (`CONTEXT.md` = visão/PRD, `PHASE.md` = fases,
    `ARCHITECTURE.md` = decisões tomadas + D1–D6 em aberto, `ROADMAP.md` = prior art/riscos/blockchain,
    `PENDING.md` = P1–P8);
  - adicionada uma **Fase 0** (estudo de prior art + decisões) antes da Fase 1;
  - spec original removida (o conteúdo vive em `project/`);
  - README, LICENSE (MIT), `.gitignore`, primeiro commit e push.
- **Próximo passo**: nenhum código ainda. O projeto espera na fila; quando começar, Fase 0.

---

### 2026-10-06 — Sessão 2

- **Objetivo**: começar aos poucos; concluir **todo o estudo** (Fase 0.1) antes de tomar qualquer decisão.
- **Decisões do usuário**: estudar tudo primeiro, depois decidir (D1–D6 seguem em aberto).
- **Feito**:
  - comparativo preliminar `rust-libp2p` vs `iroh` (D3), em conversa: iroh chegou à 1.0 e
    resolve hole punching + relay de fábrica; libp2p leva vantagem em DHT/gossip (Fase 4);
    **nenhuma decisão tomada**;
  - estudo dos 8 itens de prior art registrado em `STUDY.md` (Briar, Berty/Wesh, Jami, Session,
    SimpleX, Nostr, Matrix P2P/Pinecone, libp2p), com síntese por decisão e 4 perguntas abertas;
  - achado de maior peso: medição do DCUtR em escala (IMC '26) dá ~70% de sucesso de hole punching,
    ou seja, ~30% dos pares precisam de relay que carregue tráfego;
  - etapa 0.1 marcada como concluída; P8 resolvida.
- **Decisão D3** (mesma sessão): usuário escolheu **`rust-libp2p`** (assistente recomendava iroh).
  Custos e mitigações em `ARCHITECTURE.md`; etapa 0.2 e P3 resolvidas.
- **Decisão D2** (mesma sessão): usuário escolheu **MLS via `openmls`** (recomendação do assistente).
  Risco principal: ordenação de commits sem Delivery Service, registrado como P9. Etapa 0.3 e P2
  resolvidas.
- **Decisão D4** (mesma sessão): usuário rejeitou as opções puras e definiu um **híbrido**: pessoal
  estilo WhatsApp + workspaces estilo Slack/Discord, com **threads em todo o app**. Threads entram no
  formato de mensagem desde a Fase 1; workspaces na Fase 5/6. Pendência P10 aberta (histórico,
  papéis, custo MLS sem servidor). Etapa 0.4 e P4 resolvidas.
- **Decisão D1** (mesma sessão): usuário escolheu **outbox local → relays Nostr (Fase 3) → peers como
  mailbox (Fase 4)** (recomendação do assistente). P9 passa a assumir entrega at-least-once sem ordem.
  Etapa 0.5 e P1 resolvidas. **Fase 0 concluída** (só D6 segue aberta, de prioridade baixa).
- **Decisão D5** (mesma sessão): usuário preferiu **contato por consentimento, como amizade**:
  níveis de descoberta configuráveis (Público / só por link / Fechado); desconhecido só pode enviar
  solicitação de amizade, que cai numa caixa própria, não exige resposta e não avisa quem enviou;
  grupos/workspaces exigem convite e aceite. Rate limit por identidade como camada mínima; PoW/depósito
  descartados. P11 aberta (solicitação sem servidor). P5 resolvida.
- **Ressalva**: parte do `STUDY.md` vem de conhecimento prévio do modelo (marcada no texto) e precisa
  ser verificada na documentação oficial antes de virar premissa.
- **Próximo passo**: Fase 1.1 (setup do workspace Rust com os módulos identity, transport, overlay,
  crypto e chat). D6 (metadados) pode esperar até a Fase 4. Ainda sem código.

---

### 2026-10-06 — Sessão 3

- **Objetivo**: retomar após a Fase 0; ia começar a 1.1 (workspace Rust).
- **Dúvida do usuário**: o TruthID seria a identidade? Confirmado que já constava na spec (Sessão 1),
  mas o *como* nunca foi definido.
- **Achado**: o TruthID real é on-chain (wallet/Ledger → smart account ERC-4337 na Base) e não tem
  modo local reutilizável como crate; o `CONTEXT.md` assumia o contrário.
- **Decisão D7** (pedido do usuário): TruthID como identidade, mas utilizável por quem não é usuário
  dele → abstração com dois provedores: **standalone (padrão, Ed25519)** + **TruthID opcional** via
  prova assinada de vínculo; promoção sem perda de contatos. Fase 1.2 ajustada. P12 aberta
  (verificação do vínculo, raiz de chaves, integração).
- **Fase 1.1 feita**: workspace Cargo (edition 2024, Rust 1.98) com `crates/{identity,crypto,transport,overlay,chat,cli}`;
  dependências entre crates declaradas (identity na base; chat depende de identity, crypto e transport);
  `libp2p` 0.57 e `openmls` 0.9 confirmados no crates.io mas só entram nos crates quando usados
  (1.3 e 1.5). Escolhas: workspace com vários crates (mais organizado, reaproveitável pelo Tauri/Flutter);
  CI adiado até haver testes reais (depois da 1.2). `Cargo.lock` passou a ser versionado.
  `cargo clippy -D warnings` e `cargo test` limpos.
- **Fase 1.2 feita**: crate `kin-identity` sobre `libp2p-identity` 0.3 (só a parte de chaves/PeerId,
  sem puxar a stack libp2p inteira). Trait `IdentityProvider` (id, sign, authorize_device);
  `StandaloneIdentity` (Ed25519, save/load em arquivo 0600); `DeviceKey` (Peer ID derivado, expõe o
  `Keypair` para o transporte); `DeviceCertificate` assinado pela identidade com separador de domínio
  (`kin/device-cert/v1`) e `verify()`. 6 testes de integração (assinatura, certificado adulterado/forjado/
  device trocado, roundtrip de arquivo + permissão, bytes inválidos). P13 aberta (chave sem cifra em repouso).
- **Fase 1.3 feita**: `kin_transport::Node` sobre `libp2p` 0.57 (features tokio, tcp, noise, yamux,
  mdns, macros; `default-features = false`). Usa o `Keypair` do `DeviceKey` (Peer ID = o da identidade);
  escuta em `/ip4/0.0.0.0/tcp/0`, descobre peers por mDNS e disca neles sozinho. API: `new`, `listen`,
  `dial`, `peer_id`, `next_event()` com `NodeEvent::{Listening, PeerDiscovered, PeerConnected,
  PeerDisconnected}`. Teste de integração com dois nós no mesmo processo (8/8 execuções ok, ~0,08 s).
  Ressalva: o teste depende de multicast local; pode falhar em CI/containers sem multicast (relevante
  quando o CI entrar).
- **Fase 1.4 feita**: `Node::new(keypair, NodeConfig)` com mDNS opcional (`Toggle`), `listen_on`,
  `dial` por multiaddr e `identify` (feature adicionada ao `libp2p`). Novos eventos:
  `PeerIdentified {peer, listen_addrs, observed_addr}` e `DialFailed`. Testes (`tests/direct.rs`):
  dial por endereço + troca de identify, os dois lados enxergam a conexão, e **dial com Peer ID errado
  é recusado** (o Noise prova a identidade do outro lado). Tudo em loopback (4 testes + o de mDNS ok).
  **Ressalva**: ainda não testado entre duas redes reais (IP público); só confirma que o caminho
  funciona sem mDNS. O `observed_addr` do identify não é adotado como endereço externo (isso é
  AutoNAT/DCUtR, Fase 2).
- **Explicação e plano da 1.5**: usuário pediu primeiro uma explicação de E2EE/MLS e um plano (modo
  plano, pesquisa da API do `openmls` 0.9 por agente; depois confirmada compilando e lendo o código-fonte).
  Decisões do usuário: spike no próprio crate com interface final e estado em memória; chave MLS por
  device certificada pela identidade.
- **Fase 1.5 feita**: `kin-crypto` (`CryptoDevice`, `Conversation`, `Decrypted`, `Error`) sem vazar tipos
  do `openmls`; `kin-identity` ganhou `DeviceCertificate::to_bytes/from_bytes`, `public_key_from_ed25519`
  e `ed25519_bytes`. 10 testes de integração + 3 unitários no `crypto` (fluxo nos dois sentidos, texto
  adulterado, duplicata → `Duplicate`, fora de ordem, outra conversa, rotação de época, mensagem do
  commit chegando antes/depois, Welcome de outro device, identidade esperada) e 3 novos no `identity`.
  Achados na pesquisa/código: `MlsMessageIn::into_welcome` só existe em testes (usar `extract()`);
  `ProcessMessageError` é genérico no storage; duplicata = `SecretReuseError`. P14 (persistência MLS) e
  P15 (Peer ID ↔ chave MLS) abertas; P9 segue aberta para commits concorrentes/grupos.
- **Fase 1.6 feita** (decisões do usuário: P14 fica fora da 1.6; entrega = lib + CLI mínima):
  - `kin-transport`: `request-response` com codec próprio (u32 BE + bytes, máx. 64 KiB, ack de 1 byte);
    `Node::send(peer, bytes) -> SendId` e eventos `MessageReceived`/`MessageDelivered`/`SendFailed`.
  - `kin-chat`: `Message` (id aleatório de 128 bits, `parent: Option<MessageId>`, `sent_at_ms`, texto;
    formato v1 manual, sem serde), `Chat` (um `Conversation` por peer, em memória) e handshake in-band
    `Hello → KeyPackage → Welcome → Mls`; o de **menor** Peer ID convida (evita convite duplo quando os
    dois discam ao mesmo tempo via mDNS). Duplicata/antiga demais do MLS é ignorada; o resto vira
    `ChatEvent::Dropped`.
  - `kin-cli`: `kin [--data-dir] [--listen] [--dial] [--no-mdns]`, identidade e chave de device
    persistidas (0600, sem cifra: P13), `/r <id> texto` para thread, `/quit`. Smoke test real com duas
    instâncias em loopback ok.
  - Testes: 2 no transporte (entrega+ack, mensagem grande demais), 3 no chat (handshake + dois sentidos
    com thread, envio sem conversa, formato). Ressalva: o handshake só avança se o app dirige o nó em
    loop contínuo (o swarm só progride quando polado).
  - **Ressalva de segurança (P15)**: a identidade mostrada vem do certificado MLS, não da conexão; nada
    prova que o Peer ID que enviou o Welcome/KeyPackage é o device daquele certificado.
- **P14 resolvida (Fase 1.6b)**: `kin-crypto` ganhou `Provider` próprio (RustCrypto + `SqliteStorageProvider`
  com codec JSON; migrações antes de compartilhar a conexão em `Rc`). O mesmo banco guarda a chave de
  assinatura MLS e o certificado do device (recarregados em `CryptoDevice::open`, que falha com
  `IdentityMismatch` se o banco é de outra identidade) e a tabela `kin_conversations` (Peer ID → id do
  grupo). `Conversation::load`; `Chat::open` recarrega as conversas e reanuncia `ConversationReady` a
  cada reconexão, então não há novo handshake após reiniciar. Testes: 2 no crypto (reinício dos dois
  lados com mensagem em voo e duplicata após reinício; mesma chave e rejeição de outra identidade) e 1
  no chat; smoke test real da CLI em duas sessões seguidas ok.
  **Custos assumidos**: `rusqlite::Connection` não é `Sync` → `CryptoDevice`/`Chat` são `!Send` (o clippy
  barrou o `Arc`; usar `Rc`); um app Tauri/Flutter terá de manter o `Chat` numa thread dedicada ou
  `LocalSet`. Banco sem cifra em repouso (P13), arquivo 0600. P9 segue aberta (commits concorrentes).
- **Fase 1.7 feita** (só testes, nenhum código de produção alterado): `crates/chat/tests/two_nodes.rs` com 6
  cenários e helpers extraídos para `tests/common/mod.rs` (agora usados também por `chat.rs`):
  1. mDNS no chat (duas instâncias se acham e conversam sem `dial`);
  2. envio a peer offline → `SendFailed`, nunca `Delivered` (lacuna do outbox, D1/Fase 3);
  3. quem disca reinicia (outra porta, mesmo banco) → conversa volta sem novo handshake;
  4. quem escuta reinicia → o outro redisca, idem;
  5. mensagens em voo na queda não são reentregues, só as novas chegam, uma vez cada;
  6. um lado perde o banco → o handshake **não** se refaz (ver P16).
  Achados: `Node` não tem API de desconectar (queda nos testes = `drop`); com banco perdido, o lado que
  ainda tem estado reanuncia `ConversationReady` e o remetente recebe `Delivered` por mensagens que o
  destino só descarta (`Dropped`) → **P16** aberta. Estável em 5 execuções seguidas; `cargo test
  --workspace`, clippy e fmt ok.
- **P15 resolvida** (decisão: certificado cobre as duas chaves, em vez de reusar a mesma chave nos dois
  protocolos ou assinar o Peer ID à parte):
  - `identity`: `DeviceCertificate` v2 (domínio `kin/device-cert/v2`) com `signing_key` (MLS, antes `device`)
    e `network_key`; `authorize_device(signing_key, network_key, created_at)`; `device_peer_id()` →
    `network_peer_id()`. Payload com prefixo de tamanho nas chaves.
  - `crypto`: `Expected { identity, peer }`; `invite`/`join` recusam Peer ID diferente (`UnexpectedPeer`);
    `CryptoDevice::new/open` recebem a chave de rede e `open` falha com `DeviceMismatch` se ela mudou;
    `Conversation::peer_ids`. **Achado**: o `join` antigo conferia os membros depois de `into_group`, ou
    seja, com o grupo já gravado no banco; agora confere o `StagedWelcome` antes, sem deixar grupo órfão.
    O openmls consome o KeyPackage ao ler o Welcome, então um Welcome recusado queima o KeyPackage (o
    peer só manda outro num novo `Hello`/reconexão).
  - `chat`: `on_key_package`/`on_welcome` exigem o Peer ID da conexão; `Chat::open` ignora conversa cujo
    rótulo não bate com o certificado. Teste unitário `key_package_from_another_peer_is_refused`.
  - Quebra deliberada: bancos `.kin` antigos (certificado v1) não abrem; apagar `mls.sqlite`. Smoke test
    da CLI com dois diretórios novos ok; `cargo test --workspace`, clippy e fmt ok.
- **Fase 2 (rodada 1: 2.1 hole punching + AutoNAT e 2.2 relay) feita** (decisões do usuário: só a
  conectividade nesta rodada, 2.3/2.4 ficam para a próxima; laboratório de NAT com Docker):
  - `kin-transport`: libp2p com `relay`, `dcutr`, `autonat`, `quic`, `dns`; `NodeConfig` ganhou `relays`,
    `relay_server: Option<RelayLimits>`, `external_addrs`, `autonat_global_only`; `Node::use_relay`,
    `circuit_address`; eventos `PeerRoute`, `NatStatus`, `RelayReserved`, `HolePunch`. `PeerConnected` agora
    só na 1ª conexão (as seguintes viram `PeerRoute`). `kin-chat` repassa tudo e expõe `DialFailed`
    (antes engolido). CLI: `--relay`, `--serve-relay`, `--external-addr`, `--relay-max-*`, status de rota.
  - Testes: `transport/tests/relay.rs` (4: mensagem com ack por relay, 5×60 KiB sem limite de bytes,
    limite de bytes corta o circuito, DCUtR troca relay por direta em loopback), `autonat.rs` (15 s por
    causa do `boot_delay`), `chat/tests/relay.rs` (handshake MLS + P15 por relay).
  - **Laboratório** `lab/` (Docker, 133 MB, binário montado do host): relay + 2 roteadores com NAT
    (iptables) + a + b. `lab/run.sh` roda: NAT cone → conversa pronta pelo relay e depois **conexão direta
    por hole punching**; NAT simétrico (`--random-fully`) → continua pelo relay; relay cai depois do furo →
    direta segue viva. Estável em 4 execuções completas seguidas.
  - **Achados**: (1) o cliente libp2p **recusa a reserva** se o relay não anunciar endereços externos
    (`NoAddressesInReservation`); (2) discar o circuito junto da abertura da conexão com o relay cancela o
    dial; (3) o roteador de laboratório precisa **descartar** (não RST) o SYN antecipado, como um NAT
    doméstico; (4) meu primeiro padrão de log confundia a conexão direta com o *relay* com a direta com o
    *peer* (falso positivo), corrigido exigindo o Peer ID do outro lado.
- **Fase 2 (rodada 2: 2.4 convite e 2.3 relays com ranking) feita — Fase 2 concluída**:
  - `kin-transport`: `Node::dial_peer` (vários endereços, Peer ID conferido), dial de circuito adiado dentro
    do `Node` até a reserva (resolve P17f), `shareable_addresses` (circuito sem loopback, externos, escuta
    global). `RelayBook` (módulo `relays`): candidatos, pontuação (taxa de sucesso suavizada ÷ latência),
    reserva nos `relay_count` melhores, backoff de 30 s, prazo de 15 s por tentativa, `RelayLost`,
    `relay_stats`, `add_relay`; relay sem `/p2p/<id>` é erro.
  - `kin-chat`: `Invite` (formato v1 manual, `kin://invite/<base64url>`, confere a assinatura ao ler),
    `Chat::invite` / `shareable_invite` / `accept`; `accept` fixa a identidade esperada por Peer ID e o
    `Expected` do P15 recusa quem apresentar outra. `kin-crypto`: `CryptoDevice::certificate()`.
  - `kin-cli`: `/invite` (link + QR com `qrcode`), `/accept`, `--accept`, `--relay-count`.
  - Testes: `chat/tests/invite.rs` (6: roundtrip, links danificados e byte a byte, aceitar conecta, **convite
    forjado que reivindica o Peer ID da Ana é recusado**, convite próprio, convite por circuito),
    `transport/tests/relay.rs` (+5: `dial_peer`, circuito adiado, `shareable_addresses`, reserva em N relays,
    failover, relay morto na partida) e unitários do `RelayBook`. Laboratório: cenários `invite` e
    `relay-failover` (5 cenários, estáveis).
  - **Achados**: (1) o cliente de relay registra o circuito como endereço *externo*, então o filtro de
    loopback tem de valer também para os externos; (2) no convite forjado, se coube à Ana convidar no MLS ela
    cria a conversa do lado dela e só a Bia (que fixou a identidade) recusa: estado de um lado só, mesma
    família do P16 (P18); (3) uma falha única do cenário `invite` na primeira subida a frio (não repetiu em
    8 execuções): o prazo do passo foi alargado.
- **Próximo passo**: Fase 3 (mailbox/offline). Antes de distribuir: teste manual entre redes reais, QUIC no
  laboratório (P17a), P16/P18, P13 (chave sem cifra). A Fase 2 só fecha de verdade com o teste em redes
  reais; no laboratório está provada.

### 2026-10-29 — Sessão 4

- **Objetivo**: o dono pediu para padronizar o fluxo entre os cinco projetos (anchor, kin, lume, truthid, warden) e "alinhar o máximo de coisas possíveis". Fonte das peças: pasta `padrao/` do repo `masterlxz/workspace` (privado), ADR 0005 de lá.
- **Decisões do dono**: ruleset `protect-main` igual em todos (PR com 0 aprovações, check `guard` obrigatório, sem force push, sem apagar a `main`, admin só via PR); `guard` genérico; template de PR do warden; secret scanning, push protection e alertas do Dependabot ligados; logs de sessão não são reordenados; CI de testes por stack fica para outra rodada.
- **Achado**: o local estava 19 commits atrás do remoto; o `kin` já tem código (workspace Rust, Fases 1 e 2) e **nenhuma proteção na `main`** nem CI. O levantamento tinha tratado o kin como "sem código" por causa disso; corrigido antes de mexer. O repositório também **mudou de dono**: `masterlxz/kin` virou `Arven-Labs/kin` (o GitHub redireciona; o `origin` local foi corrigido).
- **Feito** (PR #1, `chore/padrao-fluxo`): `.github/pull_request_template.md` e `.github/workflows/ci-private-files.yml` (job `guard`: barra segredo, chave e credencial rastreados). Nenhum código tocado.
- **Fica**: depois do merge e do `guard` verde na `main`, ligar o ruleset, os recursos de segurança e o `delete_branch_on_merge` (mudança de CI/permissões: uma confirmação do dono por repositório, §4). Ligar o ruleset antes deixaria todo PR esperando um check que nunca reporta. CI de testes do workspace Rust: outra rodada.
- **Não verificado**: CI do PR #1 (visto com `gh pr view`): `guard` verde (workflow `CI Private Files`); é o único workflow do repo. Estado `CLEAN` e `MERGEABLE`. Antes do PR, o `guard` também foi testado localmente (`padrao/test-guard.sh` no workspace, com fixtures) e contra os arquivos já rastreados do repo, sem nenhum acerto. **Ainda não verificado**: o ruleset e os recursos de segurança do GitHub, que só são ligados depois do merge.
