# ADR 0021 — O `kin` normal abre um `--data-dir` pareado (P30 fatia 2d-ii-b)

- **Status**: aceito e implementado no `cli` (Sessão 25, 2026-11-14, PR #198). Área de identidade e chaves (AGENTS.md §4): auditado
  pelo `security-auditor` (sem críticos, 1 importante corrigido; tabela no fim) e com 3 mutações pegas. **Nenhum formato de fio novo** e
  nenhuma mudança em `identity`, `crypto` ou `chat`: só a CLI passa a usar o `Chat::open_paired` (ADR 0018).
- **Relacionados**: ADR 0017 (`open_paired`), 0018 (`Chat::open_paired`), 0020 (`kin pair`), P30, P31.

## Contexto

Depois do `kin pair accept` (ADR 0020), o diretório tem `primary.id`, `device.key` e o `mls.sqlite` com o certificado v4, mas o `kin`
normal só conhecia mestra e primário: via só `device.key`, tratava o diretório como novo e **geraria uma mestra**, o oposto do que o
pareamento autenticado existe para evitar.

## Decisão

1. **`primary.id` decide o modo.** É o único arquivo que só o `kin pair request` cria. `load` o testa antes de tudo e chama
   `load_paired`; `LocalIdentity::Paired(IdentityId)` carrega só a identidade pública.
2. **`load_paired` nunca cria chave.** Exige `device.key` existente; recusa `--pair` e a presença de `identity.key`, `authority.key` ou
   `authority.cert` (um pareado não guarda mestra nem autoridade); `--protect` só cifra o `device.key`.
3. **`provider()` virou `Option`**: o pareado não certifica nada (o certificado já está no banco), então não há um provider falso. Em
   `main`, `Some` → `Chat::open`, `None` → `Chat::open_paired(&id, ...)`, que confere a identidade a cada abertura (ADR 0017).
4. **Trinco v4 só na CLI**: `ensure_current_issuer` recusa certificado de emissor `Master` (v3) num pareado. Cobre o único ponto de
   abertura do `Chat` na CLI; o trinco geral (e no `Chat::open_paired`) é da fatia 3 (P31(o)).
5. **Contatos confiáveis (P30 (s)): `trusted` começa vazio** e segue o fluxo existente (`/trust` na primeira conversa; irmãos são
   confiados por construção). É o comportamento do `--pair` antigo: não há regressão. Descartadas: mandar a lista junto do pareamento
   (formato de fio novo, exige revisão externa) e confirmação extra por contato.
6. **Estado sem identidade não é diretório novo** (auditoria I1): `load` recusa quando há `mls.sqlite` mas nenhuma `identity.key` (e
   nenhum `primary.id`/autoridade, que são roteados antes). Antes só o `--pair` fazia essa checagem.
7. A mensagem de `primary.id` corrompido (ADR 0020) deixou de mandar **apagar** o arquivo: manda regravar com a identidade conferida ou
   usar um diretório novo (apagar levava a um beco: `kin pair request` dá `AlreadyPaired`).
8. O anúncio "device pareado ... certificado v4" só sai depois de `Chat::open_paired` e `ensure_current_issuer` passarem.

## Alternativas descartadas

- **Provider "vazio" para o pareado**: esconderia o requisito de identidade esperada (mesmo argumento do ADR 0018).
- **Detectar o pareado pela ausência de `identity.key`**: um diretório novo também a não tem.
- **Exigir `Issuer::Authority` dentro do `Chat::open_paired`**: toca `chat`/`crypto` auditados; fica para a fatia 3.

## Consequências

- Um device pareado só conversa com contatos depois do primeiro `/trust` (ou de sincronizar com o primário); conversas e histórico
  chegam normalmente, o que espera é o processamento da lista de devices de quem convidou.
- Um diretório com `mls.sqlite` e sem identidade passa a falhar também no modo mestra (antes gerava identidade nova por cima do banco).

## Limites conhecidos e o que não foi validado

- **Lab com 2 processos e NATs** (2d-iii) e uso real em 2 máquinas; `Chat` aberto pela CLI não tem teste (só o `load`).
- **Prompt de senha antes de falhar** (S1): `files_state` não conhece `primary.id`; num pareado sem `device.key`, o `kin` pede a senha
  duas vezes e só então falha; num pendente termina em `NotPaired` com mensagem que não cita `kin pair accept`.
- `Mixed` + `--protect` em pareado dá mensagem incoerente com a primeira (S2); `Chat::open_paired` abre o banco v3 e cria o
  `.outbox.db` antes de a CLI recusar (S4, resíduo baixo).
- **`primary.id` não é autenticado nem ligado ao `device.key`** (S5): quem escreve no diretório troca `primary.id` e o certificado do
  banco juntos. É a premissa do ADR 0017; endurecer (gravá-lo dentro do `device.key` cifrado) fica como ideia.
- Sem teste de `open_keys`/`decide` com `primary.id`, de `--protect` em pareado, nem de `primary.id` como link pendurado ou FIFO passando
  por `load` (S7).

## Parecer do `security-auditor` e como foi tratado

Sem críticos; confirmou que `load` nunca cria chave com `primary.id` presente, que a mistura com mestra e autoridade é recusada antes de
qualquer leitura e que `Chat::open_paired` confere a identidade a cada abertura.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: sem `primary.id`, o diretório pareado caía no ramo "novo" e gerava mestra | importante | **feito**: `load` recusa estado sem identidade; mensagem de `primary.id` ajustada; 2 testes falharam antes (run 43642173147) |
| S1: senha pedida antes de falhar; `NotPaired` sem dica | sugestão | **pendência** (P31) |
| S2: `Mixed` + `--protect` em pareado | sugestão | **pendência** (P31) |
| S3: anúncio antes de conferir; texto "próxima fatia" obsoleto; ADR 0021 inexistente | sugestão | **feito** |
| S4: v3 barrado depois de abrir o `Chat` | sugestão | **pendência** (fatia 3, P31(o)) |
| S5: `primary.id` não autenticado | sugestão | **documentado** (premissa do ADR 0017) |
| S6: symlink/TOCTOU | sugestão | sem exploração (mesmo nível de P31(h)) |
| S7: lacunas de teste | sugestão | **pendência** (P31) |
