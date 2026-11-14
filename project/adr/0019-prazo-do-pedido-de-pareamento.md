# ADR 0019 — Prazo do pedido de pareamento (P30 fatia 2d-ii-a1)

- **Status**: aceito e implementado no `crypto` (Sessão 25, 2026-11-14, PR #188). Área de identidade (AGENTS.md §4): auditado
  pelo `security-auditor` (1 importante corrigido, tabela no fim) e com 5 mutações pegas. **Nenhum formato de fio muda**: o prazo é
  local ao device pendente (o `KPR` não ganha `created_at`).
- **Relacionados**: ADR 0016 (pedido e SAS), ADR 0017 (`PendingDevice`), P30, P31.

## Contexto

O pedido de pareamento guardado no banco (ADR 0017) não expirava: ficava válido até `discard_request`. A auditoria da 2c-ii (S4) e a
da 2c-i pediram um prazo. O primário não guarda estado e o `KPR` não tem hora (mudar o formato de fio exige revisão externa e é
assunto da fatia 4), então o prazo mora no device novo.

## Decisão

1. **15 minutos** (`PAIR_REQUEST_TTL_SECS = 900`), decisão do dono. Conta a partir da criação do pedido.
2. **`PendingDevice::request`** grava a hora (`pair_requested_at`, u64 BE em `kin_meta`) **antes** do pedido. Com o pedido guardado
   vencido, descarta e gera outro (novo nonce, novo código de comparação). **`install_certificate`** recusa com
   `PairRequestExpired` se vencido; o banco **continua pendente** (nada é gravado antes de tudo conferir).
3. **Falha fechado** (auditoria, item 1): hora ausente (banco da 2c-ii), ilegível, ou **mais de 300 s à frente** de `now` conta
   como vencida. Até 300 s à frente vale (relógio um pouco atrasado). O limite é inclusivo: 900 s vale, 901 s vence.
4. **`now_secs` devolve `Error::Clock`** com o relógio antes de 1970, em vez de 0 (com 0 o prazo nunca venceria).
5. **Tempo testável**: `request_at(.., now)` e `install_certificate_at(.., now)`; as versões simples usam o relógio do sistema.
   Escolhi acrescentar as `_at` em vez de mudar a assinatura das existentes para não reescrever ~40 chamadas de teste.
6. `finish_pairing` apaga a hora na **mesma transação** do certificado; `discard_request` apaga pedido e hora.

## Alternativas descartadas

- **`KPR` v2 com `created_at`**: o primário também recusaria pedido velho, mas muda formato de fio já auditado; fica para a fatia 4.
- **Prazo configurável por flag**: sem dor concreta (YAGNI).
- **Hora no futuro tolerada sem limite** (`saturating_sub`, versão inicial): o prazo deixava de valer com relógio adiantado ou
  arquivo editado.

## Consequências

- O prazo limita só o intervalo entre **pedir e instalar**. A aprovação do primário não expira: o certificado cobre as chaves, não o
  nonce, então um emitido para um pedido vencido ainda instala depois da renovação (só quem tem as chaves privadas deste device o
  usa; teste `a_certificate_for_an_expired_request_still_installs_after_renewal`). A validade do certificado é da fatia 4.
- Quem edita o banco já controla signer e pedido; o prazo **não** defende contra isso, só contra relógio e esquecimento.
- Uma queda entre as duas gravações deixa "hora sem pedido" (inofensivo, regravada no próximo pedido), nunca "pedido sem hora".

## O que não foi validado

- `now_secs` com erro (relógio antes de 1970): não é testável sem injetar o relógio do sistema.
- Valor de tipo SQLite inesperado (`TEXT`/`INTEGER`) em `pair_requested_at`: `meta()` pode devolver erro de armazenamento em vez de
  "ausente"; só `discard_request` desbloqueia. Não verifiquei o `FromSql` do rusqlite fixado (P31).
- Concorrência real entre processos; a conclusão sobre a corrida vem da leitura do compare-and-swap.

## Parecer do `security-auditor` e como foi tratado

Sem críticos e sem bypass por quem não escreve no arquivo do banco.

| Achado | Gravidade | Tratamento |
|---|---|---|
| 1: hora no futuro ou relógio falho deixa o pedido imortal | importante | **feito** (decisão do dono): tolerância de 300 s, `Error::Clock`; 4 testes falharam antes (run 43623210121) |
| 2: `pair_requested_at` órfão depois de parear | sugestão | **feito**: apagada na transação de `finish_pairing`; teste falhou antes |
| 3: hora com tipo SQLite inesperado não falha fechado | sugestão | **pendência** (P31), não verificado no rusqlite |
| 4: pedido vencido com outra `device_key` é recriado em silêncio (antes: `DeviceMismatch`) | sugestão | **pendência** (P31); não é bypass, o pedido novo ainda exige o SAS |
| 5: lacunas de teste | sugestão | **feito**: futuro, `u64::MAX`, hora órfã, `discard`, cert do pedido antigo |
| 6: ADRs 0016 e 0017 diziam "TTL é da CLI" | sugestão | **feito**: linhas atualizadas |
