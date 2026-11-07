# Log de Sessões

> **Nota**: Este log foi criado junto com o projeto. As sessões serão registradas aqui conforme o trabalho avança.
>
> Última atualização: 2026-11-05 (Sessão 7)

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

---

### 2026-11-05 — Sessão 5

- **Objetivo**: P9 (ordenação de commits MLS sem Delivery Service), recorte 1:1 + ADR geral.
- **Decisões do dono**: escopo "1:1 + ADR geral" (multi-device e grupos só no ADR); concorrência por
  **committer designado** (descartados: fork com rollback; curar por re-entrada). Aprovou os testes pela
  tabela de comportamentos (Etapa D) e a criação do ADR e da branch.
- **Feito** (branch `p9-committer-e-buffer`):
  - `crypto`: `is_committer` (menor índice de folha), `propose_rotation`, `commit_pending`; `rotate_keys` e
    `commit_pending` só do committer (`NotCommitter`); `decrypt` guarda propostas `Update`
    (`Decrypted::ProposalQueued`) e continua recusando `Add`/`Remove` soltos.
  - `chat`: buffer de mensagens de época futura por peer (teto 32, sem repetidas), reprocessado na ordem de
    chegada a cada commit; estouro emite `ChatEvent::Stale` e descarta o buffer. `cli` trata o evento novo.
  - ADR 0001 (`project/adr/`), P9 reescrito em `PENDING.md`, ponteiro em `ARCHITECTURE.md` (D2).
- **Achados**: (1) a API de proposals do `openmls` 0.9.0 (`propose_self_update`, `store_pending_proposal`,
  `commit_to_pending_proposals`) existia como previsto (conferida no código do crate); (2) a mutação "remover a
  deduplicação do buffer" **não era pega** pelo teste original, porque a cópia repetida vira `Duplicate` no
  MLS; o teste foi reescrito para exigir que reentrega não gaste vagas do teto, e a mutação passou a ser pega.
- **Verificado (local)**: `cargo test --workspace` sem falhas (+8 testes novos; antes de implementar, 7 dos 8
  falhavam por asserção), `cargo clippy --workspace --all-targets` sem avisos, `cargo fmt --check` ok.
  Mutações: teto `>=`→`>` (pega), committer sempre verdadeiro (pega por 2 testes), deduplicação removida
  (pega depois do ajuste); todas revertidas.
- **CI**: PR #2, `guard` verde (visto com `gh pr view`), `CLEAN` e `MERGEABLE`. É o único workflow do repo.
- **Não verificado**: testes Rust no CI (não rodam lá, só localmente); fluxo proposta→commit
  com banco SQLite reaberto (`Chat::open`); teto de 32 sem medição com mailbox real.
- **Próximo passo**: Fase 3 (mailbox/offline). P9 segue aberto para multi-device (3.5) e grupos; P16 recebe a
  recuperação depois de `Stale`.

---

### 2026-11-05 — Sessão 6

- **Objetivo**: Fase 3, etapa 3.2 (outbox local). Plano em dois PRs: este (outbox) e depois 3.1 (trait de
  mailbox + backend em memória).
- **Decisões do dono**: escopo "3.1 + 3.2 juntos" em dois PRs; outbox guarda o texto **em claro** e cifra ao
  enviar; **um envio por vez** (janela 1) para garantir a ordem; aceitou sem reenvio periódico (P19) e o
  esquecimento de ids vistos depois de 1024 por peer. Aprovou os testes pela tabela e o ADR 0002.
- **Feito** (branch `fase3-outbox`):
  - `chat/src/outbox.rs`: SQLite próprio (`<banco>.outbox.db`, 0600) com a fila e os ids vistos.
  - `chat`: `send` enfileira e tenta enviar; sai ao conectar e ao ficar pronta a conversa; ack apaga da fila;
    falha de envio mantém na fila (`ChatEvent::Queued`); o receptor deduplica por `MessageId`. `cli` trata `Queued`.
  - ADR 0002; P19 e P20 em `PENDING.md`; 3.2 marcada em `PHASE.md`; Fase 3 "em andamento" em `OVERVIEW.md`.
