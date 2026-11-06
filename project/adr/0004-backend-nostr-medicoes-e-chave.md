# ADR 0004 — Backend Nostr da caixa postal: tamanho, envelope e chave do destinatário (Fase 3.3)

- **Status**: proposto (Sessão 8, 2026-11-06). Vira "aceito" quando o dono confirmar a chave (opção B) e o segundo spike (escrita real) for feito ou dispensado.
- **Relacionados**: D1 em `ARCHITECTURE.md`; ADR 0003 (interface de mailbox); P15 (certificado do device); `STUDY.md` (seção Nostr).

## Contexto

A 3.3 implementa o `Mailbox` (ADR 0003) sobre relays Nostr, com gift wrap NIP-17/59 (D1). Duas perguntas travavam o
desenho: (1) as mensagens MLS cabem no que os relays aceitam? (2) de onde vem a chave secp256k1 do destinatário, já
que o Nostr só fala secp256k1 (Schnorr, BIP-340) e o kin usa Ed25519? Este ADR registra o que foi medido num spike
só de leitura (nenhum evento foi publicado) e a decisão proposta.

## Medições (2026-11-06)

**Tamanhos no kin** (medidos com um teste temporário em `crypto`, apagado depois): `KeyPackage` 461 B, `Welcome`
1.157 B, mensagem de aplicação = texto + 145 B (1 B → 146; 1 KB → 1.170; 16 KB → 16.534), commit
(`rotate_keys`) 672 B. Pela caixa postal só vai `Envelope::Mls` (ADR 0003, decisão 4), com 1 B de tag a mais.

**Tamanho do evento NIP-59** (estimativa, confiança média: base64, padding do NIP-44, duas camadas, moldura de
~330 B por evento):

| Mensagem | Bytes MLS+1 | Evento | Crescimento |
|---|---|---|---|
| App, 1 B | 147 | ~1,8 KB | 12× |
| App, 1 KB | 1.171 | ~4,6 KB | 3,9× |
| Commit | 673 | ~3,9 KB | 5,8× |
| App, 16 KB | 16.535 | ~55 KB | 3,3× |
| App, 48 KB | 49.303 | não cabe | NIP-44 estoura 64 KiB |

O teto prático é o **NIP-44**, não o relay: 65.535 bytes de texto puro por camada, e o gift wrap tem duas, cada uma
com base64 e padding. Resulta em **~30 KB por mensagem MLS**.

**Relays públicos** (NIP-11, campo `limitation`; é o limite *declarado*): `relay.damus.io` e `relay.primal.net`
1.000.000; `relay.snort.social` e `nostr.wine` 524.288 (este é pago e com escrita restrita); `nos.lol` e
`offchain.pub` 131.072; `nostr-pub.wellorder.net` não declara; `purplepag.es` só perfis e listas (`content` 131.072);
`relay.nostr.band` e `relay.nostr.bg` sem NIP-11 legível. Nenhum anuncia NIP-17; só o damus anuncia NIP-59; o NIP-40
(expiração) aparece nos strfry, mas não no damus.

## Decisão (proposta)

1. **Gift wrap NIP-17/59** como previsto em D1, com o blob MLS em base64 no `content` do rumor.
2. **Teto de ~30 KB por mensagem** no backend Nostr; acima disso, o `Mailbox` devolve `DepositFailed`. Fragmentação
   (anexos) fica para a Fase 5, não para a 3.3.
3. **Chave do destinatário: opção B.** O device gera uma chave secp256k1 própria, incluída no `DeviceCertificate`
   (v3), assinada pela identidade. O backend resolve Peer ID → chave secp256k1 pelo certificado.
4. **Escrita real fica como risco aberto**: declarar NIP-59 não prova que o relay guarda e serve kind 1059. Medir
   antes de implementar (ver "Em aberto").

## Alternativas descartadas

- **A. Derivar a secp256k1 da semente do device (HKDF).** Sem formato novo, mas acopla duas chaves e exige acesso à
  semente. Mais difícil de auditar que uma chave declarada no certificado.
- **C. Chave efêmera por mensagem.** O destinatário precisa de um endereço estável para buscar; não serve.
- **Fragmentar já na 3.3.** Complexidade (reordenação, perda parcial) sem demanda: mensagens de chat cabem.

## Consequências

- Certificado v3: bancos e certificados antigos deixam de valer (como na v2, P15). Custo de migração baixo hoje,
  porque nada foi distribuído.
- Mais uma chave para proteger (junto de P13, chave sem cifra em repouso).
- O relay vê o destinatário (tag `p`), horário e tamanho, não o remetente (D1, D6). O tamanho do evento revela
  a faixa do conteúdo (padding do NIP-44 só aproxima).

## Em aberto

- **Escrita real**: relays aceitam, guardam e servem kind 1059 para o destinatário? Retenção? Exigem autenticação
  (NIP-42) para ler? Só se mede publicando (segundo spike, com confirmação do dono).
- **Expiração (NIP-40)** por relay, que decide como fazer o TTL da 3.4.
- Estimativa dos tamanhos do evento: só uma implementação real fixa os números.
- A lista de relays é amostra de 2026-11-06 e envelhece; o dono ainda não escolheu a lista padrão.
