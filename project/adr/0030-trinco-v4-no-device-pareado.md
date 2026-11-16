# ADR 0030 — Trinco v4 no device pareado: `open_paired` e a instalação recusam certificado da mestra (P30 fatia 3e-i)

- **Status**: **aceito e implementado** (Sessão 27, 2026-11-16). Área de segurança (identidade, AGENTS.md §4): auditado pelo
  `security-auditor` (0 críticos, 2 importantes, 5 sugestões; tabela no fim). Só o lado A da 3e; o lado B (trinco por identidade nos
  contatos) é a **3e-ii**, ainda sem ADR.
- **Relacionados**: ADR 0011 (item 4), 0017 (S5), 0018 (S1), 0021 (S4), P30, P31 (o)/(r).

## Contexto

No regime v3 a própria mestra assina o certificado do device, então a mestra mora em cada device (o `/pair` antigo a copia). No v4 uma
autoridade (certificada pela mestra) assina e a mestra fica fria. Um device "pareado" com certificado v3 não está no regime novo: revogá-lo
não protege a identidade. Até aqui só a CLI barrava isso (`ensure_current_issuer`), e **depois** de `Chat::open_paired` já ter aberto o banco;
quem usasse a biblioteca direto abria um banco de mestra como pareado.

## Decisão

1. **`CryptoDevice::open_paired`** (e, por herança, `Chat::open_paired`) recusa com **`Error::NotAuthorityIssued`** um certificado cujo
   emissor não é `Issuer::Authority`. A checagem vem **depois** de `certificate.verify()` e de `IdentityMismatch` (a identidade errada continua
   sendo o primeiro erro) e antes da conferência das chaves.
2. **`PendingDevice::install_certificate(_at)`** faz a mesma recusa **antes** de `finish_pairing` (auditoria I1): sem isso, um v3 consumia o
   pedido e deixava o banco preso (`open_paired` recusa e `request` dá `AlreadyPaired`). Com a recusa o banco **segue pendente** e o v4 certo
   ainda vale.
3. Forma que **falha fechado**: `!matches!(issuer, Issuer::Authority(_))`, e não `matches!(Master)` (uma variante futura de `Issuer` não passa
   por omissão).
4. A mensagem diz "pareie de novo, em um `--data-dir` novo" (auditoria S2). `ensure_current_issuer` da CLI fica como defesa em profundidade.

## Alternativas descartadas

- **Checar só no `Chat`**: deixaria `CryptoDevice::open_paired` (usado direto, p. ex. no teste de `pair.rs`) sem o trinco.
- **Só na abertura (o plano original)**: o dono mandou incluir a instalação depois da auditoria I1.
- **Reaproveitar `IdentityMismatch`**: a causa é outra e o usuário precisa da dica.

## Consequências

- Um banco pareado com v3 não abre mais; a recuperação é um `--data-dir` novo e `kin pair`.
- **Não resolve o lado B**: `credential::verify` dos contatos continua aceitando v3 de uma identidade que já migrou para v4 (P31 (ak), 3e-ii).
  Esta decisão só impede que **este** device pareado rode com v3.

## Provas

- Esqueleto: o teste do `chat` e `open_paired_refuses_a_certificate_issued_by_the_master` falharam pela asserção; os 2 controles passaram
  (run 43958160518); verde (43958950571).
- Mutações (branches descartáveis): `Master` por `Authority` (43959538712): 10 testes falham; trinco antes da identidade (43959577683): 2 falham.
- Instalação: o teste novo falhou sozinho (22 de 23, run 43963691200); verde (43966071427). A mutação "tirar a checagem" é o próprio run vermelho.

## Auditoria (`security-auditor`, somente leitura)

| Achado | Gravidade | Destino |
|---|---|---|
| I1: a instalação aceitava v3 e deixava o banco preso | importante | **corrigido** (decisão 2) |
| I2: ADRs 0017/0018/0021 e P31 (o)/(r)/(ae) defasados | importante | **corrigido** (notas e PENDING) |
| S1: comentário de `ensure_current_issuer` errado (roda depois do `open_paired`) | sugestão | **corrigido** |
| S2: mensagem sem "`--data-dir` novo" | sugestão | **corrigido** |
| S3: `matches!(Master)` falha aberto para variante nova | sugestão | **corrigido** (decisão 3) |
| S4: sem teste da ordem `NotAuthorityIssued` vs `DeviceMismatch` e de v3 adulterado | sugestão | P31 (ba) |
| S5: o trinco é local, não global | sugestão | 3e-ii |
