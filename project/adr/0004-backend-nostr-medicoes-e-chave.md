# ADR 0004 — Backend Nostr da caixa postal: tamanho, envelope e chave do destinatário (Fase 3.3)

- **Status**: aceito (Sessão 8, 2026-11-06). O dono confirmou a opção B (chave secp256k1 no certificado v3) e a dependência `secp256k1` (libsecp256k1); o spike de escrita foi feito. A decisão 3 foi implementada no PR do certificado v3; as decisões 1, 2 e 4 ficam para o backend (PR seguinte).
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
4. **Tratar cada relay como poço que pode recusar.** A escrita foi medida (seção abaixo) e a política varia por relay
   (web of trust, autenticação na leitura). Uma recusa de um relay é falha *daquele relay*, não da entrega: o backend
   depende de mais de um relay (3.4) e implementa NIP-42 para ler de relays que o exigem.

## Medição de escrita (2026-11-06, segundo spike)

O dono autorizou publicar eventos de teste: 6 por relay, kind 1059, `content` em base64 aleatório (não é NIP-44
real), chave de wrap e destinatário descartáveis, sem segredo. Resultado:

| Relay | 1 KB | 4 KB | 16 KB | 32 KB | NIP-40 (exp. 120 s) | Leitura por `#p` |
|---|---|---|---|---|---|---|
| nos.lol | aceito | aceito | aceito | aceito | honrou (6 → 5 em ≤150 s) | sem autenticação |
| offchain.pub | aceito | aceito | recusado | recusado | honrou (4 → 3) | sem autenticação |
| relay.damus.io | não testado (HTTP 503 no websocket) | | | | | exige NIP-42 (`auth-required`) |

- O `offchain.pub` recusou 16 KB e 32 KB com `Policy violated and pubkey is not in our web of trust`. O gift wrap real
  usa uma chave nova a cada mensagem, igual à do teste, então relays com web of trust podem recusar mensagens
  maiores que alguns KB.
- Só o `nos.lol` aceitou 32 KB. O teto de ~30 KB é o máximo; mensagens de chat devem ficar bem abaixo.
- O `damus` só serve kind 1059 a quem se autentica (NIP-42). Bom para privacidade, mas exige implementar o NIP-42.
- Os eventos sem expiração (1, 4, 16 e 32 KB) ficaram nos relays; a chave de wrap foi descartada de propósito, então
  não há como apagá-los. Todos têm chave descartável e conteúdo aleatório.

## Alternativas descartadas

- **A. Derivar a secp256k1 da semente do device (HKDF).** Sem formato novo, mas acopla duas chaves e exige acesso à
  semente. Mais difícil de auditar que uma chave declarada no certificado.
- **C. Chave efêmera por mensagem.** O destinatário precisa de um endereço estável para buscar; não serve.
- **Fragmentar já na 3.3.** Complexidade (reordenação, perda parcial) sem demanda: mensagens de chat cabem.

## Consequências

- Certificado v3: bancos e certificados antigos deixam de valer (como na v2, P15). Custo de migração baixo hoje,
  porque nada foi distribuído.
- Mais uma chave para proteger (junto de P13, chave sem cifra em repouso). Ela mora no arquivo `device.key` (agora
  versionado: byte de formato, keypair de rede, chave secreta Nostr); arquivos antigos deixam de carregar.
- Dependência `secp256k1` 0.31.1 (libsecp256k1 em C), escolhida em vez de `k256` por não usar o `crypto-bigint`
  0.5.5 do P22. Entrou com 9 pacotes novos no lockfile (`secp256k1-sys`, `bitcoin_hashes` e família).
- A credencial MLS cresce 34 bytes (chave Nostr + prefixo), refletida em `KeyPackage` e `Welcome`.
- O relay vê o destinatário (tag `p`), horário e tamanho, não o remetente (D1, D6). O tamanho do evento revela
  a faixa do conteúdo (padding do NIP-44 só aproxima).

## Em aberto

- **Retenção longa**: só medi até 150 s; não sei por quanto tempo cada relay guarda um evento sem expiração.
- **`relay.damus.io`**: a escrita (o 503 pode ser transitório) e a leitura autenticada com NIP-42 não foram testadas.
- **Relays pagos ou restritos** (ex.: `nostr.wine`) não foram testados.
- **Chave (opção B)**: ainda não confirmada pelo dono; o ADR continua "proposto" por isso (toca criptografia, §4).
- Estimativa dos tamanhos do evento: só uma implementação real fixa os números.
- A lista de relays é amostra de 2026-11-06 e envelhece; o dono ainda não escolheu a lista padrão.
