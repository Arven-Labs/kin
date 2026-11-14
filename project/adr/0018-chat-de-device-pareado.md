# ADR 0018 — `Chat::open_paired`: o chat de um device pareado (P30 fatia 2d-i)

- **Status**: aceito e implementado no `chat` (Sessão 25, 2026-11-14, PR #185). Área de identidade (AGENTS.md §4): auditado pelo
  `security-auditor` (sem críticos nem importantes) e com 2 mutações pegas. **Nenhum formato de fio novo**: só um caminho de abertura.
  A CLI ainda não o usa (2d-ii).
- **Relacionados**: ADR 0011 (opção B/D), ADR 0014 (cert v4), ADR 0016 (pedido e SAS), ADR 0017 (`PendingDevice`, `open_paired`), P30, P31.

## Contexto

O `Chat::open` exige `&dyn IdentityProvider` (mestra ou autoridade) só para certificar o device. Um device **pareado** (2c) não tem
nenhum dos dois: recebeu o certificado v4 pronto e o guardou no banco (`PendingDevice::install_certificate`). Faltava abrir o `Chat`
sobre esse banco.

## Decisão

1. **`Chat::open_paired(expected, device_key, config, db)`**: chama `CryptoDevice::open_paired` (ADR 0017) e segue o mesmo caminho de
   `Chat::open`. `expected` é a identidade do primário, recebida fora de banda e guardada **fora** do `mls.sqlite`; é conferida a
   cada abertura.
2. **Helper privado `open_with_device`**: o trecho que `open` e `open_paired` têm em comum (outbox `db.with_extension("outbox.db")`,
   `trusted`/`distrusted` com a regra "desconfiança vence", recarga das conversas) foi movido sem mudar a ordem nem o comportamento.
3. O contato enxerga o device pareado como a **identidade do primário** (a identidade vem do certificado v4, não do device).

## Alternativas descartadas

- **`Option<&dyn IdentityProvider>` em `Chat::open`**: esconderia o requisito de identidade esperada e permitiria abrir sem conferir.
- **Copiar o corpo de `open`**: dois caminhos de recarga poderiam divergir.

## Consequências

- Um device recém-pareado começa com `trusted` **vazio** (vem da outbox dele); os contatos do primário só chegam por sincronização
  entre irmãos. **Decisão de produto para a 2d-ii**.
- `open_paired` aceita também um banco criado pela mestra (cert v3, `Issuer::Master`) se a identidade esperada bater: o nome diz
  "pareado" sem garantir o emissor. O trinco v4 vs v3 é da fatia 3 (P31(o)); por ora só está documentado no doc da função.
- Uma recusa pode deixar um `.db` vazio se o caminho não existia (`Provider::open` cria; `open` faz o mesmo).

## O que não foi validado

- Uso real pela CLI e o lab (2d-ii e 2d-iii). Revisão externa do formato do cert v4 (ADR 0014).
- Se o openmls recusa um `Update` que troca a chave do leaf sem recertificar: `conversation.rs` enfileira `Update` sem chamar
  `credential::verify` (pré-existente, fora desta mudança; registrado no P31).

## Parecer do `security-auditor` e como foi tratado

Sem críticos nem importantes. Confirmou: sem bypass da identidade esperada (ordem em `device.rs`: signer e cert, `verify()`,
identidade, chaves de rede e Nostr, chave MLS), sem herança do modo mestra, sem regressão em `open` (corpo movido igual) e que o
P31(p) está resolvido (`credential::verify` confere `signature_key == cert.signing_key` nos 3 caminhos de entrada).

| Achado | Gravidade | Tratamento |
|---|---|---|
| S1: `open_paired` abre banco de mestra (v3) | sugestão | **documentado** no doc da função; trinco na fatia 3 (P31(o)) |
| S2: lacunas de teste no chat (`trusted`/`distrusted`, outbox em recusa, `open` sobre pendente) | sugestão | helper compartilhado, coberto por `open`; sem teste novo |
| S3: recusa pode criar `.db` vazio | sugestão | cosmético, igual a `open` |
| S4: `trusted` vazio no pareado | sugestão | decisão de produto, 2d-ii |
| S5: outbox e conversas em SQLite sem autenticação | sugestão | pré-existente (vale para `open`) |
