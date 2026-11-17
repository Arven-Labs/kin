# ADR 0034 — `/devices` e `/revoke` no primário, com o registro `issued/` (P30 fatia 5)

- **Status**: **aceito e implementado** (Sessão 28, 2026-11-17). Área de segurança (revogação, AGENTS.md §4): auditado pelo `security-auditor`
  (0 críticos, 1 importante, 5 sugestões; tabela no fim).
- **Relacionados**: ADR 0011 (decisões 4 e 6), 0022 a 0029 (tombstones), 0033, P30.

## Contexto

As fatias 1 a 4 entregaram o mecanismo de revogação (tombstone, store, canal, derrubada, trincos), mas nada na CLI o usava. Além disso, o primário **não guardava
os certificados que emitia**: `kin pair answer` devolvia o link e esquecia, e `AuthorityIdentity::revoke_device` exige o `DeviceCertificate` inteiro (recusa certificado
de outra identidade). Sem registro não há o que listar nem o que revogar.

## Decisão

1. **Registro `issued/<peer-id>.cert`** no `--data-dir` do primário: um arquivo por certificado emitido (bytes do `DeviceCertificate`), 0600, pasta 0700, gravado por
   `pair::answer_and_record` com o `write_new_atomic` que já existia (nunca substitui). Se gravar falha, **o link não é entregue**.
2. **Idempotência só para o mesmo device**: se o arquivo já existe, ele precisa ser um certificado válido, da mesma identidade, com as **mesmas três chaves** (as que o
   tombstone cita). Qualquer outra coisa é erro e nada é emitido (auditoria I1: antes, `AlreadyExists` virava `Ok` e um certificado novo com outra chave MLS ficava fora do
   alcance do `/revoke`, com falsa confirmação). Consequência: re-parear com o mesmo `device.key` e outra chave MLS exige um `device.key` novo ou apagar o registro à mão.
3. **`/devices`** (`devices::list` + `describe`): lê `issued/`, descartando em silêncio o que não é `<peer-id>.cert`, passa de 64 KiB, não verifica, é de outra identidade
   ou tem nome diferente do Peer ID do conteúdo. O estado (`ativo`/`revogado`) vem do store de tombstones (`Chat::is_revoked`), `conectado` de `Chat::is_ready`.
4. **`/revoke PREFIXO [confirmo]`** (`devices::decide_revoke`): prefixo do Peer ID (com ou sem `12D3KooW`), mínimo 4 caracteres depois dele, **único**; sem `confirmo`
   só mostra o device e o comando de confirmação com o Peer ID inteiro; com `confirmo`, `primary.revoke_device` + `Chat::broadcast_revocation`. Recusa o device do próprio
   primário. Só no modo `LocalIdentity::Primary`: no modo da mestra (v3) revogar não adianta (cada device tem a mestra, ADR 0011) e no pareado só o primário revoga.
5. O arquivo de `issued/` **não** é apagado ao revogar.

## Alternativas descartadas

- **Tabela no `mls.sqlite`**: o `pair answer` roda em outro processo e mexeria no schema do banco do `Chat` (AGENTS.md §4).
- **Sem registro, `/revoke` recebendo o link do certificado**: o primário depende de o dono ter guardado o link, e o device roubado não é listável.
- **Nomear o arquivo pela tupla de chaves (ou hash do certificado)** para permitir dois certificados do mesmo Peer ID: mais flexível, mas amplia o formato; ficou
  para quando houver necessidade (decisão do dono se quiser: hoje recusa).

## Consequências e limites

- **Apagar `issued/<peer>.cert` impede o `/revoke` daquele device** (negação da revogação) e o primário não tem outra cópia. Plantar um arquivo **não** dá poder: sem a
  chave da autoridade a cadeia não verifica. Mitigação possível, não feita: `/revoke` aceitar o link do certificado como alternativa.
- Devices pareados **antes** desta fatia não têm registro e não aparecem.
- `/devices` não diz quantos arquivos ignorou (corrompido e removido ficam iguais a "nunca emitido").
- Um registro gravado sem o link entregue (a saída falha, o usuário desiste) aparece como `ativo` em `/devices` sem nunca ter entrado: inofensivo. Sem `fsync` do diretório
  depois do `hard_link`: uma queda de energia pode perder a entrada de diretório (o conteúdo é sincronizado).
- O `/revoke` imprime "gravado e enviado a quem está conectado": quem está offline só sabe ao reconectar (ADR 0011, decisão 6).
- `show_devices` e `revoke_device` (a fiação no `main.rs`) **não têm teste** (o `main.rs` não tem infraestrutura com `Chat`); só `devices.rs` e `answer_and_record`.

## Auditoria (`security-auditor`, somente leitura)

| # | Achado | Gravidade | Destino |
|---|---|---|---|
| I1 | `record` tratava `AlreadyExists` como sucesso sem conferir o conteúdo | importante | corrigido (confere as 3 chaves e a identidade; 2 testes) |
| S1 | A confirmação mostrava só o prefixo digitado | sugestão | corrigido (Peer ID inteiro) |
| S2 | "tombstone enviado" era falso sem peer conectado | sugestão | corrigido (texto) |
| S3 | `now` com `map_or(0)` | sugestão | corrigido (erro) |
| S4 | `answer_and_record` reparseia o link | sugestão | não feito (round-trip barato) |
| S5 | `/devices` não conta os ignorados | sugestão | não feito |
| T | TOCTOU entre `metadata` e `read`; leitura antes do teste do nome; pasta 0700 | baixo | corrigido |
| L | Lacunas de teste (mínimo de 4 caracteres, nome errado, não regulares, ordem, `record` ignorado) | importante | corrigido, exceto a fiação do `main.rs` |
