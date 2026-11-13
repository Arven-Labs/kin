# ADR 0012 — Cifra em repouso de `identity.key` e `device.key` (P13, P30 fatia 1a)

- **Status**: aceito e implementado (Sessão 21, 2026-11-12, PR #118). Área de criptografia (AGENTS.md §4): auditado pelo
  `security-auditor`, com os achados tratados (tabela no fim). É um formato de **arquivo local**, não de fio.
- **Relacionados**: ADR 0011 (revogação de device próprio; esta é a fatia 1a), ADR 0006 (`/pair`), P13 e P30 em `PENDING.md`.

## Contexto

`identity.key` (a mestra Ed25519) e `device.key` (rede + Nostr) eram gravados em claro, só com modo 0600. Quem copia o arquivo
(backup, disco roubado, malware com leitura) é a identidade para sempre. O ADR 0011 põe a custódia como primeira fatia antes de
tirar a mestra dos devices secundários. O dono decidiu cortar em **1a só cifra em repouso** (mestra fria e chave de autoridade
viram 1b) e proteger por **senha, Argon2id + XChaCha20-Poly1305**.

## Decisão

1. **Formato** (`crates/identity/src/sealed.rs`):
   `"KINK" | versão u8 (=1) | m_cost u32 BE (KiB) | t_cost u32 BE | lanes u8 | salt 16 | nonce 24 | cifra + tag 16`.
   O cabeçalho inteiro é o AAD do AEAD. Salt e nonce novos a cada `seal`.
2. **KDF**: Argon2id v0x13, saída de 32 bytes. Padrão de escrita 64 MiB, t=3, 1 lane. **Piso e teto na leitura** (parte do formato
   v1): 8 MiB a 256 MiB, t 1 a 6, lanes 1 a 4, conferidos **antes** de rodar o KDF (o Argon2 aborta, não devolve `Err`, se faltar
   memória).
3. **Senha normalizada em NFKC** (`normalize_passphrase`) antes do KDF: o mesmo texto digitado em teclados ou sistemas diferentes
   dá os mesmos bytes. Não apara espaços nem muda a caixa. Mudar isso depois exige nova versão do formato.
4. **Um erro só** para senha errada e arquivo adulterado (`WrongPassphraseOrCorrupt`): distinguir seria um oráculo.
5. **Política de leitura contra downgrade**: arquivo cifrado exige senha; arquivo em claro **só abre sem senha informada**. Com
   senha informada é `PlainFileWhereSealedExpected` (quem tem escrita no diretório não pode trocar as chaves por outras em claro).
   Arquivos de chave têm teto de 64 KiB na leitura.
6. **Escrita atômica**: temp de nome único no mesmo diretório (modo 0600, `create_new`) + `fsync` + `rename` + `fsync` do diretório.
   A `protect` valida o conteúdo (`from_bytes`), cifra, **reabre em memória e compara** antes de substituir o original.
7. **CLI** (`crates/cli/src/keys.rs`): senha por prompt sem eco (`rpassword`, duas vezes ao criar ou migrar),
   `--passphrase-file ARQUIVO` ou `KIN_PASSPHRASE_FILE` (a flag vence a variável; `--no-passphrase` a ignora). O arquivo de senha
   tem até 4 KiB, UTF-8, perde só a quebra de linha final e é **recusado se legível por grupo ou outros**. `--no-passphrase` grava em
   claro (aviso). Diretório antigo em claro e sem senha **abre com aviso**; para cifrar: `--protect` (migra no lugar). `--pair` com
   senha grava a mestra importada já cifrada. Sem terminal e sem senha para um diretório novo: erro.

## Alternativas descartadas

- **Keyring do SO**: não roda headless nem no lab e é dependência pesada. Fica como backend opcional futuro, atrás de uma interface.
- **Cifrar só a mestra**: a chave Nostr do `device.key` decifra a caixa postal.
- **Aceitar a senha em bytes crus**: a mesma senha digitada de dois jeitos trancaria a identidade, e não dá para consertar depois sem
  migração.
- **Argon2 0.6 e chacha20poly1305 0.11**: APIs que não conheço bem; usei `argon2 =0.5.3` e `chacha20poly1305 =0.10.1` (esta já
  estava no lock). `rpassword =7.5.4` (a 7.5.5 tinha dois dias).

## Consequências

- **Senha perdida = identidade perdida.** Não há recuperação até a 1b (mestra fria) ou a opção E do ADR 0011.
- **Fora do alcance**: `mls.sqlite` (chaves MLS) e o **histórico seguem em claro** (P26b); cifrar o banco é item próprio.
- **A migração não apaga o rastro**: o `rename` troca o inode e os blocos do arquivo em claro antigo podem continuar recuperáveis
  (SSD, journal, snapshot, backup). Uma chave que já esteve em claro deve ser considerada exposta se o disco ou o backup não for
  confiável; a CLI avisa. O mesmo vale para o link do `/pair` e seu arquivo.
- Zeroização é **melhor esforço**: `SchnorrKeypair` (secp256k1) e `libp2p::Keypair` não zeram no drop, e `nostr_secret_bytes()`
  devolve um array simples.
- `rename` substitui um symlink em `identity.key` por arquivo comum, descarta dono, ACL e xattr e quebra hardlinks. Em Windows o
  modo 0600 não se aplica. Android e iOS não foram revistos.
- O lab usa senha fixa em `lab/passphrase` (só do laboratório) e tem o cenário `multi-device-plain` para o caminho em claro.

## O que não consegui validar

- Revisão externa do formato (uso das crates conferido pelo auditor por leitura; build e testes só no CI).
- O código do `argon2 0.5.3` não estava no cache local: o aborto por falta de memória e o efeito da feature `zeroize` nos blocos
  vêm do conhecimento do crate. Conferir no docs.rs.
- O prompt sem eco (`rpassword`) não tem teste automatizado: depende de terminal real. O lab não o cobre (stdin é fifo).
- Se `ed25519_dalek` está com a feature `zeroize` neste build do `libp2p-identity`.
- `cargo audit` (base local) apontou avisos que já existiam (`crypto-bigint`, `libcrux-kem`, `paste`, `proc-macro-error2`); nenhum é
  das crates novas.

## Parecer do `security-auditor` e como foi tratado

Sem críticos. A parte criptográfica (parâmetros do Argon2, XChaCha com nonce de 24 bytes, AAD, CSPRNG, oráculo de erro, colisão de
formatos, temp e symlink) foi confirmada correta.

| Achado | Gravidade | Tratamento |
|---|---|---|
| 1: arquivo em claro aceito mesmo com senha (downgrade por quem escreve no diretório) | importante | `PlainFileWhereSealedExpected`; a CLI só abre em claro sem senha |
| 2: temp de nome fixo (corrida entre dois processos) | importante | nome único por tentativa, sem apagar às cegas |
| 3a: blocos do arquivo em claro antigo recuperáveis | importante | só documentado e avisado (não há solução portátil) |
| 3b: `protect` cifrava qualquer arquivo | importante | `protect` de identidade e de device validam, reabrem e só então substituem |
| 4: leitura sem limite e arquivo inteiro para ver o magic | sugestão | teto de 64 KiB; `key_file_is_sealed` lê 4 bytes |
| 5: teto do KDF alto (aborto por OOM) | sugestão | 256 MiB e t ≤ 6, antes de existir arquivo v1 |
| 6: zeroização parcial | sugestão | feature `zeroize` do argon2 e temporários do `DeviceKey` em `Zeroizing`; o resto fica melhor esforço |
| 7: `KdfParams::MIN` pública e fraca | sugestão | `#[doc(hidden)]`; a CLI só usa `DEFAULT` |
| 8: caminho relativo no `fsync`; `rename` sobre symlink | sugestão | `.` como diretório; o resto documentado |
| 9: normalização Unicode da senha | pergunta | NFKC (decisão do dono) |
