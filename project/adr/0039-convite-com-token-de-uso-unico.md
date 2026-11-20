# ADR 0039 — Convite com token de uso único e revogável (P18)

- **Status**: **aceito e implementado na biblioteca** (Sessão 35, 2026-11-19, PR #242); a CLI (`--require-invite` opt-in, `/invites`, `/revoke-invite`) e os cenários de lab chegam no PR #246; a admissão por identidade (PR 3a, #248) e o **padrão fechado** na CLI (PR 3b, #251: `--open-admission`, sem `--require-invite`) estão na seção "Atualização" no fim. Área de segurança (admissão, formato de fio, schema, parsing de entrada externa, AGENTS.md §4): auditado pelo `security-auditor` (0 críticos, 0 altos, 4 médios, 5 baixos; tabela no fim).
- **Relacionados**: ADR 0033 (link com validade assinada), ADR 0011 (decisão 5, frescor), D5 (contato por consentimento), P18, P16 (conversa de um lado só, fora daqui).

## Contexto

O link de convite (ADR 0033) tem validade assinada, mas é *bearer* e multiuso: não tem nonce nem destinatário, e **quem gera o link não confere quem entra**. Do lado de quem aceita, `expected` fixa a identidade exigida (`Chat::accept`); do lado de quem gerou, `on_hello` e `on_key_package` criavam a conversa com qualquer peer que conectasse (`Expected.identity = None`). Resultado: o link não podia ser revogado e valia para qualquer um por até 24 h (7 dias, se o emissor fosse malicioso). Quem cria o grupo MLS é o de **menor Peer ID**, não quem gerou o link, então a admissão tem de funcionar nos dois papéis.

## Decisão (dono, 2026-11-19)

1. **Link v3** (`kin://invite/`): `versão 3 | len16 certificado | n u8 | n × (len16 endereço) | token [u8; 16] | expires_at u64 (BE) | len16 assinatura`. O token (16 bytes de `rand::random`, CSPRNG) vai **dentro da parte assinada**; o byte de versão também, então um v3 não vira um v2 válido com o mesmo domínio `kin/invite/v2` (o `identity` não mudou). O v2 ainda é lido, sem token; o v1 segue recusado.
2. **Token de uso único**: gravado em `kin_invite_tokens (token, expires_at)` (**schema v9**, aditivo; um build antigo recusa o banco v9, como nas v3 a v8), **teto de 16 vivos** (vencidos são varridos ao contar; passou do teto, `Error::TooManyInvites`). Gastar é um `DELETE ... WHERE token = ? AND expires_at > ?` único (atômico), usado também para revogar.
3. **`Hello` leva o token**: `Envelope::Hello(Option<[u8; 16]>)`; payload de 0 bytes (sem token) ou de 16; qualquer outro tamanho é `Malformed`. `Chat::accept` guarda o token do link e o envia só ao peer do link, até a conversa ficar pronta.
4. **Portão de admissão** (`Chat::set_require_invite`, **padrão `false`**: a biblioteca continua aberta; mDNS e os testes não mudam): com ele ligado, peer **desconhecido** só abre conversa se apresentar um token **vivo**. Conhecido, e portanto isento: já tem conversa, está em `expected` (eu aceitei o link dele), ou é device de uma identidade confiável (`household`, `contact_devices`). O portão roda em `on_hello`, `on_key_package` e `on_welcome`, e **repete** a checagem de token vivo (outro peer pode ter gasto o token, ou eu o ter revogado, entre o `Hello` e o `Welcome`).
5. **O token é gasto em `ready`** (a conversa nasceu), não no `Hello`: um handshake que falha no meio não queima o link de quem convidei. Quem **já é contato** e apresenta um token vivo o gasta na hora (`ready` não roda de novo para ele). O uso único vale com o portão ligado ou não.
6. **`KeyPackage` antes do `Hello`**: o `Hello` com o token e o `KeyPackage` vão por substreams sem ordem garantida. Quando o `KeyPackage` chega primeiro e o peer ainda não está admitido, ele é **guardado** (até 16 KiB cada, até 64 peers) e processado quando o `Hello` com o token chegar; cai com a desconexão.
7. **Revogação**: `Chat::invites()` lista os vivos (`id` = 4 primeiros bytes do token em hex, que não revela o token) e `Chat::revoke_invite(prefixo)` (mínimo de 4 caracteres, maiúsculas valem; ambíguo é erro) apaga o token e tira da memória quem já o apresentara.
8. `Debug` de `Invite` e de `Envelope` escreve `<oculto>` no lugar do token.

## Alternativas descartadas

- **Pedido pendente com `/allow`** (confirmação de quem convida): segue o D5, mas quebra o encontro por mDNS e os testes atuais sem uma regra de exceção, e acrescenta um passo manual por conversa. Pode vir depois, em cima disto.
- **Token no `KeyPackage`** em vez do `Hello`: muda o formato do MLS KeyPackage e do `Envelope::KeyPackage`; o `Hello` já é uma mensagem de controle nossa.
- **Esperar o `Delivered` do `Hello` antes de mandar o `KeyPackage`**: resolve a ordem sem guardar nada, mas acrescenta estado e latência ao handshake. O `KeyPackage` guardado é menor.
- **Guardar o hash do token** (SHA-256) no banco: o `chat` não tem a dependência hoje, e o token só vale até ser usado; a coluna fica em claro no mesmo arquivo 0600 do histórico. Registrado como possível endurecimento.
- **Portão ligado por padrão na biblioteca**: quebraria mDNS e os testes; o aplicativo liga.

## Consequências e limites

- **Continua *bearer***: quem interceptar o link antes do convidado e entrar primeiro vence; o convidado legítimo falha de forma visível (token já gasto). O uso único reduz, não elimina.
- **Link v2 com o portão ligado** nunca passa (sem token): os v2 emitidos antes da troca morrem quando o portão liga (24 h de validade). Um app antigo que recebe um link v3 o recusa como `InvalidInvite`.
- **`Hello` com payload em nó antigo**: vira `Malformed` e `ChatEvent::Dropped`, sem pânico; só ocorre se um convidado v3 fala com um nó que não emite v3.
- **O token é comparado por consulta SQLite**, não em tempo constante: risco desprezível (128 bits, sem oráculo, ruído de rede muito acima), aceito.
- **O portão decide pelo Peer ID, antes de ver o certificado**: um device novo de identidade já conhecida (par recém-pareado, segundo device de um contato) que ainda não está em `household` nem `contact_devices` precisa de um token. **Não validado** com pareamento e multi-device; entra no lab do PR seguinte.
- **Em memória**: `admitted` e `held_key_packages` caem com a conexão (`PeerDisconnected`); o `Hello` seguinte apresenta o token de novo. Não verificado se o `PeerDisconnected` do nó dispara por conexão ou só quando a última conexão do peer cai (`max_established_per_peer` é 4).
- **CLI (PR #246, Sessão 35)**: `--require-invite` (**opt-in**, desligada por padrão), `/invites` e `/revoke-invite ID`. O **padrão fechado fica para um PR 3**: lendo o código, com o portão ligado `--dial` simples (`Chat::dial` não registra o peer como conhecido, então o próprio nó que disca não manda ou não aceita o `KeyPackage`) e o pareamento e o multi-device (o primário não conhece o Peer ID do device novo: `issued/` só está no disco e `household` vem de `DeviceList`, que exige conversa pronta) **não passam**. O conserto é de biblioteca: admitir por **identidade autenticada** (o certificado do `KeyPackage`/`Welcome`: minha identidade ou uma confiável passa sem token) e tratar como conhecido o peer que eu disquei; pede nova auditoria e a regressão dos cenários do lab como prova.
- **Fora desta fatia**: os endereços públicos no link (P18 b); a conversa de um lado só quando o convidado recusa a identidade (P18 c, família do P16).
- Quebra de API: `Chat::invite_link` agora é `&mut self` e devolve `Result`; consumidores fora deste repo (p. ex. `warden/mobile`) não foram conferidos.
- Relógio: `now_secs()` devolve 0 se o relógio falhar (token fica vivo no banco, mas o link sai vencido); relógio que volta no tempo pode reviver um token ainda não varrido. Não explorável à distância.

## Provas

- **Testes primeiro** (CI, run 44433226020): 13 dos 17 testes novos falharam com os stubs (`invites()` vazio, sem token no link, portão sem efeito), e 4 passaram de propósito (portão desligado, link v2, contato conhecido, `Hello` de tamanho errado). Depois da implementação (run 44434145874): tudo verde, 357 testes da biblioteca e 15 de `invite_token`.
- **Mutação de propósito** (PRs descartáveis #243 a #245, fechados sem mesclar): (1) `admits` sem rechecar o token vivo → falhou `two_peers_racing_for_one_token_leave_only_one_conversation`; (2) `ready` sem gastar o token → falharam 3 (`accepting_a_token_link_opens_the_conversation_and_spends_the_token`, `a_spent_link_cannot_be_used_by_someone_else`, `two_peers_racing_...`); (3) `>` virando `>=` na checagem de vencimento → falhou `an_invite_token_is_live_until_it_expires_and_can_be_taken_only_once`. Nota: só **um** teste pega a mutação 1; é o da corrida.

## Atualização: PR 3a, admissão por identidade autenticada (PR #248, 2026-11-19)

Decisões do dono (plan mode e perguntas durante o PR):

1. **O `Hello` leva o certificado do device** (`Envelope::Hello(token, certificado)`, tag 4: `tem token u8 | [token; 16] | certificado`). Quem recebe confere a cadeia (`verify`) e que `network_peer_id()` é o Peer ID da conexão (autenticado pelo libp2p); só então grava a identidade em `hello_identity` (teto 64, em memória). Motivo: o de **maior** Peer ID responde ao `Hello` com o `KeyPackage` e precisa decidir **antes** de ver o certificado do `KeyPackage`. A tag 0 (só token) ainda é lida.
2. **`admits` passa também** por (a) identidade autenticada **minha ou aceita por `trust`** (recalculada a cada chamada: `untrust` fecha), e (b) **quem eu disquei** por `Chat::dial` com `/p2p/<id>` terminando o endereço (`dialed`, teto 8, some no `ready`, no `DialFailed` e no `PeerDisconnected`). Endereço sem `/p2p/` não admite ninguém.
3. **O `KeyPackage` e o `Welcome` têm de ter a identidade do `Hello`** (`expected_identity`: a do convite aceito, senão a do `Hello`). Sem isso, quem tivesse as chaves de um device de T entrava pelo portão como T e abria a conversa com um certificado de outra identidade X (auditoria, médio 1). A revogação continua conferida no `credential::verify` do `invite`/`join`, sobre o certificado que vale.
4. **O certificado só vai no `Hello` a quem já conheço** (`knows`: conversa, `expected`, `household`, `contact_devices`, `dialed`). Estranho recebe o `Hello` sem identidade (auditoria, médio 2: o certificado expõe identidade e chave Nostr). Consequência: **device que me disca sem `/p2p/` não se identifica**, e um par que só se achou por mDNS (nenhum dos dois "conhece" o outro) não passa pelo portão por identidade; usa `--dial` com Peer ID ou link.

Alternativas descartadas: `KeyPackage` a qualquer um e barrar no `Welcome` (perde o efeito do portão, exige política dentro do `join`); registrar o Peer ID em `household` na emissão (não cobre device de contato); exigir link também no `--dial` (quebra o lab).

**Compatibilidade:** nó antigo que recebe a tag 4 a trata como `Malformed`; se for o de maior Peer ID, nunca responde com `KeyPackage`. Metade dos pares novo/antigo falha o handshake. Aceito (pré-lançamento, mesmo custo do schema v9).

**Limites conhecidos (auditoria, baixos 5 e informativo):** 64 conexões com certificados autoassinados esgotam `hello_identity` e o próximo `Hello` legítimo perde a vaga em silêncio (exige 64 conexões; não mitigado); o `trust` por repasse de irmão (`on_device_list`) é transitivo como antes e agora também abre o portão por identidade. **Não validado:** cadeia v4 (`Issuer::Authority`) no caminho do `Hello` (os testes usam certificado v3; o PR 3b cobre no lab com `paired-device`).

**Provas:** testes primeiro (CI, run 44526156174): 3 de 6 falharam; depois, 7 de 7 verdes. Testes da auditoria falharam antes da correção (run 44531149351: 4 falhas, 363 passando) e passam depois. Mutação de propósito (PRs descartáveis #249 e #250, fechados): `admits` aceitando qualquer identidade derrubou 11 testes e 2 cenários de lab; sem conferir o Peer ID do certificado derrubou `a_hello_certificate_of_another_peer_is_refused`.

## Atualização: PR 3b, CLI com o portão fechado por padrão (PR #251, 2026-11-19)

- **Padrão fechado**: `chat.set_require_invite(!open_admission)`. `--open-admission` abre mão do portão; `--require-invite` **sumiu** (flag desconhecida dá erro, com teste). A CLI imprime o modo na partida (aviso se aberto). Os cenários de NAT do lab (`setup()`) sobem o `a` com `--open-admission` porque testam transporte e o `b` disca sem link; `invite-*`, `multi-device`, `paired-device`, `revoke-device` e `history-sync` rodam com o portão **fechado**.
- **Achado do lab** (`revoke-device`): `Chat::accept` com o peer **já conectado** (o `c` subira com `--dial`) não reapresentava o `Hello` com o token, porque `dial_peer` não faz nada com a conexão existente. Corrigido: `accept`, com o peer conectado e sem conversa, reenvia o `Hello` e, se este nó tem o maior Peer ID, também o `KeyPackage` (o de maior ID só o manda ao receber o `Hello` do outro, que já passara recusado). Teste de integração nas duas ordens de Peer ID.
- **Cadeia v4 validada**: `paired-device` (certificado emitido pelo primário) passa pelo `Hello` com o portão fechado.
- **Auditoria** (`security-auditor`, 0 críticos, 0 altos, 0 médios, 3 baixos): baixo 1 (falha silenciosa do portão: aviso na partida) e baixo 2 (`accept` com estado meio aceito se `key_package()` falhar: erro vira `Dropped`) corrigidos; baixo 3 (`/accept` repetido gera `KeyPackage` extra, só por ação do usuário) aceito.
- **Falha silenciosa que continua**: estranho recusado não gera evento nenhum; só o aviso da partida diz que o portão está ligado. Um evento "peer conectou e não foi admitido" fica como melhoria.
- **Provas**: testes e lab primeiro (CI, 2 testes de parse e 10 cenários do lab falharam); teste do `accept` falhou antes da correção; mutação de propósito no PR descartável #252.

## Atualização: P34, evento "peer não admitido" (PR #257, 2026-11-19)

- **Decisão do dono** (plan mode): evento **sem motivo** (`admits` continua `bool`) e **um por conexão**. Alternativa descartada: enum de motivos (muda `admits` em 3 chamadores, e "token morto" viraria oráculo do estado do convite se o texto vazasse).
- **Onde**: `notify_not_admitted`, chamado em `on_hello` depois de `note_hello_identity`/`note_invite_token`. É o único ponto em que a recusa é definitiva (o `KeyPackage` pode chegar antes do `Hello`). Só avisa com `admits == Ok(false)`: erro de banco não é recusa. Nada é transmitido ao peer.
- **Limites aceitos** (auditoria do PR #257, baixos 2 a 4): o teto de 64 (`MAX_NOT_ADMITTED_NOTIFIED`) esgota com 64 conexões simultâneas de estranhos e então suprime o aviso, sem afetar a admissão; o set é limpo em `PeerDisconnected`, então reconexão em laço gera uma linha por ciclo; o aviso é um instantâneo e não é retratado se o peer for admitido depois.
- **Achado corrigido**: o texto da CLI citava `/trust`, que não abre o portão para estranho (`send_hello` só manda o certificado a quem já conheço).

## Atualização: P18 (b), o link não revela o IP de quem o gera (PR #270, 2026-11-20)

- **Decisão do dono** (plan mode): `/invite` leva **só endereços de circuito de relay** (o link mostra o IP do relay, não o do emissor); `/invite --direct` inclui também o IP direto (`--external-addr`, escuta de IP global). Alternativa descartada: manter tudo e só avisar (não resolve o vazamento).
- **Onde**: `Node::shareable_circuit_addresses` (filtro `only_circuit`: circuito e não loopback), `Chat::shareable_invite_via_relay`, `parse_invite` e `InviteScope` na CLI. Sem mudança de formato de fio nem de schema; `Chat::invite_link` e `shareable_invite` não mudaram (sem quebra de API).
- **Fora**: `own_invite`/`DeviceList` (vai dentro da conversa MLS a quem já aceitei, não é link *bearer*) continua com todos os endereços.
- **Consequência**: sem reserva em relay o `/invite` não gera link até ela existir; quem roda com `--external-addr` e sem relay precisa de `/invite --direct`. O IP direto volta a aparecer só para quem já é par (DCUtR) ou por escolha explícita.
- **Provas**: testes primeiro (CI, run 44826514388): os 2 testes novos falharam (stubs inertes) e os demais passaram; depois da implementação, CI e `Lab` (17 cenários) verdes no commit `c6cdfd5`. **Mutação de propósito** (PR descartável #271, fechado): filtro sem exigir circuito → só `only_circuit_addresses_survive_for_the_invite_link` falhou (run 44828007463).
- **Auditoria** (`security-auditor`, somente leitura: 0 críticos, 0 altos): (1, baixo) o filtro é sintático: relay em IP privado de LAN ou em hostname `localhost` passa, e quem roda o próprio relay em casa mostra o IP dele no link; **aceito**, inerente ao circuito, documentado aqui. (2, fora do escopo, médio) `own_invite`/`DeviceList` mandam o IP direto a irmãos e a contatos confiáveis dentro da conversa MLS (`set_advertised_addrs` limita). (3, fora do escopo, médio, confiança média) o Identify do libp2p entrega `listen_addrs` e o endereço observado a todo peer conectado, inclusive estranhos: o IP direto sai por aí mesmo sem `--direct`; não verificado no código do libp2p. (4, baixo, antigo) `/invite--direct` ou com tab cai como mensagem ao peer atual. 2 e 3 ficam registrados no P18 do `PENDING.md`; 4 aceito.
- **Não verificado**: nó real com relay e `external_addresses` (o teste usa endereços sintéticos; o `Lab` só prova que o fluxo de convite segue funcionando, não o conteúdo do link).

## Auditoria (`security-auditor`, somente leitura)

| # | Achado | Gravidade | Destino |
|---|---|---|---|
| 1 | Erro em `take_invite_token` dentro de `ready` (com `?`) deixava a conversa pela metade e o token vivo | médio | **corrigido**: o erro vira `Dropped`, o resto do `ready` segue |
| 2 | Teto de 8 entradas: DoS barato do `KeyPackage` guardado; `hello_tokens` perdia o token em silêncio no 9º `accept` | médio | **corrigido**: teto próprio de 64 para os guardados; `hello_tokens` despeja uma entrada em vez de perder a nova |
| 3 | O portão não está ligado em nenhum aplicativo; a CLI não lista nem revoga | médio | **PR 2** (CLI, padrão fechado); registrado acima |
| 4 | Device novo de identidade conhecida não passa pelo portão (Peer ID antes do certificado) | médio | **registrado**; validar no lab do PR 2 |
| 5 | Quem já é contato apresenta o token e ele fica vivo | baixo | **corrigido**: gasta na hora |
| 6 | `admits` com erro de banco perdia o `KeyPackage` | baixo | **corrigido**: erro conta como "não admitido" e guarda |
| 7 | Token vazaria por `Debug` futuro; coluna em claro | baixo | `Debug` **corrigido**; coluna em claro **aceita** (hash é endurecimento possível) |
| 8 | `invites()` esconde erro; prefixo curto ou maiúsculo em `revoke_invite` | baixo | prefixo mínimo de 4 e `to_ascii_lowercase` **corrigidos**; `invites()` segue devolvendo lista vazia no erro (**aceito**, mudaria a API) |
| 9 | Relógio (`now_secs() == 0`, relógio que volta) | baixo | **aceito**, registrado |
| — | Comparação não constante; compatibilidade com nó antigo; v2 com portão; *bearer* | informativo | registrados acima |

O auditor **não** achou bypass do portão: `on_hello`, `on_key_package` e `on_welcome` passam por `admits`; mDNS e `dial` só conectam; o mailbox só aceita `Envelope::Mls`; dois peers com o mesmo token se resolvem no primeiro `ready`. Não validou: o pareamento e o multi-device com o portão ligado, o tamanho real de um `KeyPackage` v4 frente ao teto de 16 KiB, e o corpo completo de `migrate`.

## Atualização: P18 (d), o convidante confirma quem entra (biblioteca, PR #274, 2026-11-20)

- **Decisão do dono**: pedido pendente com confirmação (opção C), como **segundo opt-in** (`Chat::set_require_approval`, padrão `false`), sem mudar o fio nem o schema. Antes tinha sido descartado por quebrar mDNS e testes; com o portão fechado por padrão na CLI e a biblioteca aberta, só o caminho "token vivo, estranho" muda.
- **Correção de premissa** (minha, no desenho): eu disse que a identidade de quem usa o link só aparece no `KeyPackage`. Não: quem aceitou um link tem o dono em `expected`, e `send_hello` manda o certificado a quem está em `expected`. O `Hello` com token já traz a identidade (`hello_identity`).
- **Como funciona**: com portão e aprovação ligados, `on_hello` com token vivo e identidade grava o pedido (`pending_approval`, teto 16) e emite `ChatEvent::AdmissionRequested { peer, identity }` (uma vez por conexão, no lugar do `NotAdmitted`). `admits` só aceita o token se o peer está em `approved`. `Chat::approve` retoma o handshake nos dois papéis: de **menor** Peer ID, processa o `KeyPackage` guardado; de **maior**, manda o próprio `KeyPackage` (que não saiu enquanto esperava). `Chat::deny` descarta o `KeyPackage` e não pergunta de novo na conexão. O token só é gasto em `ready`, então pendente ou negado não queima o link. Estado em memória; some com a conexão.
- **Premissa minha, não confirmada**: depois de `deny` o token segue vivo (o convidado pode tentar de novo; revogar é `revoke_invite`). A alternativa "negar também revoga" ficou em aberto para o dono.
- **Provas**: testes primeiro (CI, run 44835564573): os 3 de comportamento falharam e os 3 guardas passaram; depois da implementação, CI e `Lab` verdes (commit `99099a6`). Mutação de propósito (PR descartável #275, fechado): `admits` ignorando `approved` derrubou 3 testes (run 44836905982).
- **Auditoria** (`security-auditor`: 0 críticos, 0 altos): (baixo 1) um 2º `Hello` com outro certificado trocava a identidade mostrada → **corrigido** (identidade fixa com pedido pendente ou aprovado); (baixo 2) a aprovação valia pelo peer, não pelo token → **corrigido** (token fixo); (médio) 16 pedidos de quem tem o link podem esconder o convidado real → **mitigado**: `revoke_invite` leva os pedidos do link junto; o teto em si fica **aceito** (não dá entrada, só ruído); (baixo 3) ligar `set_require_approval` com peers já em `admitted` não gera pedido até o próximo `Hello` → **documentado**: chamar antes de conectar. Os três corrigidos têm teste que falhou antes (CI, run 44837161738).
- **Não verificado**: o caminho do mailbox (o auditor não o leu); se o nó emite `PeerDisconnected` por conexão ou só na última; o teto de 16 sem teste; a CLI e o `Lab` (PR 2).
