# ADR 0014 — Certificado de device v4, assinado pela autoridade (P30 fatia 2a)

- **Status**: aceito e implementado (Sessão 23, 2026-11-13, PR #145). Área de criptografia e **formato de fio** (AGENTS.md §4):
  auditado pelo `security-auditor` (sem críticos nem importantes; achados na tabela do fim) e com 7 mutações pegas. O formato é
  próprio (mesma estrutura do v3 e do `KAUT`, não de um padrão) e **não deve virar protocolo estável antes de revisão externa**.
- **Relacionados**: ADR 0011 (opção B com D), ADR 0013 (chave de autoridade), P30 em `PENDING.md`.

## Contexto

Na decisão D, a mestra fica fria e a **chave de autoridade** do primário certifica os devices. Até aqui o `DeviceCertificate` só
sabia ser assinado pela mestra (v3, domínio `kin/device-cert/v3`, verificado com a chave da identidade). O certificado chega aos
outros pelo credential MLS (`crypto/src/credential.rs`) e pelo convite/`DeviceList` (`chat/src/invite.rs`), e os dois só chamam
`from_bytes` + `verify`.

Decisões do dono (Sessão 23): a fatia 2 vira quatro sub-fatias, um PR cada (**2a** formato, 2b primário com a autoridade, 2c pedido
de pareamento, 2d CLI e lab); o pareamento será **manual, sem rede**; o v4 **embute a cadeia**; e, depois da auditoria, o device
**amarra a emissão** do certificado de autoridade (A1).

## Decisão

1. **`Issuer`** no `DeviceCertificate`: `Master` (v3, inalterado) ou `Authority(Box<AuthorityCertificate>)` (v4). O campo
   `identity` continua, para os chamadores não mudarem; no v4 ele vem da cadeia.
2. **Fio v4**: `"KDC" | 4 | len16 AuthorityCertificate::to_bytes ("KAUT" v1) | len16 chave MLS | len16 chave de rede |
   len16 Nostr(32) | created_at u64 BE | len16 assinatura`. O v3 começa sempre por `0x00` (parte alta do tamanho da identidade),
   então o magic separa os dois sem ambiguidade, e um build antigo lê `0x4B44` como tamanho e **falha fechado**.
3. **Payload assinado pela autoridade**: `kin/device-cert/v4 || len32 identidade || len32 chave de autoridade || len32 created_at
   do KAUT || len32 MLS || len32 rede || len32 Nostr || created_at`. A identidade entra porque, no v4, quem assina não é ela; o
   `created_at` do `KAUT` amarra **qual emissão** certificou o device (A1), para a fatia 3/4 poder revogar ou expirar por emissão.
4. **`verify` do v4**: a mestra assinou o `KAUT`; a identidade do `KAUT` é a do certificado; a autoridade assinou o payload.
5. **`AuthorityIdentity { key, certificate }`** implementa `IdentityProvider` e emite v4. O construtor recusa `KAUT` com assinatura
   inválida e chave que não é a do `KAUT` (`Error::AuthorityKeyMismatch`); cobre a parte "verificador chave↔cert" da S5 do ADR 0013.
   O `CryptoDevice` e o `Chat` não mudaram: já recebiam `&dyn IdentityProvider`.

Quem assina o quê: a mestra assina `kin/device-cert/v3` e `kin/authority-cert/v1`; a autoridade só `kin/device-cert/v4`. Nenhum
domínio é prefixo de outro, e nenhuma das duas chaves assina bytes livres (S1, PR #144).

## Alternativas descartadas

- **Referência por hash** ao `KAUT`, que viajaria separado: a verificação passaria a depender de estado e de ordem de chegada.
- **Numerar o v3**: quebraria todos os devices atuais.
- **Tipo separado `DeviceCertificateV4`**: todo chamador teria de lidar com dois tipos.
- **Amarrar só a chave de autoridade** (A1, versão inicial): obrigaria a revogação e a validade a serem sempre por chave.

## Consequências

- **Builds antigos não leem o v4**: recusam o membro MLS e descartam a `DeviceList` **inteira** se um único item for v4 (A6). Até a
  2b, ninguém emite v4 fora dos testes; na 2b, anunciar a quebra e atualizar os devices antes de criar um primário com autoridade.
- O certificado cresce ~170 bytes (credential, convite, `DeviceList` ≤ 8 itens); cabe no teto de 64 KiB, e o QR do convite fica
  mais denso.
- A mestra pode ser a própria autoridade (certificar a si mesma): não abre ataque (domínios separados), mas contraria a mestra fria;
  a 2b trata ao montar o primário na CLI.

## O que não consegui validar

- Revisão externa do formato. Teste de propriedade/fuzz do parser (pede dependência nova, `proptest`; registrado).
- Rollout entre builds v3 e v4 em rede real (lab só na 2d).

## Parecer do `security-auditor` e como foi tratado

Sem críticos nem importantes. Confirmou: a cadeia fecha em todos os caminhos de rede; `Issuer`, formato e domínio são sempre
consistentes; sem pânico com bytes hostis; um build antigo não lê v4 como v3 válido.

| Achado | Gravidade | Tratamento |
|---|---|---|
| A1: o payload amarrava só a chave de autoridade, não a emissão do `KAUT` | baixa | **feito**: `created_at` do `KAUT` no payload; teste que falhou antes (run 43539973828) |
| A2: campos `pub` permitem v4 incoerente em memória, e `to_bytes` grava a identidade da cadeia | baixa | **feito no P31(a)** (Sessão 23, PR #160): sai o campo `identity`; `Issuer::Master(IdentityId)` e `identity()` derivam a identidade do emissor, e o `verify` do v4 perde a conferência que só pegava esse estado |
| A3: a recarga do SQLite (`CryptoDevice::build`) não chama `verify` nem confere a chave MLS (já existia) | baixa | **pendência** (P31) |
| A4: codificação protobuf não canônica aceita (S4 do ADR 0013) | sugestão | segue na S4; nenhum consumidor usa os bytes como id |
| A5: chave Ed25519 de ordem pequena passa no `verify` não estrito do libp2p (já existia) | baixa | **pendência** (P31) |
| A6: build antigo descarta a `DeviceList` inteira com um item v4 | compatibilidade | registrado em Consequências; ordem de atualização na 2b |
| Sem fuzz/proptest do parser | sugestão | **pendência** (P31), com avaliação da dependência |
