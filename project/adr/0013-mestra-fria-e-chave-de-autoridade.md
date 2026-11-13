# ADR 0013 — Mestra fria e chave de autoridade (P30 fatia 1b)

- **Status**: aceito e implementado (Sessão 22, 2026-11-13, PR #129). Área de criptografia e identidade (AGENTS.md §4): auditado
  pelo `security-auditor`, com os achados tratados ou registrados (tabela no fim). **Só custódia**: formato de **arquivo local**,
  nenhum byte de fio muda. O formato do certificado de autoridade é próprio (estrutura copiada do `DeviceCertificate`, não de um
  padrão) e **não deve virar protocolo antes de revisão externa**.
- **Relacionados**: ADR 0011 (opção D, esta é a fatia 1b), ADR 0012 (cifra em repouso, reaproveitada), P13 e P30 em `PENDING.md`.

## Contexto

O ADR 0011 escolheu certificados delegados (B) com a opção D para o primário perdido: a **mestra fica fria** (offline, só assina) e
certifica uma **chave de autoridade** que vive no primário e, nas fatias seguintes, assina devices e revogações. Perder o primário
rotaciona a autoridade **sem mudar o `IdentityId`** (contatos, `trust`, lista negra e histórico são por identidade).

## Decisão

1. **Corte** (decisão do dono): só o crate `identity` e comandos de CLI offline; **sem** cadeia no certificado de device e **sem**
   mexer em `credential::verify`. A autoridade ainda não assina devices nem revogações (fatias 2 e 3). Builds v3 seguem interoperando.
2. **`AuthorityKey`** (Ed25519): mesmos `save_sealed`/`load_with`/`protect` do ADR 0012 (cifra por senha, política anti-downgrade),
   mais `save_sealed_new`, que **nunca substitui** um arquivo existente (publica com `hard_link`, não `rename`; exige sistema de
   arquivos com hard link).
3. **`AuthorityCertificate`**: `{identity, authority_key, created_at, signature}`, assinado pela mestra sob o domínio
   `kin/authority-cert/v1` (distinto de `kin/device-cert/v3`; construtor único, o chamador não passa bytes). Arquivo:
   `"KAUT" | versão u8 (=1) | len16 identidade | len16 chave de autoridade | created_at u64 BE | len16 assinatura`; recusa magic ou
   versão diferente e bytes sobrando. `verify` só diz que a mestra assinou; **quem decide a identidade esperada é o chamador**.
   `created_at` entra na assinatura mas **ninguém o lê como validade** (o frescor é a fatia 4).
4. **CLI offline** (`crates/cli/src/cold.rs`, sem rede e sem `Chat`):
   - `kin cold init --master-out ARQ --authority-dir DIR`: gera a mestra (cifrada, para levar a mídia offline) e a autoridade
     (`authority.key` cifrada + `authority.cert` no diretório do primário). O diretório do primário **nunca** recebe a mestra.
   - `kin cold rotate-authority --master ARQ --authority-dir DIR [--expect-identity BASE58]`: a mestra existente certifica uma
     autoridade nova, mesmo `IdentityId`. **A autoridade antiga não é revogada** (só na fatia 3); a CLI avisa.
   - Senhas por `--master-passphrase-file` / `--passphrase-file` ou prompt; **precisam ser diferentes** e não vazias (a senha do
     primário não pode abrir a mestra). Nunca sobrescreve: recusa e não toca no que existe.
   - A mestra fora do diretório do primário é conferida **resolvendo `.`, `..` e links** (ancestral existente mais fundo
     canonicalizado), antes de pedir as senhas. Diretórios criados com modo 0700.
   - Falha depois de gravar a mestra: remove a `authority.key` criada nesta execução e o erro diz onde a mestra está.
5. A CLI normal **ainda não consome** `authority.key` e `authority.cert`; isso vem com o pareamento autenticado (fatia 2).

## Alternativas descartadas

- **Cadeia mestra→autoridade→device já no certificado (v4)**: quebra builds v3 e fixa formato de fio sem revisão externa e sem
  consumidor; fica para as fatias 2 e 3.
- **Só a mestra fria, sem tipo de certificado**: adia justamente as decisões difíceis (domínio, formato).
- **Mestra só em memória na criação**: perder a tela perderia a mestra e impediria rotacionar a autoridade depois.
- **Keyring do SO**: não roda headless (ver ADR 0012).

## Consequências

- **Senha da mestra fria perdida = identidade perdida**, sem recuperação (a opção E do ADR 0011 segue como evolução).
- Deixar o arquivo da mestra no disco do primário anula o ganho; a CLI só consegue conferir o caminho, não o que o usuário faz depois.
- `rotate-authority` não protege contra um primário já roubado enquanto a revogação não existir.
- O `init` roda no computador do usuário: a mestra existe em claro na memória durante o comando (a CLI avisa para rodar sem rede).
- `created_at` vem do relógio do computador (frio e offline costuma errar); a fatia 4 precisa decidir se aceita `--now` explícito.

## O que não consegui validar

- Revisão externa do formato do certificado e do desenho (D); referências do ADR 0011 seguem não conferidas.
- A garantia de `hard_link` em sistemas de arquivos sem suporte (FAT, alguns de rede): falha em vez de substituir, mas não testei.
- O prompt sem eco e o `init` ponta a ponta com `KdfParams::DEFAULT` num terminal real (os testes usam `MIN`).
- Zeroização é melhor esforço (ressalvas do ADR 0012; `to_bytes` devolve `Vec<u8>` simples).
- Se `IdentityProvider::sign` (assinatura livre da mestra) é chamado fora de `crates/` ou de testes.

## Parecer do `security-auditor` e como foi tratado

Sem críticos; a separação de domínios foi confirmada correta (nenhum domínio é prefixo do outro; `from_bytes` sem pânico de índice).

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: "mestra dentro do diretório do primário" comparava só componentes (`..`, link e `./` contornavam) | importante | `resolve` canonicaliza o ancestral existente e normaliza o resto; testes com `..` e symlink |
| I2: "não sobrescrevo" com corrida (`exists()` e depois `rename`) | importante | `save_sealed_new` por `hard_link`; testes de que não substitui e não deixa temporário |
| I3: falha parcial no `init` (mestra sem autoridade, chave sem cert; senha vazia pela API pública) | importante | senha vazia recusada antes de escrever; limpeza da `authority.key`; erro cita a mestra |
| S1: `sign` livre na mestra e na autoridade | sugestão | **pendência**: a mestra nunca deve assinar bytes não estruturados; `sign` vira restrito antes da fatia 2 |
| S2: sem magic e versão | sugestão | decisão do dono: `KAUT` + versão 1 |
| S3: aceita chaves que não são Ed25519 | sugestão | **sem efeito nesta build** (só a feature `ed25519` do `libp2p-identity`): outro tipo nem decodifica; sem teste possível |
| S4: maleabilidade dos bytes (campos desconhecidos no protobuf) | sugestão | **pendência**: só importa se alguém usar hash dos bytes como identidade do certificado |
| S5: `load_with(None)` abre `authority.key` em claro; falta amarrar chave e certificado | sugestão | **pendência** para a fatia 2 (`load_sealed` e um verificador chave↔cert) |
| S6: `rotate` sem aviso de que a antiga segue válida; sem `--expect-identity` | sugestão | os dois feitos |
| S7: KDF `DEFAULT` e sem força mínima de senha na mestra fria | sugestão | **pendência** (decisão de produto) |
| S8: zeroização parcial (`to_bytes`, `clone`, `rpassword`) | sugestão | **pendência**, mesma ressalva do ADR 0012 |
| S9: umask dos diretórios, `fsync` do diretório, ordem dos prompts, eco de `--x=segredo`, `put()` com `expect`, relógio | sugestão | feitos: 0700, validação antes dos prompts, sem eco. **Pendentes**: `fsync` do diretório no `authority.cert`, `put()` com `Result`, `--now` |
