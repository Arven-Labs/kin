# ADR 0016 — Pedido de pareamento autenticado e código de comparação (P30 fatia 2c-i)

- **Status**: aceito e implementado no `identity` (Sessão 24, 2026-11-14, PR #162). É a metade do `identity`; o device
  "pendente" do `crypto` (2c-ii) e a CLI (2d) vêm depois. Área de criptografia e identidade (AGENTS.md §4): auditado pelo
  `security-auditor` (3 importantes corrigidos; tabela no fim) e com 11 mutações pegas. **Formato de fio novo: exige revisão
  externa antes de virar protocolo** (ADR 0011).
- **Relacionados**: ADR 0011 (opção B/D, pareamento autenticado), ADR 0014 (certificado v4), ADR 0015 (primário), P30, P31.

## Contexto

O `/pair` copia a mestra para cada device, então revogar não revoga nada (ADR 0011). A 2a criou o certificado v4 e a 2b o
primário sem a mestra, mas o primário ainda não tinha como certificar um device novo (`/pair` recusa). A 2c entrega o
protocolo de pareamento sem copiar segredo: o device novo gera as chaves e mostra um **pedido**; o primário confere e devolve o
**certificado v4**. Transporte manual (texto ou QR), sem rede.

## Decisão

1. **`PairRequest`** (`KPR` 1): as três chaves públicas do device (MLS, rede, Nostr), um nonce de 16 bytes e uma **prova de
   posse por chave**: cada uma assina `kin/pair-request/v1` sobre as três chaves e o nonce (framing com prefixo de tamanho).
   `verify` confere as três sem curto-circuito e sem dizer qual falhou; recusa a mesma chave em dois papéis. A chave MLS é do
   openmls, então ela assina por um closure; o domínio é imposto dentro do `PairRequest`.
2. **Primário**: `AuthorityIdentity::answer_pair_request(pedido, sas_confirmado, created_at)` verifica o pedido e **só certifica
   se o código digitado for igual a `pedido.sas_code(&própria identidade)`**: a API não emite sem confirmação humana.
3. **Código de comparação (SAS)**: `SHA-256(kin/pair-sas/v1 ‖ identidade ‖ 3 chaves ‖ nonce)`, 128 bits viram `% 10^24`
   (**24 dígitos, ~80 bits**, 4 grupos de 6). Não cobre as provas (o Schnorr não é determinístico).
4. **Device novo**: calcula o SAS com a identidade que recebeu **do primário, fora de banda, antes de gerar o pedido**, nunca a
   do certificado devolvido. `accept_certificate(cert, identidade_esperada)` só aceita certificado válido, dessa identidade e
   com as três chaves do pedido.
5. `PairRequest::generate` sorteia o nonce do CSPRNG; `new` (nonce injetado) fica para teste. O Schnorr usa aux_rand aleatório.
6. Dependência nova `sha2 =0.11.0` (já na árvore via `libp2p-identity`; zero compilação nova).

## Alternativas descartadas

- **Código de 6 dígitos**: 20 bits se quebram gerando ~1 milhão de chaves.
- **SAS com compromisso prévio (ZRTP)**: duas idas e voltas no transporte manual. Registrado como limite (abaixo).
- **Identidade do certificado devolvido no SAS do device novo** (rascunho): quem intercepta escolhe a identidade e o nonce e
  colide os dois lados em ~2^30 (aniversário). Corrigido pelo item 4.
- **Peer ID como código**: cobre só a chave de rede.
- **Byte de papel por prova**: redundante depois de recusar a mesma chave em dois papéis.

## Consequências

- O fluxo manual tem um passo a mais: o primário entrega a identidade (QR/link) **antes** do pedido, e a pessoa digita no
  primário o código lido no device novo.
- O código tem 24 dígitos: mais longo de comparar; é o preço dos ~80 bits sem commit-reveal.
- Pedido sem `created_at`: não expira sozinho; **TTL e descarte de pedido já respondido são da CLI (2d)**, o primário não guarda
  estado.

## Limites conhecidos e o que não foi validado

- **Sem commit no SAS**: quem vê o pedido pode procurar um nonce que colida (~2^80 com 24 dígitos, antes 2^60). Aceito; o
  commit-reveal fica como reforço se o canal manual tiver atraso controlado pelo atacante.
- **Sem vetor fixo do SAS**: nenhum teste fixa o valor de um pedido conhecido; mudar domínio, ordem dos campos ou módulo só é
  pego pelo teste de cobertura por chave. Pendência (P31(i)).
- **Chave Ed25519 de ordem pequena** passa na prova de posse (mesmo A5 do ADR 0014, P31(c)); condição: o primário confirma o SAS.
- Não foi confirmado quais features do `libp2p-identity` estão ligadas (o `verify` não recusa explicitamente chave não Ed25519).
- Não foi feito `cargo audit` do `sha2`; Schnorr sobre mensagem de tamanho variável (`schnorrsig_*_custom`) não é validado por
  verificadores BIP-340 de 32 bytes fixos (aqui só o Kin verifica).
- Sem fuzz/proptest do parser (P31(d)). Sem revisão externa do formato.
- Reprodutibilidade: o teto do pedido (1 KiB) e o teto da prova (128 B) são meus; não vieram de especificação.

## Parecer do `security-auditor` e como foi tratado

Sem bypass criptográfico da prova de posse e sem panic alcançável por bytes de rede.

| Achado | Gravidade | Tratamento |
|---|---|---|
| 1: SAS do device novo com a identidade do certificado devolvido (aniversário, ~2^30) | importante | **feito**: identidade esperada fora de banda + `accept_certificate`; testes que falharam antes |
| 2: SAS sem commit; grind de nonce ~2^60 | importante | **parcial**: 24 dígitos (~80 bits); commit-reveal descartado por ora |
| 3: primário certificava sem confirmar o SAS | importante | **feito**: `answer_pair_request` recebe o SAS digitado |
| 4: mesma chave em dois papéis; domínio único | sugestão | **feito**: `verify` recusa chave repetida |
| 5: chave de ordem pequena passa na prova | sugestão | **pendência** (P31(c)) |
| 6: Schnorr sem aux_rand | sugestão | **feito** |
| 7: `put` com `expect` pode dar panic | sugestão | **feito**: teto da prova em `new` |
| 8: nonce injetado pelo chamador; sem frescor | sugestão | **feito** (`generate`); TTL é da 2d |
| 9: viés do `u64 % 10^18` | sugestão | **feito**: `u128 % 10^24` |
| 10: lacunas de teste | sugestão | **parcial**: um teste por chave no SAS, teto, erro do signer; sem vetor fixo |
