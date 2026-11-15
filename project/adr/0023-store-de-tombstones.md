# ADR 0023 — Store de tombstones no `crypto` (P30 fatia 3b)

- **Status**: aceito e implementado no `crypto` (Sessão 26, 2026-11-15). Área de segurança (revogação de acesso, AGENTS.md §4):
  auditado pelo `security-auditor` (sem críticos; tabela no fim) e com 4 mutações (4 pegas). É só **armazenar e consultar**: nada
  chama `is_revoked` ainda (a ligação é a 3d) e nada recebe tombstone da rede (3c).
- **Relacionados**: ADR 0011 (tombstones, sem despejo, fail-closed), ADR 0022 (formato `DeviceRevocation`), P30, P31 (af).

## Contexto

A 3a (ADR 0022) entregou o formato. A 3b dá a ele um lugar para viver e uma pergunta para responder: "este device está revogado?".

## Decisão

1. **Tabela `kin_revocations`** no `mls.sqlite`, criada em `Provider::open` ao lado de `kin_meta` (`CREATE TABLE IF NOT EXISTS`, sem
   versão de esquema, aditivo). No `crypto` e não no `chat`: é onde a `credential::verify` roda e onde está a conexão SQLite.
2. **Chave primária `(identity, signing_key, network_key, nostr_key)`** (P31 af). `identity` em base58 (TEXT) e as três chaves em 32
   bytes (BLOB). Nunca só as chaves nem o `cert_hash`: as chaves de device são públicas, e uma autoridade de outra identidade assina
   validamente um tombstone com as chaves de um device da vítima.
3. **`store_revocation(&DeviceRevocation)`** confere `verify()` e usa a identidade **da cadeia assinada** (`revocation.identity()`),
   nunca um parâmetro do chamador. `INSERT ... ON CONFLICT DO NOTHING` (idempotente; ignora só o conflito de chave). **Sem despejo**:
   não passa por teto nenhum.
4. **`is_revoked(&DeviceCertificate) -> Result<bool>`**: o erro de leitura é erro. **`is_revoked_or_unknown -> bool`** é o ponto
   fail-closed: erro vira `true`. Quem decide aceitar um par deve usar o segundo.
5. **Pré-condição documentada**: o certificado consultado já passou por `DeviceCertificate::verify`.

## Alternativas descartadas

- **Tabela no `chat` (`outbox.rs`, esquema v8)**: a `credential::verify` mora no `crypto`; o oráculo atravessaria o crate.
- **Chave por `cert_hash`**: um certificado reemitido para as mesmas chaves (v3, mestra à mão) escaparia do tombstone (ADR 0022, item 4).
- **`is_revoked -> bool` com o erro tratado por dentro**: esconde banco corrompido de revogação real.

## Consequências e requisitos para a 3c/3d

- **Sem teto nem política de admissão (auditoria I1)**: qualquer identidade válida grava tombstones sem limite, e identidades custam
  zero. Com o canal da 3c ligado, isso é negação de serviço por disco. **Decisão que precisa vir antes da 3c** (do dono): admitir só
  tombstones de identidade própria ou conhecida (conversa, `trusted`) e limitar a taxa por origem no canal. Teto com recusa deixaria o
  atacante bloquear a revogação legítima; teto com despejo reintroduz o replay.
- **A 3d deve consultar depois do `credential::verify`, num ponto único** (auditoria I2), com teste de ordem.
- **Reidratar após perda de banco (S4)**: apagar linhas, restaurar backup antigo, o reset do P16 ou recriar o banco (`IF NOT EXISTS`)
  devolvem devices revogados ao estado "não revogado", sem erro. Vem do P13 (banco em claro); sem armazenamento autenticado não fecha.
  A 3c precisa reidratar os tombstones depois de qualquer perda; avaliar guardá-los fora do banco MLS.
- **Falha de gravação não é "tombstone inválido" (S2)**: `SQLITE_BUSY` ou disco cheio devem ser tentados de novo, não descartados.
- **`is_revoked_or_unknown` trata `SQLITE_BUSY` transitório como revogado (S5)**: logar a causa e tentar de novo antes de derrubar o par.
- **Esquema sem versão (S6)**: um campo novo (validade, fatia 4) exigirá `ALTER`; o `chat/outbox.rs` já tem o padrão de `user_version`.

## Limites conhecidos e o que não foi validado

- Sem revisão externa. `busy_timeout` e `synchronous` efetivos no WAL não foram validados.
- `ed25519_bytes` na chave: hoje só Ed25519 existe no build, então `NotEd25519` é inalcançável; se outro tipo de chave entrar, a
  consulta bloqueia (fail-closed) e a gravação falha (o device fica irrevogável). Sem teste desse caminho (S3).
- Lacunas de teste não cobertas (S7): tombstone adulterado em campo que não a assinatura; mestra revogando v4 e autoridade revogando
  v3; certificado reemitido; conflito do mesmo `(identidade, 3 chaves)` com tombstones diferentes; tamanho do tombstone gravado.
- Nenhum chamador usa o store ainda (`#[allow(dead_code)]` nos três métodos).

## Parecer do `security-auditor` e como foi tratado

Sem críticos. Descartou como falso positivo: tombstone de outra identidade revogando device legítimo, `OR IGNORE` escondendo conflito
que muda a decisão, colisão por concatenação, ambiguidade base58/BLOB, `NotEd25519` como bypass, concorrência na gravação, replay.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: sem admissão nem teto | importante | **requisito da 3c** (acima); decisão do dono |
| I2: `is_revoked` supõe certificado verificado | importante | **feito**: pré-condição documentada; teste de ordem na 3d |
| I3: nada fixa a identidade na chave primária | importante | **feito**: teste das duas identidades nas duas ordens; mutação D (tirar da PK) pega só ele |
| S1: `OR IGNORE` engole qualquer restrição | sugestão | **feito**: `ON CONFLICT (chave) DO NOTHING` |
| S2, S5 | sugestão | requisitos da 3c/3d (acima) |
| S3: `ed25519_bytes` | sugestão | documentado, sem teste |
| S4: linhas apagadas ou banco recriado | sugestão | requisito da 3c (reidratar); limite do P13 |
| S6: esquema sem versão | sugestão | pendência (fatia 4) |
| S7: lacunas de teste | sugestão | pendência (P31) |
