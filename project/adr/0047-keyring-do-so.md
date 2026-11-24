# ADR 0047: keyring do SO como fonte da senha das chaves (`--keyring`)

Status: aceito (P13, Sessão 46).

## Contexto
A senha das chaves (ADR 0012) vinha de prompt ou `--passphrase-file`. No desktop isso é digitar a cada partida, ou deixar um arquivo de senha em texto no disco, que anula parte da cifra. O ADR 0012 (l.38) deixou o keyring do SO "como backend opcional futuro, atrás de uma interface".

## Decisão
- O keyring guarda **a senha**, não as chaves. Os arquivos selados e o formato do ADR 0012 não mudam; `--protect`, `kin passwd` e o pareamento seguem iguais. A senha vai como **segredo binário** (`set_secret`), byte a byte, já normalizada em NFKC.
- **Opt-in**: `kin --keyring` lê a senha (`Source::Keyring` ao lado de `File` e `Prompt`, em `keys::decide`). `kin keyring save [--data-dir D] [--passphrase-file ARQ]` grava, **só depois de conferir que a senha abre todos os arquivos selados** (`keys::check_passphrase`); `kin keyring forget` apaga (idempotente).
- Entrada: serviço `kin`, usuário = **caminho canônico do `--data-dir`** (`canonicalize`, com `path::absolute` se o diretório sumiu; recusa caminho não-UTF-8). Não há identidade legível antes de abrir as chaves, que é o que a senha destrava, então o caminho é o único nome estável. Mover o diretório exige novo `save`.
- `--keyring` **recusa** `--passphrase-file`, `--no-passphrase`, `--protect`, diretório novo e chaves em claro (nada a ler; nunca cria nem migra). Vence `KIN_PASSPHRASE_FILE`. Sem Secret Service/Keychain/Credential Manager, **falha com mensagem clara**: sem fallback silencioso.
- Dependência: `keyring =4.2.0` (features padrão `v1`: Secret Service via `zbus` no Linux, Keychain no macOS, Credential Manager no Windows, por `cfg` de alvo) e `keyring-core =1.1.0` (o `Entry` usado; os testes ligam o store `mock`). Versões exatas, como as demais de segredo. Atrás do módulo fino `crates/cli/src/os_keyring.rs`.

## Alternativas descartadas
- **Guardar as chaves no keyring**: muda o formato, a migração, o `--protect`, o pareamento e o lab.
- **`keyring-core` + stores escolhidos** (só `zbus-secret-service-keyring-store`): menos código no build, mas só Linux e API mais nova; o README do `keyring` manda isso só para quem precisa de controle fino.
- **Código próprio** sobre `secret-service`, `security-framework` e `windows-sys`: três integrações a manter, com segredo manuseado por nós.
- **`secret-tool`/`security` por linha de comando**: a senha passaria por processo externo; frágil.
- **Fallback automático para prompt**: esconderia o motivo da falha e treinaria o usuário a ignorá-la.

## Avaliação da dependência (2026-11-24; conferir antes de adotar versão nova)
`keyring` 4.2.0 (2026-08-29): 42,7 M downloads, repo ativo (push 2026-11-19), MIT OR Apache-2.0, sem avisos no OSV para `keyring`, `keyring-core`, `secret-service`, `zbus`. **Dono único** no crates.io nas crates de store (`brotskydotcom`), sinal de alerta da norma, mitigado pela org `open-source-cooperative` e pela fronteira fina. Sem `build.rs`, `unsafe`, `libdbus` ou `openssl` nas três crates lidas pela auditoria (Linux = Rust puro). **Não medido**: a árvore de transitivas completa e o tempo de build (precisa do `Cargo.lock`).

## Consequências e limites aceitos
- **Quem lê o keyring abre as chaves**: qualquer programa da mesma sessão, com o keyring desbloqueado, lê a senha (o Secret Service não tem ACL por aplicativo). A cifra em repouso segue valendo contra disco ou backup roubado e contra outro usuário, **não** contra malware no mesmo usuário. A entrada também expõe o caminho do `--data-dir`. Dito no aviso do `save`.
- **Entrada velha** depois de `kin passwd` ou de mover o diretório: `--keyring` falha sem alterar nada e o erro manda rodar `kin keyring save`; o `passwd` avisa disso.
- **Sem timeout**: com o keyring bloqueado e sem agente de prompt (SSH, headless), a chamada ao Secret Service pode ficar esperando o desbloqueio. Falha fechado, mas sem mensagem. Gatilho para tratar (thread com `recv_timeout`): o roteiro manual ou um usuário reproduzir o travamento.
- **Entrada envenenada** (outro programa da sessão cria a mesma entrada): só dá falha (`Ambiguous` ou senha que não abre), nunca aceitação; o erro orienta a apagar as duplicadas. Uma entrada criada antes do primeiro `save` é adotada e sobrescrita.
- **Relay**: `kin keyring save` recusa um diretório cujo `db.key` não abre, embora o relay subisse ignorando-o (ADR 0045); `--keyring` num VPS sem keyring dificilmente funciona.
- **`forget` depois de apagar ou mover o diretório** pode procurar outra chave e dizer "não havia senha", deixando a entrada órfã; apague-a no gerenciador de senhas do SO.
- **O CI não usa `--locked`** nem `cargo audit`/`deny`: a árvore transitiva nova só fica fixa com o `Cargo.lock` commitado.
- **Nada do backend real roda no CI nem no lab** (sem D-Bus no container): só o `mock`. Quem cobre o Secret Service é o roteiro manual no PC do dono.