- **Achados**: (1) o teste de ponta a ponta que eu escrevi prometia "na ordem", mas com janela de 8 a rede
  embaralha (`["dois","um","tres"]` na rodada 142 de 150); o erro foi do meu teste/desenho, achado repetindo o
  teste em laço. Corrigido com janela 1 (0 falhas em 100 rodadas depois); (2) re-cifrar a cada tentativa gera
  bytes diferentes, então só o `MessageId` identifica a repetição.
- **Verificado (local)**: `cargo test --workspace` 86 passando, 0 falhando; `clippy` sem avisos; `fmt` ok.
  Mutações (janela `>=`→`>`, dedupe desligada, ack sem apagar da fila, erro de um no teto de ids vistos): todas
  pegas e revertidas.
- **CI**: PR #3, `guard` verde (visto com `gh pr view`), `CLEAN` e `MERGEABLE`. É o único workflow do repo.
- **Não verificado**: testes Rust no CI (não rodam lá); rede real entre máquinas; custo de esvaziar fila grande (uma ida e volta por mensagem);
  permissão 0600 do `outbox.db` sem teste; reenvio quando o envio falha com a conexão aberta (P19).
- **Próximo passo**: PR 2 (3.1): trait de mailbox + backend em memória + `ChatEvent::Deposited`.

---

### 2026-11-05 — Sessão 7

