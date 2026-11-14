# ADR 0017 — Device pendente de pareamento e reabertura sem `IdentityProvider` (P30 fatia 2c-ii)

- **Status**: aceito e implementado no `crypto` (Sessão 24, 2026-11-14, PR #174). Área de chaves e identidade (AGENTS.md §4):
  auditado pelo `security-auditor` (2 importantes corrigidos; tabela no fim) e com 10 mutações pegas. Nenhum formato de fio
  novo; só chaves novas em `kin_meta` no SQLite. O `Chat` e a CLI ainda não usam isto (fatia 2d).
- **Relacionados**: ADR 0011 (opção B/D), ADR 0014 (certificado v4), ADR 0015 (primário), ADR 0016 (pedido e SAS), P30, P31.

## Contexto

A 2c-i deu ao `identity` o `PairRequest` e a resposta do primário. O device novo (secundário) não tem `IdentityProvider` (nem
mestra, nem autoridade), mas o `CryptoDevice::build` gera a chave MLS e a certifica no mesmo passo, com um provider à mão. Faltava
onde guardar a chave e o pedido entre pedir e receber o certificado, e como reabrir depois.

## Decisão

1. **`PendingDevice`** (tipo separado, não um modo do `CryptoDevice`): `open(path)` abre o banco, gera ou reaproveita a chave MLS
   **sem certificá-la**; `request(device_key)` devolve o pedido guardado ou gera e guarda um; `install_certificate(device_key,
   cert, esperada)` confere e instala; `discard_request()` apaga o pedido e mantém a chave MLS. O `CryptoDevice` segue sempre
   completo, com certificado.
2. **O pedido fica no banco** (`kin_meta.pair_request`): reabrir devolve o mesmo nonce e o mesmo código de comparação, e o
   certificado só é aceito se for das chaves do pedido guardado. O pedido guardado é conferido a cada uso contra o signer e a
   `device.key` em uso (o banco é só um arquivo).
3. **Instalação atômica**: o certificado entra por `INSERT` simples e o pedido só é apagado se ainda for exatamente o lido
   (compare-and-swap, `changes() == 1`), numa transação; qualquer falha desfaz e o banco **continua pendente**.
4. **`CryptoDevice::open_paired(identidade_esperada, rede, nostr, path)`**: reabre sem `IdentityProvider`. Verifica o certificado
   a cada abertura, exige a **identidade esperada, guardada fora do banco**, as chaves de rede e Nostr, e que a chave MLS do
   certificado seja a do signer guardado.
5. **Um banco com pedido pendente não abre como mestra ou primário** (`PairingInProgress`): senão `build` geraria outra chave
   MLS por cima da que o pedido declarou.

## Alternativas descartadas

- **Certificado opcional no `CryptoDevice`**: todo `.certificate()` passaria a poder falhar.
- **Pedido novo a cada abertura**: o código de comparação mudaria entre reinícios.
- **Identidade guardada só em `kin_meta`**: quem escreve no arquivo trocaria a identidade junto com o certificado (I1).
- **Mexer no `Chat` já nesta fatia**: toca o crate `chat` (muitos testes); fica para a 2d.

## Consequências

- O chamador (2d) precisa guardar a identidade do primário **fora do `mls.sqlite`** (por exemplo junto da `device.key`) e passá-la
  em toda reabertura.
- Um certificado cobre as chaves, não o nonce: depois de `discard_request`, um certificado emitido pelo primário de verdade para
  as chaves deste device continua válido; um para as chaves de um intruso nunca casa.
- `AlreadyPaired` também vale para banco de mestra (já tem certificado); a mensagem foi ajustada.

## Limites conhecidos e o que não foi validado

- O compare-and-swap do `finish_pairing` **não tem prova de vermelho**: pegar a corrida exige dois processos ou falha injetada.
- Cada checagem de chave contra a `device.key` está mascarada pelas outras nos testes (não dá para construir uma `DeviceKey` com
  uma chave só trocada).
- **Estados parciais** (signer sem certificado, certificado sem signer) não têm erro próprio no `build`, em `PendingDevice::open`
  nem em `open_paired` (S2). `build` não chama `certificate.verify()` nem confere a chave MLS do certificado (S3, já era assim).
- Dois `request()` concorrentes podem divergir do código mostrado (S1): falha fechado.
- O signer MLS e o estado MLS seguem **em claro** no SQLite (0600): P13. Restaurar backup antigo reverte contadores (T6 do ADR 0011).
- Aceita certificado v3 (da mestra) na instalação e na reabertura: o trinco v4 vs v3 é da fatia 3 (S5).
- Não foi conferido se o peer exige `credential.signature_key == cert.signing_key` além do `verify()` do certificado.
- Não foi verificado o `busy_timeout` do `rusqlite` e o comportamento da corrida entre processos.

## Parecer do `security-auditor` e como foi tratado

Sem bypass da verificação do certificado, sem panic novo; a atomicidade do `finish_pairing` estava correta.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: `open_paired` não fixava a identidade; certificado válido de outra identidade para as mesmas chaves passava | importante | **feito**: `expected` fora do banco; teste que falhou antes |
| I2: `install_certificate` não amarrava o pedido à `device.key`; gravação sem compare-and-swap | importante | **feito** (CAS sem prova de vermelho); teste da `device.key` que falhou antes |
| S1: dois `request()` concorrentes | sugestão | **pendência** (P31) |
| S2: matriz de estados parciais no `build` | sugestão | **pendência** (P31) |
| S3: `build` sem `verify()` | sugestão | **pendência** (P31(b)) |
| S4: pedido eterno, sem descarte | sugestão | **feito**: `discard_request`; TTL é da 2d |
| S5: certificado v3 aceito no secundário | sugestão | **pendência** (fatia 3) |
| S6: nome `AlreadyPaired` enganoso | sugestão | **feito**: mensagem ajustada |
