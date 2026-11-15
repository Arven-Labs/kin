# ADR 0027 — Recusar device revogado no `credential::verify` (P30 fatia 3d-i)

- **Status**: aceito e implementado no `crypto` (Sessão 26, 2026-11-15). Área de segurança (revogação de acesso, AGENTS.md §4):
  auditado pelo `security-auditor` (0 críticos, 2 importantes; tabela no fim) e com 4 mutações (4 pegas). Falta a **3d-ii** (derrubar o
  par que já está na conversa), que é **bloqueadora do P30**.
- **Relacionados**: ADR 0022 (tombstone), 0023 (store), 0024 e 0025 (canal), P30, P31 (b), (q).
- **Nota de numeração**: o código da 3c-ii-b cita "ADR 0026", que nunca foi escrito. Este é o 0027 para não apontar essas citações para
  o ADR errado.

## Contexto

Até a 3c o store de tombstones era escrito (recebido, emitido, repassado, reenviado), mas **nada o consultava**: um device revogado
continuava entrando em conversas. A 3d fecha isso. O dono cortou em **3d-i (recusar no `crypto`)** e **3d-ii (derrubar o par no `chat`)**.

## Decisão

1. **`credential::verify` recebe `&Provider`** e recusa com `Error::Revoked` se `is_revoked(&cert)` for verdadeiro. A consulta vem **depois**
   do `parse` (que confere a assinatura do certificado) e da conferência da chave MLS: `is_revoked` assume certificado verificado (ADR 0023
   I2), e consultar antes deixaria um certificado forjado passar como "não revogado". Teste de ordem:
   `verifies_the_certificate_before_asking_about_revocation`.
2. **Ponto único**: os três chamadores de `verify` (`join`, `invite` e o `Add` em `decrypt`) passam `&inner.provider`. Não há como chamar
   `verify` sem a consulta.
3. **Falha fechada, mas diagnosticável (auditoria I2)**: se o store não pode ser lido, `verify` devolve `Error::Storage` (recusa), **não**
   `Revoked`. Assim a UI não anuncia uma revogação que não houve. `Provider::is_revoked_or_unknown` foi removido (sem uso).
4. **Sem `busy_timeout` próprio**: a primeira versão pôs `PRAGMA busy_timeout = 250`, e a auditoria mostrou que o rusqlite já abre com
   5 s (o PRAGMA só reduzia). Removido: vale o padrão de 5 s. Custo aceito: uma consulta pode segurar o runtime por até 5 s no pior caso
   (raro em WAL, só leitura). **Não verificado** com dois processos.

## Alternativas descartadas

- **Consultar no `chat` depois do `join`/`invite`**: duplica o ponto e o teste de ordem não cobre o `crypto` sozinho.
- **Retry de `SQLITE_BUSY` por código (ADR 0023 S5)**: sem `log`/`tracing` no crate, `Error::storage` guarda só a string e não dá para
  testar sem dois processos. Fica adiado.
- **Manter 250 ms**: um BUSY viraria "recusa" por falha transitória.

## Consequências

- `Revoked` sai antes de `UnexpectedIdentity` e `UnexpectedPeer`: aceitável (fail-closed).
- `Conversation::load` (reabrir) e `CryptoDevice::build` **não** reconsultam: um par revogado antes de reabrir volta vivo (ADR 0023 S4, 3d-ii).
- Quem já é membro **não** é reconsultado a cada mensagem (`identity_of` em `decrypt` só lê): é o caso da 3d-ii.

## Lacunas conhecidas

- **`Update` e `UpdatePath` não passam por `verify`** (auditoria I1; P31 (q)): um membro pode trocar a própria credencial por outra com
  certificado válido, sem consulta ao store nem comparação com a anterior. Já existia antes da 3d-i. Para a 3d-ii: `verify` do leaf novo
  e exigir identidade igual à do leaf anterior.
- **O `Add` em `decrypt` não tem teste que falhe se a consulta sumir**: o teto de 2 membros (ADR 0006) recusa antes. A mutação desse
  chamador não é pega; vale quando a Fase 5 levantar o teto, ou extraindo `verify_adds`.
- `on_key_package` cria a `Conversation` antes do `invite`: um `Revoked` deixa um grupo órfão (como outros erros).
- `busy_timeout` e corrida entre processos: sem prova.

## Verificação (resultado real)

| Etapa | Run | Resultado |
|---|---|---|
| Testes com esqueleto | 43767773390 | 4 de 4 testes de revogação falharam (e os 4 que deviam passar, passaram) |
| Implementação | 43768170329 | verde |
| Depois da auditoria | 43770056544 | verde |

**Mutações** (branches descartáveis, 4 de 4 pegas): M1 `verify` ignora a revogação (43768498330, 4 testes); M2 erro de leitura conta como
"não revogado" no `Provider` (43768510609, 2 testes); M3 `parse` sem `cert.verify()` (43768525594, 2 testes: ordem e forjado); M4 erro de
leitura conta como "não revogado" no `verify`, contra o código pós-auditoria (43770378414, 1 teste).

**Auditoria** (`security-auditor`, somente leitura): sem críticos. I1 (Update sem `verify`) virou lacuna acima (P31 q); I2 (`busy_timeout`
e erro de leitura virando `Revoked`) corrigido. Sugestões aplicadas: teste das 3 chaves (rede e Nostr) e comentário de `identity_of`;
as outras viraram as lacunas acima.