- **Objetivo**: Fase 3, etapa 3.1 (interface de mailbox), PR 2 de 2 depois da outbox (PR #3).
- **Decisões do dono**: interface fica só com o store-and-forward (o envio direto libp2p fica fora); aceitou o
  depósito recusado repetido só no próximo gatilho (sem temporizador) e o teto de 8 depositadas por peer.
  Aprovou os testes pela tabela e o ADR 0003.
- **Feito** (branch `fase3-mailbox`):
  - `chat/src/mailbox.rs`: trait `Mailbox` (`attach` com canal de eventos, `deposit`), `MailboxEvent` e o backend
    de referência em memória (`MemoryRelay`, `MemoryMailbox`), públicos.
  - `chat`: `set_mailbox`; sem conexão, `flush` cifra e deposita (teto 8, marca persistente em `kin_deposited`
    gravada só na confirmação); `next_event` escuta rede e mailbox; `Retrieved` entra pelo mesmo caminho do envio
    direto, só se for mensagem MLS; `ChatEvent::Deposited` e `DepositFailed`; queda do peer manda a fila para a
    caixa postal. `cli` trata os eventos novos. Helpers de teste (`Person`, `connect`...) foram para `tests/common`.
  - ADR 0003; P21 em `PENDING.md`; 3.1 marcada em `PHASE.md`; `OVERVIEW.md`.
- **Achados**: (1) a mutação "aceitar handshake pela caixa postal" **sobreviveu** a todos os testes: a regra que
  evita contornar a amarração ao Peer ID (P15) não estava protegida. Escrevi
  `a_handshake_envelope_from_the_mailbox_is_dropped` (ele entrou depois da tabela aprovada) e a mutação passou a ser
  pega; (2) a confirmação do depósito sem gravar só é pega pelo teste de reinício, não pelos unitários.
- **Verificado (local)**: `cargo test --workspace` 95 passando, 0 falhando; `clippy` sem avisos; `fmt` ok; os testes
  de ponta a ponta (mailbox e outbox) passaram 100 de 100 rodadas. Mutações (teto, marca sem gravar, dedupe
  desligada, handshake aceito): todas pegas e revertidas.
- **CI**: PR #4, `guard` verde (visto com `gh pr view`), `CLEAN` e `MERGEABLE`. É o único workflow do repo.
- **Não verificado**: testes Rust no CI (não rodam lá); backend assíncrono real (cancel-safety, ordem, starvation sob carga); `Welcome` e
  `KeyPackage` um a um pela caixa postal (o teste cobre `Hello`, mesmo ramo do código); varredura inicial (P21).
- **Próximo passo**: 3.3, backend Nostr (NIP-17/59), que precisa antes de um spike: tamanho aceito pelos relays
  para mensagens MLS e a chave secp256k1 do destinatário.

---

### 2026-11-06 — Sessão 8

- **Objetivo**: triagem dos alertas do Dependabot (`/dependencias kin`) e correção do que cabe só no lockfile.
- **Alertas antes**: 4 abertos (#1 a #4), todos `rustls` 0.23.45, runtime, `Cargo.lock`, corrigido na 0.23.46.
  Quem puxa: `libp2p-quic` (via `quinn`/`quinn-proto`) e `libp2p-tls`; nenhum crate do kin usa `rustls` direto.
- **Classificação**: #1 (Brotli) **não alcançável**: `brotli` não está no `Cargo.lock` e o decoder vem desativado.
  #2 (`Debug` da sessão), #3 (grupo de troca de chaves envenenado) e #4 (ECH rejeitado): **não verificados**.
  O `libp2p-tls` 0.7.0 não menciona `ClientSessionStore` nem ECH, mas isso é indício, não prova. Nenhum foi dispensado.
- **Feito** (branch `deps-rustls-0.23.46`): `cargo update -p rustls --precise 0.23.46`. O diff do lockfile mexe só no
  `rustls` (versão e checksum) e na escolha de `getrandom` dele (0.4.3 → 0.3.4, que já estava no lockfile).
- **Achado**: `cargo audit` lista 5 vulnerabilidades e 3 avisos de manutenção que o Dependabot não mostra. Registrados
  no P22, sem corrigir (fora do escopo deste PR, tocam criptografia).
- **Verificado (local)**: `cargo test --workspace --locked` sem falhas; `clippy` sem avisos; `fmt` ok.
- **Alertas depois**: 0 abertos, relido da API depois do merge do PR #5 (os 4 com `state=fixed`, 2026-11-06 12:42 UTC).
- **Não verificado**: alcançabilidade dos itens do P22; testes Rust no CI (não rodam lá); redes reais.
- **Próximo passo**: reler os alertas na `main` depois do merge; classificar o P22; depois o spike da 3.3.
- **P22 (segunda rodada, branch `deps-lockfile-p22`)**: `cargo update -p rustls-webpki -p ctutils -p tokio`
  (0.103.16, 0.4.8, 1.54.1). O diff mexe só nesses três pacotes. `cargo test --workspace --locked`: 95 passando,
  0 falhando; `clippy` sem avisos; `fmt` ok. `cargo audit` caiu de 5 vulnerabilidades para 3 (`crypto-bigint` e os
  dois do `libcrux-kem`) e 2 avisos de manutenção. O `libcrux-kem` não é compilado (só lockfile); o `crypto-bigint`
  espera o upstream. Detalhes e evidências no P22.
- **Não verificado (P22)**: se `p256`/`ml-dsa` chegam a executar (o `Ciphersuite` fixo é Ed25519/X25519, mas não li o
  caminho dentro do `openmls`); `tokio::AsyncFd` dentro do `hickory-net`; testes Rust no CI (não rodam lá).
- **Spike da 3.3 (só leitura; PR de docs `docs-spike-3-3`)**: o dono limitou a medição a NIP-11 (nada foi
  publicado em relay). Tamanhos medidos com um teste temporário em `crypto`, apagado depois (`git status` limpo).
  Resultado no ADR 0004 (proposto): mensagem MLS de app = texto + 145 B; o teto prático (~30 KB) vem do NIP-44
  (64 KiB por camada, duas camadas no gift wrap), não dos relays (≥128 KB declarados). Nenhum relay da amostra
  anuncia NIP-17; só o damus anuncia NIP-59. Proposta: chave secp256k1 própria no `DeviceCertificate` v3.
- **Não verificado (spike)**: tamanhos do evento são estimativa, lista de relays é amostra.
- **Spike de escrita (autorizado pelo dono)**: 6 eventos kind 1059 por relay (payload aleatório, chaves descartáveis,
  script no scratchpad, nada no repo) em `nos.lol` e `offchain.pub`; o `relay.damus.io` respondeu HTTP 503. Achados:
  `nos.lol` aceitou até 32 KB; `offchain.pub` recusou 16 e 32 KB por web of trust (a chave de wrap real também é nova a
  cada mensagem); NIP-40 honrado nos dois; leitura sem autenticação nos dois e com NIP-42 obrigatório no damus.
  Consequência: falha por relay é normal, o backend precisa de vários relays e de NIP-42. Detalhes no ADR 0004.
- **Não verificado (escrita)**: retenção além de 150 s; escrita e leitura autenticada no damus; relays pagos; o
  ciphertext é aleatório, não NIP-44 real. Ficaram 4 eventos no `nos.lol` e 3 no `offchain.pub` (chave descartável,
  não dá para apagar).
- **Certificado v3 (branch `cert-v3-nostr-key`)**: o dono confirmou a opção B e a dependência `secp256k1` 0.31.1
  (libsecp256k1), em vez de `k256` (que usa o `crypto-bigint` 0.5.5 do P22). `DeviceKey` guarda a chave de rede e a
  Schnorr (formato do arquivo versionado, antigos recusados); `DeviceCertificate` v3 cobre a chave Nostr no payload
  assinado; `NostrPublicKey` só existe se for ponto válido da curva; `CryptoDevice::new/open` recebem a chave Nostr e
  falham com `DeviceMismatch` se o banco guarda outra. Testes novos (9 comportamentos, tabela aprovada pelo dono):
  vermelhos por não compilarem antes (14 e 7 erros), verdes depois.
- **Verificado (local)**: `cargo test --workspace --locked` 102 passando, 0 falhando (eram 95); `clippy` sem avisos;
  `fmt` ok; `cargo audit` sem vulnerabilidade nova (os mesmos 5 itens do P22). Mutações pegas e revertidas: chave Nostr
  fora do payload assinado (falha `swapping_the_nostr_key_...`) e `open` sem conferir a chave Nostr (falha
  `the_nostr_key_is_certified_...`).
- **Achados**: a dependência nova trouxe 9 pacotes ao lockfile (`secp256k1-sys`, `bitcoin_hashes`, `bitcoin-io`,
  `bitcoin-consensus-encoding`, `hex-conservative` x2, `arrayvec`...), e o `tempfile` passou a usar `getrandom` 0.4.3
  (versão que já estava no lockfile). A API `from_slice` da `secp256k1` está depreciada; usei `from_byte_array`.
- **Não verificado**: testes Rust no CI (não rodam lá); a proteção da chave secreta Nostr em repouso (P13); nenhum
  código Nostr (assinatura, NIP-44) existe ainda; a `libsecp256k1` em C não foi auditada por mim.
- **Próximo passo**: backend `NostrMailbox` (NIP-17/59, NIP-42, vários relays), em PR próprio, com plano e testes antes.
- **Plano do backend (aprovado pelo dono)**: crate `nostr` (só protocolo, NIP-44/59/17/42) + `tokio-tungstenite`, em
  vez de `nostr-sdk`; PRs em sequência: A alinhar `secp256k1` a 0.30, B camada de protocolo sem rede (`wrap`/`unwrap`,
  teto de 30 KB, `Mailbox::learn_peer`), C websocket + vários relays + NIP-42 com relay falso nos testes, D CLI.
- **PR A (branch `secp256k1-0.30`)**: `secp256k1` `=0.31.1` → `=0.30.0` (o `nostr` 0.45 exige ^0.30). Três chamadas
  ajustadas em `identity/src/device.rs` (`thread_rng` de `rand` 0.8, `from_byte_array(&array)`, `from_seckey_slice`);
  sem mudança de comportamento nem de formato. `cargo test --workspace --locked` 102 passando sem editar nenhum teste;
  `clippy`, `fmt` ok; sem `secp256k1` duplicado; `cargo audit` com os mesmos 5 itens do P22. Efeito: o lockfile ganha
  `rand` 0.8.8 ao lado do 0.9.5. Não verificado: testes Rust no CI (não rodam lá).
- **PR B (branch `nostr-wrap-protocol`)**: camada de protocolo do backend Nostr, sem rede. `kin_chat::nostr` (`wrap`,
  `unwrap`, `Unwrapped`, `WrapError`, `MAX_BLOB` = 30.000): gift wrap kind 1059 → seal → rumor kind 14 com o blob em
  base64; o `unwrap` herda do crate `nostr` as conferências de kind, assinaturas e autor do rumor = autor do seal, e
  exige ainda rumor kind 14, base64 válido, blob ≤ teto e `content` do wrap ≤ 256 KiB antes de decifrar.
  `DeviceKey::nostr_secret_bytes`, `Conversation::peer_nostr_keys`, `Mailbox::learn_peer` (padrão vazio) e o `Chat`
  ensina a caixa postal ao fechar o handshake e ao ligá-la numa conversa recarregada. Dependência `nostr =0.45.7`
  (`std`, `nip59`, `os-rng`): 13 diretas, 18 entradas novas no lockfile, `rand` 0.10 (terceira versão do `rand`).
- **Achado na leitura do crate**: o `nostr` 0.45.7 estende o NIP-44 com prefixo de tamanho maior acima de 65.535
  bytes. O teto de ~30 KB do ADR 0004 deixa de ser limite do protocolo e vira política nossa (ADR corrigido). O
  crate sorteia o `created_at` do wrap até 2 dias no passado; um TTL (3.4) só faz sentido acima de 2 dias.
- **Verificado (local)**: testes novos escritos antes (não compilavam: 22, 1 e 2 erros) e verdes depois; `cargo test
  --workspace --locked` 117 passando, 0 falhando (eram 102); `clippy`, `fmt` ok; `cargo audit` com os mesmos 5 itens do
  P22. Mutações pegas e revertidas: teto `>=`, rumor kind 14 não exigido, `content` do wrap sem teto antes de
  decifrar, `learn_peer` ausente ao recarregar e ausente ao fechar o handshake.
- **Não verificado**: testes Rust no CI (não rodam lá); a cripto NIP-44 é a do crate `nostr`, sem revisão minha; nenhum
  relay real foi usado neste PR (sem rede); o crate `nostr` aceita eventos hostis só até onde os testes exercitam.
- **Próximo passo**: PR C, `NostrMailbox` com websocket (`tokio-tungstenite`), vários relays e NIP-42, com plano
  próprio (decisões abertas: lista padrão de relays, `since`, reconexão e backoff, `Deposited` com 1 de N relays).
- **PR C (branch `nostr-relay-mailbox`)**: `NostrMailbox` sobre `tokio-tungstenite` 0.30.0 (`rustls-tls-webpki-roots`).
  Decisões do dono: relays só por configuração (sem lista embutida), `Deposited` quando ≥1 relay aceita
  (`DepositFailed` só se todos falham), `since` = agora − 3 dias a cada conexão, sem persistir. Um worker tokio por
  relay (`nostr/relay.rs`): `REQ` kind 1059 `#p` próprio, `OK` com prazo (10 s), `AUTH` NIP-42 com a chave Nostr do
  device, reconexão com espera de 1 s a 60 s (jitter ±20%), mensagem máxima de 1 MiB. `nostr/mailbox.rs`: diretório de
  pares nos dois sentidos, dedupe de 4096 ids, `ws://` só para loopback. Relay falso de teste em
  `tests/common/fake_relay.rs`.
- **Bug achado pelo teste fim a fim**: `Chat::set_mailbox` ligava o backend (`attach`) antes de ensinar os pares; num
  runtime multi-thread o worker já recebia o evento com o remetente "desconhecido" e o descartava. Agora ensina antes
  de ligar. Os testes de unidade não pegavam; o teste com dois `Chat` reais pegou.
- **Verificado (local)**: 16 testes novos escritos antes (não compilavam) e verdes depois; `cargo test --workspace
  --locked` 133 passando, 0 falhando (eram 117); estáveis em 15 rodadas e em 8 sob carga de CPU; `clippy`, `fmt` ok;
  `cargo audit` com os mesmos 5 itens do P22. Mutações pegas e revertidas: dedupe desligado, remetente desconhecido
  aceito, `Deposited` só com todos os relays, `since` = agora, `AUTH` ignorado. Lockfile: 6 entradas novas
  (`tokio-tungstenite`, `tungstenite`, `tokio-rustls`, `webpki-roots` x2, `sha1`); o provider do `rustls` segue só
  `aws-lc-rs`.
- **Não verificado**: testes Rust no CI (não rodam lá); **nenhum relay real nem `wss://` com certificado válido**
  (só `ws://` em loopback e um servidor que não fala TLS); a cripto é a do crate `nostr`; limites no P23.
- **Próximo passo**: PR D, flag `--nostr-relay` na CLI e roteiro com relay real (fecha o P23(f)); depois 3.4.

### 2026-11-06 — Sessão 9

- **Objetivo**: PR D da 3.3, flag `--nostr-relay` na CLI e roteiro com relay real (P23(f)).
- **Feito** (branch `cli-nostr-relay`): `--nostr-relay URL` (repetível) em `crates/cli/src/main.rs` liga
  `NostrMailbox` via `Chat::set_mailbox`; `ws://` remoto é recusado na partida. A CLI deixou de esquecer a conversa
  atual ao desconectar (`*current = None` removido), senão não dava para enviar a quem saiu.
- **Roteiro real (autorizado pelo dono)**: A e B (diretórios temporários, TCP loopback) fecham a conversa; A é
  encerrado; B envia `oi-offline-1` → `(na fila…)` e `(depositada na caixa postal…)`; B é encerrado; A reinicia só com
  os relays e recebe `[f4a79c78] peer: oi-offline-1`. Relays: `wss://nos.lol` e `wss://relay.damus.io`. Uma
  tentativa anterior foi recusada pela CLI (`sem conversa pronta`, bug abaixo), então só 1 mensagem foi enviada aos relays.
- **Achado**: o bug do `current` acima só apareceu no roteiro real, não nos testes.
- **Verificado (local)**: clippy, fmt, `cargo test --workspace --locked` sem falhas.
- **Não verificado**: qual relay aceitou (CLI não imprime por relay), logo NIP-42 do damus não isolado; só 1 mensagem;
  sem teste automatizado da CLI; testes Rust não rodam no CI.
- **Próximo passo**: 3.4 (replicação em N mailboxes com TTL) ou imprimir resultado por relay.

### 2026-11-06 — Sessão 10

- **Objetivo**: P23(g), resultado do depósito por relay (escopo escolhido pelo dono: só o item g).
- **Feito** (branch `relay-outcome`): `MailboxEvent::RelayOutcome { id, relay, accepted, reason }` emitido por
  `Shared::ack` a cada resposta de relay (inclusive desconectado, sem resposta a tempo e worker encerrado);
  `ChatEvent::MailboxRelay`; a CLI imprime `(relay URL: aceitou)` ou `(relay URL: recusou: motivo)`. `Deposited` e
  `DepositFailed` não mudaram. O motivo vem do relay (hostil): vira uma linha de no máximo 200 caracteres, sem
  caracteres de controle (`one_line`).
- **Testes**: 5 novos escritos antes (3 erros de compilação por falta das variantes), verdes depois. O helper `next` dos
  testes antigos passou a pular `RelayOutcome`; nenhum assert antigo mudou.
- **Verificado (local)**: `cargo test --workspace --locked` 138 passando, 0 falhando (eram 133); clippy e fmt ok.
  Mutações pegas e revertidas: `accepted` sempre verdadeiro (3 testes falham) e motivo sem `one_line` (1 teste falha).
- **Não verificado**: contra relay real (ainda não rodei o roteiro com a saída nova); CI (testes Rust não rodam lá);
  o evento não liga o resultado ao `MessageId` (só ao relay).
- **Próximo passo**: rodar o roteiro real para ver o resultado por relay (publica em relays públicos, pede
  autorização) e depois 3.4.

### 2026-11-07 — Sessão 11

- **Objetivo**: rodar o roteiro real da Sessão 9 com a saída por relay (P23(f) e (g)), autorizado pelo dono.
- **Roteiro** (A e B em diretórios temporários, TCP loopback; relays `wss://nos.lol` e `wss://relay.damus.io`): conversa
  fechada; A encerrado; B envia `oi-offline-relay-outcome`; B encerrado; A reinicia só com os relays.
- **Resultado**: `(relay wss://relay.damus.io: aceitou)`, `(depositada na caixa postal: c8740a22)` e
  `(relay wss://nos.lol: recusou: o relay não respondeu a tempo)`. O A reiniciado recebeu
  `[c8740a22] peer: oi-offline-relay-outcome`. O resultado por relay aparece com relay real (P23(g) confirmado).
- **Não verificado**: uma única tentativa; se o timeout do nos.lol é lentidão passageira ou limite do relay (na
  Sessão 9 ele funcionou); qual relay entregou a mensagem; se o damus exigiu NIP-42.
- **Próximo passo**: investigar o timeout do nos.lol (valor do timeout no `NostrMailbox`, repetir o roteiro) e depois 3.4.
