# ADR 0045 — Cifra em repouso dos bancos SQLite (SQLCipher, chave aleatória selada pela senha)

- **Status**: **aceito** (2026-11-22, dono); **fatia 1 implementada** (PR #308: feature ligada e provada por teste). Fatias 2 a 4 abertas, cada uma com plan mode, testes primeiro e auditoria. Ver "Implementação, fatia 1" no fim.
- **Relacionados**: ADR 0012 (cifra em repouso de `identity.key` e `device.key`: formato selado, política contra downgrade; deixou os bancos "fora do alcance"), ADR 0008 e 0010 (histórico no motor), ADR 0037 (endereços de peer no banco), P13, P26 (b), P32 (a).
- **Área de segurança**: **toca criptografia** (AGENTS.md §4): cifra em repouso e gestão de chave. O formato da chave selada **reusa** o do ADR 0012; a cifra dos bancos é a do SQLCipher (padrão estabelecido), sem cripto própria. Auditoria obrigatória em cada fatia de código.

## Contexto

O kin guarda estado em três arquivos SQLite abertos com `rusqlite::Connection::open`, todos em claro e só com modo 0600:

| Arquivo | Quem abre | O que tem de sensível |
|---|---|---|
| `mls.sqlite` | `crypto/src/provider.rs:47`, via `openmls_sqlite_storage` (recebe a `Connection`) | Estado MLS: chaves privadas de assinatura e de criptografia, segredos de época, `state_id`, certificados e revogações |
| `mls.outbox.db` (`db.with_extension("outbox.db")`, `chat/src/chat.rs`) | `chat/src/outbox.rs:94` | Histórico em texto puro, fila de envio, endereços IP dos contatos (P32 a), confiança, tokens de convite |
| banco de pareamento | `crypto/src/pending.rs:46` | Material do device pendente |

`identity.key` e `device.key` já são cifrados por senha (ADR 0012: Argon2id + XChaCha20-Poly1305, `identity/src/sealed.rs`). Quem copia um backup ou um disco roubado e lê os bancos tem **todas as conversas** (o histórico) e **as chaves MLS** (lê o que vier depois, até a próxima rotação). Cifrar só as chaves de identidade deixa o dado mais valioso desprotegido. É o "continua aberto" do P13 e o P26 (b), marcados "antes de distribuir".

Fatos lidos no código e nas dependências (confiança alta, nada rodado):
- `openmls_sqlite_storage 0.3.0` depende de `rusqlite 0.37` com a feature `bundled`; o kin usa a mesma árvore (`libsqlite3-sys 0.35.0`, SQLite compilado do fonte, hoje sem OpenSSL no `Cargo.lock`).
- O `libsqlite3-sys` define `bundled-sqlcipher = ["bundled"]` (SQLCipher embutido, usa o OpenSSL do sistema) e `bundled-sqlcipher-vendored-openssl` (compila o OpenSSL junto). A feature é aditiva sobre o `bundled` que já está ligado.
- O SQLCipher embutido nessa versão é o **4.6.1** (SQLite 3.46.1), licença BSD de 3 cláusulas (Zetetic).
- O `Codec` do `openmls_sqlite_storage` serializa **chave de busca e valor pela mesma função** (`wrappers.rs`): cifrar no nível do app exigiria cifra determinística para as chaves.

## Critérios (em ordem)
Cobrir o dado que importa (chaves MLS e histórico); não inventar cripto (§4); não mudar o schema nem o `openmls_sqlite_storage`; uma senha só para o usuário; custo de reverter; peso da dependência (build, supply chain); dificuldade de testar.

## Alternativas

| Opção | Resumo | Prós | Contras | Veredito |
|---|---|---|---|---|
| **A. SQLCipher** (`rusqlite` com `bundled-sqlcipher`) | O arquivo inteiro, inclusive `-wal`, é cifrado por página; a `Connection` recebe `PRAGMA key` ao abrir | Cobre os três bancos de uma vez; sem mudar schema nem o openmls; padrão da indústria, auditado; sem cifra nossa | OpenSSL no build (CI, lab, PC do dono, mobile); um C a mais no supply chain; custo de CPU; metadados visíveis | **Escolhida (dono, 2026-11-22)** |
| B. Cifra no app só do histórico | Cifrar a coluna do texto com o XChaCha20 que já temos | Sem dependência nova | Deixa as chaves MLS em claro (o pior dos dois); fazer isso para o openmls pede cifra determinística, que seria cripto nossa | Descartada: não fecha o P13 |
| C. Adiar, confiar na cifra de disco do SO | Manter 0600 e avisar na CLI | Zero código | Não é distribuível (backup e disco removível); a lacuna é a do P13 | Descartada |
| D. Cifrar o arquivo inteiro e abrir em memória | Decifra o arquivo para memória (`serialize`/`deserialize`) e regrava ao fechar | Sem dependência de C | Perde a durabilidade do WAL (queda = perda do que não foi regravado) e dobra a RAM | Descartada |

## Decisão (proposta, a confirmar com o dono)

1. **SQLCipher nos três bancos**, com a feature `bundled-sqlcipher` do `rusqlite`/`libsqlite3-sys`. OpenSSL do sistema como padrão; `-vendored-openssl` só se o build do dono (OpenSSL 4.x no Arch, ver "O que não sei") ou o mobile exigirem.
2. **Chave do banco: 32 bytes aleatórios num `db.key`**, ao lado de `identity.key`, **selado** pelo formato do ADR 0012 e pela **mesma senha** (uma senha só). A senha é a "fechadura" de um arquivo pequeno; os bancos usam a chave de dentro. Trocar a senha no futuro regrava um arquivo, não recifra os bancos. Entregue ao SQLCipher como **chave bruta** (`PRAGMA key = "x'<64 hex>'"`, sem PBKDF2 extra: a dureza contra senha fraca já está no Argon2id do selado). Tipo zeroizável no `identity` (reuso de `sealed.rs`), nunca em log.
3. **Política contra downgrade (como o ADR 0012, item 5):** banco cifrado **exige** a chave; banco em claro **só abre sem chave**; com chave informada é erro. Quem tem escrita no diretório não troca o banco por um em claro.
4. **Sem senha (`--no-passphrase`)**: os bancos seguem em claro, com o mesmo aviso do ADR 0012. Não existe `db.key` em claro (não protegeria nada).
5. **Migração (`--protect`, fatia própria):** `sqlcipher_export` para uma cópia cifrada, reabrir com a chave e conferir `user_version` e contagem de linhas por tabela, trocar por `rename` atômico com `fsync` (arquivo e diretório), tratar `-wal` e `-shm`. A CLI avisa que os blocos do arquivo em claro antigo podem seguir recuperáveis (SSD, snapshot, backup), como no ADR 0012.
6. **Escopo do que cifra:** o conteúdo das páginas, o `-wal` e o journal. **Não** esconde o tamanho do arquivo, a quantidade de páginas nem o padrão de acesso.

## Fatias de implementação (cada uma com plan mode, testes primeiro e auditoria)
1. **Dependência e build:** ligar a feature, ajustar CI e lab se faltar `libssl-dev`, e um teste que prova (a) o banco cifrado só abre com a chave certa e falha com a errada ou sem chave, (b) um texto conhecido **não aparece** nos bytes do arquivo nem do `-wal`, (c) o banco em claro com chave é recusado. Mutação lida (nomes dos testes no log).
2. **API:** `Provider::open`, `Outbox::open`, `PendingDevice::open` e `Chat::open`/`open_paired` recebem `Option<&DbKey>` (muda a API pública do `chat` e do `crypto`; consumidores e testes acompanham).
3. **CLI:** criar e desbloquear o `db.key` com a senha do ADR 0012, `--protect` migrando os bancos, mensagens de erro sem vazar a chave.
4. **Lab:** cenário com banco cifrado e reinício (o `lab/passphrase` já existe).

## Consequências
- **Fecha o P13 (bancos) e o P26 (b)**: o histórico, as chaves MLS e os endereços deixam de ficar em claro em repouso.
- **Build:** passa a exigir OpenSSL (headers e biblioteca) no CI, no lab e no PC do dono, e é um item do futuro build mobile. *(Correção na fatia 1: o `Cargo.lock` **não** ganha `openssl-sys` na variante de sistema, só na `vendored`; o binário passa a ligar a `libcrypto` dinâmica. Ver "Implementação, fatia 1".)*
- **Supply chain:** mais um C compilado (SQLCipher 4.6.1, mantido pela Zetetic) e a dependência do OpenSSL do sistema. Dados do registro (2026-11-22): `libsqlite3-sys` e `rusqlite` têm os mesmos dois donos, última release em 2026-11 (a 0.40.0 e a 0.43.0); `openssl-sys` tem três logins de dono e a última release em 2026-11-01. O kin está atrás (0.35.0 e 0.37) porque o `openmls_sqlite_storage` fixa `rusqlite 0.37`.
- **Achado paralelo, independe desta decisão:** o **RUSTSEC-2026-0459** (2026-10-30, CVE-2026-119375: SQLite abaixo de 3.54 pode devolver memória não inicializada do cache de páginas ao abrir um banco com um `-wal` de outro tamanho de página) afeta `libsqlite3-sys < 0.39.0`, e o kin compila a 0.35.0. A exposição é local (precisa de um `-wal` gravado por quem não é confiável) e o Dependabot não mostra alerta aberto. A correção depende de uma versão do `openmls_sqlite_storage` com `rusqlite` mais novo; é item de dependências (`/dependencias kin`), não deste ADR. O SQLCipher 4.6.1 embutido seria **atualizado junto** com essa subida.
- **Senha perdida = identidade e histórico perdidos**, sem recuperação (a 1b, mestra fria, protege a identidade, não o histórico). Backup precisa guardar `db.key` e saber a senha.
- **A chave fica na memória enquanto o app roda**: protege disco roubado, backup e leitura por outro usuário, **não** malware rodando como o usuário.
- **Custo de CPU:** não medido (a literatura fala em alguns por cento a ~15%); mede-se na fatia 1 ou 4.
- Reverter depois de implementado exige uma migração inversa (`sqlcipher_export` para claro). **Agora** é grátis (só este ADR).

## O que não sei
- **OpenSSL 4.x:** a VM de desenvolvimento (Arch) tem OpenSSL **4.1.0**; o SQLCipher 4.6.1 é de 2024 e pode usar APIs removidas na série 4. Se não compilar no PC do dono, o caminho é `bundled-sqlcipher-vendored-openssl` (compila um OpenSSL 3.x junto), com build mais lento. Só o build do dono e o CI mostram.
- **Runner do CI:** o README do `runner-images` (branch `main`) lista `libssl-dev 3.0.13` no `ubuntu-24.04`; a imagem entregue pode diferir. Se faltar, um `apt-get install` no workflow (mudança de CI: §4).
- **Interação das features** `bundled` (do `openmls_sqlite_storage`) e `bundled-sqlcipher` na mesma árvore: por leitura do `Cargo.toml` e do `build.rs` do `libsqlite3-sys` ela é aditiva (`bundled-sqlcipher` prevalece), mas não compilei.
- **Mobile:** compilar OpenSSL para Android e iOS (e o SQLCipher oficial para eles) não foi estudado; o kin hoje é só Unix (`set_permissions` 0600), e `bundled-sqlcipher` também.
- **`-shm`:** não conferi se o índice em memória compartilhada do WAL guarda algo legível; a fatia 1 inclui olhar o arquivo.
- Frequência real de queda de energia no meio da migração: a troca é por `rename`, mas o teste de falha no meio é da fatia 3.

## Testes que vêm primeiro (quando houver código)
Os da fatia 1 acima; na 3, a migração com falha injetada no meio deixa o banco antigo intacto e utilizável, e a chave errada nunca apaga o original.

## Implementação, fatia 1 (PR #308)

Mudou só o `Cargo.toml` do workspace (`rusqlite = { version = "0.37", features = ["bundled-sqlcipher"] }`) e entrou `crates/crypto/tests/sqlcipher.rs`. Nenhuma API do `crypto`/`chat` mudou; os bancos reais **continuam em claro** até as fatias 2 a 4.

### Correções ao desenho (lidas no código e medidas no CI)
- **`Cargo.lock` não muda.** O texto acima (Consequências) dizia que o lock ganha `openssl-sys`. Errado para a variante de sistema: com `bundled-sqlcipher` o `build.rs` do `libsqlite3-sys` só liga a `libcrypto` do sistema (`rustc-link-lib=dylib=crypto`); o crate `openssl-sys` só entra com `bundled-sqlcipher-vendored-openssl`.
- **O motor SQLite desce de versão: de 3.50.2 para 3.46.1** (medido em `libsqlite3-sys-0.35.0`: `sqlite3/sqlite3.h` x `sqlcipher/sqlite3.h`). CVEs corrigidos entre 3.47 e 3.50.2 voltam; a exposição é pequena (o kin usa SQL estático com parâmetros e não executa SQL de terceiros), mas é regressão de patch do motor. O RUSTSEC-2026-0459 (SQLite < 3.54) afeta os dois. Sobe junto com o `libsqlite3-sys` quando o `openmls_sqlite_storage` aceitar `rusqlite` mais novo (SQLCipher 4.7+); item de dependências.
- **O binário passa a depender de `libcrypto.so.N` dinâmica.** O binário atual é autossuficiente. No CI (`ubuntu-24.04`, OpenSSL 3.0.13) e no Lab (containers `debian:stable-slim`, 22 jobs verdes) funcionou; um binário compilado no Arch (OpenSSL 4.1.0) só roda onde houver `libcrypto.so.4`. Para distribuir fora do desenvolvimento: decidir entre `-vendored-openssl` (estático) e documentar o requisito.
- **`sqlcipher_export` não copia o `user_version`** (lido em `sqlcipher_exportFunc`, que copia tabelas, índices, views e triggers). O `Outbox` decide as migrações por `PRAGMA user_version`; a fatia 3 tem de copiá-lo à mão (`PRAGMA enc.user_version = N`), e o teste `export_copies_a_plain_database_into_an_encrypted_one` já o fixa.

### Provas
- **Testes primeiro** (run 45084116461): 5 de 7 vermelhos pelo motivo certo, lido no log (`cipher_version` sem linha, texto em claro no `-wal`, banco em claro aceita chave, `no such function: sqlcipher_export`, chave errada lê); os 2 que passam sem a feature são o controle e a reabertura.
- **Com a feature** (run 45084667782 e Lab 45084667736, commit `33e14e0`): 7 testes verdes e a suíte inteira, com o Lab de 22 jobs verde. O SQLCipher compilou no runner com o `libssl-dev` dele.
- **Auditoria** (`security-auditor`, sem crítico/alto): precedência de features conferida no `build.rs` (uma só `libsqlite3-sys` por causa do `links`, o SQLCipher vence o `bundled`); bancos sem `PRAGMA key` seguem SQLite comum e compatíveis. Achados aplicados: testes casam o código `NOTADB` (não só `is_err`), cabeçalho do arquivo cifrado diferente de `SQLite format 3\0` (com controle), controle em claro também no `-wal`, `cipher_version` começa com `4.`, a chave não é impressa nas mensagens, `user_version` no export. Achado deixado: o `-shm` não é inspecionado (guarda índice do WAL, não conteúdo; risco baixo).
- **Mutações**: sem a feature (5 testes pegam); `PRAGMA kez` no helper e busca por outro texto (PRs rascunho #309 e #310, resultado no registro da Sessão 40).

### Ainda não verificado
Build no PC do dono com OpenSSL 4.1.0 (o auditor conferiu só os símbolos: todos exportados em `libcrypto.so.4`); compatibilidade de um banco real já existente com o motor 3.46.1; `cipher_provider`; custo de CPU; mobile.
