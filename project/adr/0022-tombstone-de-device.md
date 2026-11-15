# ADR 0022 — Tombstone de device `DeviceRevocation` (P30 fatia 3a)

- **Status**: aceito e implementado no `identity` (Sessão 25, 2026-11-14, PR #203). Área de criptografia e **formato de fio novo**
  (AGENTS.md §4): auditado pelo `security-auditor` (sem críticos, 2 importantes tratados; tabela no fim) e com 6 mutações (5 pegas,
  1 sobreviveu e virou teste). **Exige revisão externa antes de virar protocolo** (ADR 0011). É só o **formato**: nada o armazena,
  consulta ou propaga ainda (3b a 3d).
- **Relacionados**: ADR 0011 (opção B/D, tombstones), 0013 e 0014 (autoridade e certificado v4), 0016 (padrão de formato com
  domínio), P30, P31.

## Contexto

O `/pair` antigo copia a mestra para cada device, então revogar não revoga nada (ADR 0011). Com a custódia (fatia 1), o certificado v4
e o primário (2a/2b) e o pareamento autenticado (2c/2d) prontos, falta a revogação. Ela tem ~6 partes; o dono cortou a fatia 3 em
**3a (formato, esta)**, 3b (armazenar e consultar, fail-closed), 3c (canal), 3d (derrubar par MLS, Nostr e outbox) e 3e (trinco v3/v4).

## Decisão

1. **`DeviceRevocation`** (`KRV` 1): emissor, `cert_hash`, as três chaves do device (MLS, rede, Nostr), `revoked_at` e a assinatura.
   Uma declaração **por certificado**, individual e cumulativa; **sem lista substitutiva e sem "maior sequência vale"** (um replay de
   lista antiga des-revogaria; um `seq` forjado travaria a correção).
2. **Quem assina: a autoridade ou a mestra** (decisão do dono). Reusa o `Issuer` do `DeviceCertificate`: no caso da autoridade a cadeia
   (`AuthorityCertificate`) viaja dentro do tombstone e a verificação é **sem estado**, como no certificado v4. `AuthorityIdentity::
   revoke_device` e `StandaloneIdentity::revoke_device` (inerentes, fora do trait `IdentityProvider`).
3. **Conteúdo assinado**: domínio `kin/device-revocation/v1`, tipo do emissor, identidade (e, no v4, chave de autoridade e `created_at`
   dela, para amarrar *qual* emissão), `cert_hash` (SHA-256 com domínio próprio `kin/device-revocation-cert-hash/v1`), as três chaves e
   `revoked_at`, tudo com prefixo de tamanho de 32 bits. O fio usa prefixo de 16 bits e **nunca é assinado**.
4. **`revokes(&cert)` casa por identidade e pelas três chaves, não pelo hash** (decisão minha, confirmada na aprovação do plano): quem
   ainda tem a mestra (device v3) reemite um certificado para as mesmas chaves (outro `created_at`, outro hash); casar só pelo hash
   deixaria o tombstone inútil. O hash é o registro de **qual** certificado foi revogado.
5. **`revoked_at` é informativo**: nenhuma decisão de confiança o lê (não é validade nem ordem); o teste com `u64::MAX` garante.
6. **Forma canônica** (auditoria I1): `from_bytes` recusa o que `to_bytes` nunca produz (campo desconhecido e varint não mínimo no
   protobuf da chave, que o `prost` aceita), refazendo `to_bytes` e comparando. Sem isso o mesmo tombstone teria N encodings com a
   mesma assinatura válida e qualquer dedup por hash dos bytes (3b/3c) seria burlável.
7. **Teto de 2048 bytes** no parse (o real tem ~450 com a cadeia); erro único `InvalidRevocation`, sem detalhe (o tombstone virá da rede).
8. **`revoke_device` não chama `certificate.verify()` de propósito** (S4): dá para revogar um certificado de emissão antiga ou
   corrompido, e o dono só assina para a **própria** identidade.

## Alternativas descartadas

- **Lista substitutiva de revogação**: ver item 1.
- **Casar só pelo hash do certificado**: ver item 4.
- **Método no trait `IdentityProvider`**: obrigaria todos os provedores (TruthID, futuros) a assinar tombstone.
- **Só a autoridade revoga**: deixaria sem como revogar os devices v3 copiados pelo `/pair` antigo enquanto a mestra estivesse fria.

## Consequências

- **T7 (DoS por revogação)** é inerente: quem tem a mestra ou uma autoridade válida revoga devices legítimos. Em particular, uma
  **autoridade antiga** (de antes de um `rotate`) e que ainda não foi revogada **continua revogando**; e um device roubado no regime
  antigo, que tem a mestra, pode tombstonar todos os devices. A revogação da própria autoridade é outra fatia.
- **Para a 3b** (requisito escrito aqui para não se perder): o tombstone **só vale para a identidade do próprio `identity()`** (a
  cadeia verificada), então a chave de armazenamento e de consulta tem de ser **`(identity(), signing_key, network_key, nostr_key)`**,
  nunca só as chaves nem só o `cert_hash`: as chaves de device são públicas e uma autoridade de **outra** identidade assina
  validamente um tombstone com as chaves de um device da vítima (ele não revoga nada de X via `revokes`, mas indexar por chave o
  transformaria em DoS). O `verify` não confere a identidade esperada; isso é de quem chama.

## Limites conhecidos e o que não foi validado

- **Sem revisão externa** do formato (ADR 0011). **Sem vetor fixo** (known-answer) do payload nem do `cert_hash`: o `signing_payload`
  assina e verifica com a mesma função, então trocar a ordem dos campos, tirar o byte de tipo ou mudar o `CERT_HASH_DOMAIN` não é
  pego por nenhum teste (mesma pendência do `sas_code`, P31(i)); gerar o vetor exige rodar o código e fixar o valor (P31).
- **O teto de 2048 é redundante** para a correção depois da checagem canônica; nenhuma mutação o distingue (só protege o custo do parse).
- A mutação do domínio de assinatura vazio **sobreviveu** aos testes de comportamento (o enquadramento já difere do do certificado);
  o teste `the_signing_payload_is_bound_to_the_revocation_domain` a fixa.
- Não analisado a fundo: aceitação não estrita do `verify` do `ed25519-dalek` (P31(c)); o mesmo parser-de-chave permissivo existe em
  `DeviceCertificate::from_bytes` e `AuthorityCertificate::from_bytes` (P31(d)); sem fuzz.
- `Error::InvalidRevocation` e `RevocationIdentityMismatch` são novos; nenhum chamador os trata ainda.

## Parecer do `security-auditor` e como foi tratado

Sem críticos. Confirmou: domínios sem prefixo comum, sem ambiguidade entre o enquadramento len32 (payload) e len16 (fio), o byte de
tipo na assinatura, a identidade tirada da cadeia verificada, `revoked_at` sem uso de confiança, sem panic por entrada externa e
replay idempotente.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: o parse aceita encodings que `to_bytes` nunca produz (maleabilidade de fio) | importante | **feito**: forma canônica; 1 teste falhou antes (run 43654724998) |
| I2: o teste do `MAX_LEN` não testa o teto | importante | **parcial**: teste de entrada bem formada e grande; o teto é redundante após o I1, sem mutação que o distinga (documentado) |
| S1: `signing_payload` simétrico, sem vetor fixo | sugestão | **pendência** (P31) |
| S2: amarração do emissor (identidade, `created_at`) no payload não isolada | sugestão | **feito**: teste de reembrulho em outra cadeia válida da mesma chave; mutação F |
| S3: indexar por chave ou `cert_hash` na 3b vira DoS | sugestão | **requisito da 3b** (seção Consequências) |
| S4: `revoke_device` não verifica o certificado | sugestão | **documentado** como intencional |
| S5: ADR 0022 inexistente | sugestão | **feito**: este arquivo |
