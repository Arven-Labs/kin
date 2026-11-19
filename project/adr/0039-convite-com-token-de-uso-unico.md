# ADR 0039 — Convite com token de uso único e revogável (P18)

- **Status**: **aceito e implementado na biblioteca** (Sessão 35, 2026-11-19, PR #242); a CLI (padrão fechado, `/invites`, `/revoke-invite`) e os cenários de lab ficam para o PR seguinte. Área de segurança (admissão, formato de fio, schema, parsing de entrada externa, AGENTS.md §4): auditado pelo `security-auditor` (0 críticos, 0 altos, 4 médios, 5 baixos; tabela no fim).
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
- **Fora desta fatia**: a CLI hoje **não** liga o portão nem lista/revoga convites (cada `/invite` grava um token de 24 h; com 16 seguidos o 17º falha até algum vencer); os endereços públicos no link (P18 b); a conversa de um lado só quando o convidado recusa a identidade (P18 c, família do P16).
- Quebra de API: `Chat::invite_link` agora é `&mut self` e devolve `Result`; consumidores fora deste repo (p. ex. `warden/mobile`) não foram conferidos.
- Relógio: `now_secs()` devolve 0 se o relógio falhar (token fica vivo no banco, mas o link sai vencido); relógio que volta no tempo pode reviver um token ainda não varrido. Não explorável à distância.

## Provas

- **Testes primeiro** (CI, run 44433226020): 13 dos 17 testes novos falharam com os stubs (`invites()` vazio, sem token no link, portão sem efeito), e 4 passaram de propósito (portão desligado, link v2, contato conhecido, `Hello` de tamanho errado). Depois da implementação (run 44434145874): tudo verde, 357 testes da biblioteca e 15 de `invite_token`.
- **Mutação de propósito** (PRs descartáveis #243 a #245, fechados sem mesclar): (1) `admits` sem rechecar o token vivo → falhou `two_peers_racing_for_one_token_leave_only_one_conversation`; (2) `ready` sem gastar o token → falharam 3 (`accepting_a_token_link_opens_the_conversation_and_spends_the_token`, `a_spent_link_cannot_be_used_by_someone_else`, `two_peers_racing_...`); (3) `>` virando `>=` na checagem de vencimento → falhou `an_invite_token_is_live_until_it_expires_and_can_be_taken_only_once`. Nota: só **um** teste pega a mutação 1; é o da corrida.

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
