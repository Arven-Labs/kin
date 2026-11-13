# ADR 0011 — Revogação de device próprio (D7, P30)

- **Status**: **aceito** (Sessão 21, 2026-11-12): o dono respondeu as 6 decisões (seção "Decisões do dono"), todas pela
  recomendação. Revisado antes pelo parecer do `security-auditor` (seção final). Continua sendo só desenho: nenhum código mudou.
  Área de criptografia e de identidade (AGENTS.md §4), então cada fatia de implementação terá plan mode, testes primeiro e
  auditoria própria, e as fatias que definem formato de fio (declaração de revogação, pedido de pareamento, certificado v4)
  exigem revisão externa antes de virar protocolo.
- **Relacionados**: ADR 0006 (multi-device: pares device↔device, `/pair`); ADR 0007 (`untrust` de contato; deixou a revogação de
  device próprio para o D7); ADR 0009 (histórico entre irmãos); D7 em `ARCHITECTURE.md`; P12 (TruthID), P13 (mestra sem cifra em
  repouso), P18 (convite sem expiração), P21 (re-depósito), P24 e P30 em `PENDING.md`.

## Contexto (fatos lidos no código; o auditor os conferiu)

1. **A identidade é a chave pública mestra.** `IdentityId` envolve a `PublicKey` Ed25519 da mestra
   (`crates/identity/src/provider.rs`). Contatos guardam consentimento (`trust`), lista negra (ADR 0007) e histórico (ADR 0008)
   **por identidade**.
2. **Todo device tem a mestra secreta.** O `StandaloneIdentity` guarda o `Keypair` mestre (`standalone.rs`); o `/pair` imprime a
   mestra como `kin://pair/...` e o device novo a importa com `--pair`. A CLI avisa: "quem vir o link ou o QR é esta identidade e
   não há revogação" (`print_pair`, `crates/cli/src/main.rs`).
3. **O certificado de device é assinado pela mestra e não expira.** `DeviceCertificate` (domínio `kin/device-cert/v3`) cobre a chave
   MLS, a de rede (Peer ID) e a Nostr; `created_at` entra na assinatura mas **nenhum código o lê** como validade; não há campo de
   revogação (`crates/identity/src/device.rs`). É conferido em `crypto/src/credential.rs` (`parse` → `cert.verify()`), no convite
   (`chat/src/invite.rs`) e nos itens das listas de devices.
4. **Uma conversa MLS por par de devices** (ADR 0006), com a credencial do membro = o certificado. **Correção do rascunho
   anterior**: o contato aceito **não** recebe a lista de devices, só o convite do próprio remetente; a lista completa vai só a
   irmão (`chat.rs`, `device_list_for`). O fan-out sai das conversas existentes (`devices_of`), não da lista; a lista serve à
   descoberta e à discagem, é **aditiva** (nunca remove quem some) e, de um contato, só a primeira por conexão age.
5. **"Irmão" é decidido por identidade em vários pontos**, não só na credencial: o espelho `Sent`, a lista de devices de irmão (que
   faz o receptor passar a **confiar** nas identidades que ela repassa, persistido em `kin_trusted`), o `HistoryRequest` e o
   `HistoryChunk` (ADR 0009). Quem se passa por irmão herda tudo isso.

**Consequência**: revogar o certificado de um device perdido ou roubado **não revoga nada**. Quem tem o device tem a mestra e
assina um certificado novo para si. O modelo de hoje é "todos os devices são igualmente confiáveis, para sempre": comprometer um
device compromete a identidade.

## Modelo de ameaça

| | Cenário | O que o atacante tem / faz |
|---|---|---|
| T1 | Device **roubado ou perdido** | hoje: mestra, chaves de device (MLS, rede, Nostr), estado MLS, histórico **de todos os contatos** em claro (ADR 0009 copia tudo a cada irmão); sob B: só as chaves do device e o histórico |
| T2 | Device **aposentado** (honesto, ainda com as chaves) | o mesmo; o risco é futuro (backup, venda, descarte) |
| T3 | **Malware** no device em uso | o mesmo, mais o que o usuário digita; só some quando o malware sai |
| T4 | **Primário perdido ou comprometido** (sob B) | a mestra; é a perda da identidade, sem saída que o ladrão não possa disputar |
| T5 | **Adversário de rede** que retém, atrasa ou derruba mensagens | impede a revogação de chegar; reenvia declarações antigas (replay/rollback) |
| T6 | **Restauração de backup** do primário | devolve contadores ao passado; ressuscita estado revogado |
| T7 | **Revogação maliciosa** por quem tem a mestra | revoga os devices legítimos (DoS) |

Entre o roubo e a revogação, um device roubado **na condição de irmão** pode: puxar o histórico inteiro (`HistoryRequest`, sem
limite de taxa, P27b), fazer o receptor confiar em identidades do atacante (lista de devices de irmão) e forjar espelhos. Parte
desse dano **persiste** depois da revogação e precisa ser reconciliada (ver requisitos). **Fora do alcance de qualquer desenho**: o
que o atacante já leu ou copiou antes da revogação, e T3 enquanto o malware está ativo.

## Opções

### A — Rotação da mestra com sucessão assinada
K2 sucede K1 por declaração assinada por K1; os contatos aceitam a troca.
- **Limite estrutural**: o ladrão também tem K1 e pode assinar uma sucessão **rival** (K2'). Sem verificação fora de banda
  (comparar um código com o contato) o contato não distingue; "o primeiro que chegar vence" favorece quem agir mais rápido.
- **Custo**: o `IdentityId` muda; consentimento, lista negra e histórico por identidade precisam migrar.
- **Papel**: **caminho de migração** das identidades atuais (a mestra já passou por vários devices) e, enquanto não houver D ou
  E, último recurso em T4. Não serve como revogação do dia a dia.

### B — Certificados delegados (a mestra sai dos devices secundários)
A mestra fica só num device **primário**; os demais têm apenas o certificado e as próprias chaves.
- **Pareamento autenticado, sem copiar segredo**: o device novo gera as chaves e mostra um **pedido** autoassinado (cada chave do
  pedido assina o conjunto, com domínio; prova de posse) por QR ou código curto. O primário mostra o mesmo código (comparação nos
  dois lados, tipo SAS), exige **confirmação explícita** e devolve o certificado. O device novo recebe o `IdentityId` do primário
  **no mesmo canal fora de banda**, senão aceitaria certificado de uma identidade do atacante (MITM trocando chaves públicas).
- **Revogação por tombstones**, não por lista substitutiva: uma declaração **por certificado**, **assinada individualmente** pela
  mestra, **cumulativa** (revogado fica revogado), que cita o hash do certificado e **as três chaves** (MLS, rede e Nostr). Sem
  "maior sequência vale": uma lista substitutiva deixa um replay de lista antiga **des-revogar** um device para quem nunca viu a
  nova, e um `seq = u64::MAX` forjado por quem tem a mestra travaria a correção para sempre (T6, T7). Se for preciso detectar
  bifurcação, encadear por hash do anterior (como a sigchain do Keybase).
- **Canal**: **payload novo, sempre processado** (não a `DeviceList`, que o contato quase não lê), enviado pelo primário a todos os
  contatos (por MLS e pela caixa postal por identidade) e **repassável** por qualquer device. **Risco aceito e documentado**: o
  contato cujo único par é com o device roubado só ouve esse device e **pode nunca saber** (o ladrão não repassa).
- **Frescor** (C2, em aberto): revogação só por push torna "não recebi" indistinguível de "não revogado", e para uma identidade
  nunca vista não há estado a conferir. É preciso **pelo menos um** de: convite com **expiração própria** (P18) ou **prova de
  frescor** periódica assinada pelo primário (estilo `nextUpdate` do X.509; o certificado ganha validade ou o contato exige a
  prova recente). Sem isso a janela de exposição é ilimitada para contato offline.
- **Cobre T1 e T2** se o primário estiver seguro.
- **Custos**: o primário vira ponto único e alvo de maior valor (T4); certificado v4; mudança de pareamento e **quebra da API**
  `CryptoDevice::build` (`crypto/src/device.rs`), que hoje gera a chave MLS e certifica no mesmo passo com a mestra à mão; contatos
  guardam tombstones; builds v3 não leem v4.

### D — Hierarquia de dois níveis (variante de B)
Uma **mestra fria** (offline, só assina) certifica uma **chave de autoridade** que vive no primário e assina os devices e as
revogações. Perder o primário rotaciona a autoridade **sem mudar o `IdentityId`**. É o formato do cross-signing do Matrix
(chave mestra assina a *self-signing key*, que assina os devices). Resolve T4 parcialmente; custa a custódia de uma chave fria.

### E — Chave de recuperação pré-comprometida
O certificado inicial leva o **hash** de uma chave de recuperação guardada offline (como a *pre-rotation* do KERI); só quem a tem
assina a rotação, então o ladrão com K1 **não consegue forjar** uma sucessão. Fecha a corrida de A. Custa uma chave a mais para o
usuário guardar.

### C — Devices pares, sem mestra (cadeia de assinaturas, como o Keybase)
Qualquer device revoga qualquer outro; a identidade passa a ser uma cadeia assinada.
- **Descartada por ora**: muda o que é o `IdentityId` (tudo depende dele) e o TruthID (D7) pressupõe raiz única.

## Decisões do dono (2026-11-12)

| # | Decisão | Resposta |
|---|---|---|
| 1 | Direção | **B** (certificados delegados); C fica como alternativa futura |
| 2 | Resposta ao primário perdido (T4) | **D** (mestra fria offline certifica a chave de autoridade do primário) |
| 3 | Migração | **Identidade nova**: a atual é descartada; cada contato reconfirma por verificação fora de banda |
| 4 | Primário | **Obrigatório** para adicionar e revogar devices |
| 5 | Frescor | **Convite com expiração primeiro** (resolve o P18); a prova de frescor periódica do primário fica para depois, se precisar |
| 6 | Contato que só alcança o device roubado | **Risco aceito e documentado** |

Consequências dessas respostas sobre as fatias abaixo: a fatia 1 passa a incluir a **mestra fria e a chave de autoridade** (D),
não só a cifra em repouso; a fatia 4 é, por ora, só **expiração do convite** (P18). O resto da lista de requisitos e de fatias
vale como está.

## Decisão proposta (texto original, já aceito pelas respostas acima)

1. **Direção: B**, com **D ou E** como resposta ao T4 (decisão do dono entre as duas; sem uma delas o primário é ponto único sem
   saída). A é só migração. Confiança média: conheço o desenho geral, **não conferi as especificações** (ver referências).
2. **Primário obrigatório** para adicionar e revogar devices; os secundários nunca veem a mestra.
3. **Validade/frescor: obrigatória** (convite com expiração e/ou prova de frescor); a escolha exata é uma fatia própria. Mudança em
   relação ao rascunho, que dispensava `not_after`.
4. **A identidade atual é "queimada" na migração**: a mestra já esteve em todos os devices pareados com `/pair` e não há como
   garantir que foi apagada. Migração = A com verificação fora de banda; contato que não reconfirmar continua confiando em K1,
   então o regime novo precisa de **trinco por identidade** (uma identidade em regime v4 nunca volta a aceitar v3) e **prazo de
   depreciação** de K1.
5. **Requisitos transversais** (valem para qualquer fatia):
   - **Todo caminho de "irmão"** (credencial, espelho, lista de devices, histórico) confere o estado de revogação; a revogação
     **reconcilia a confiança herdada** de um device revogado (o que ele fez o receptor confiar via lista de irmão).
   - **Par MLS**: em MLS 1:1 não há `Remove` do outro membro; o receptor **apaga o estado local do grupo**, **recusa re-handshake**
     (novo `KeyPackage` com o mesmo certificado, `join`, commit `Add`) e **rechecka ao recarregar** grupos do SQLite
     (`peers()`, `identity_of` e `decrypt` hoje não reverificam). A `credential::verify` é pura e passa a precisar de um oráculo.
   - **Nostr e caixa postal**: o contato desaprende a chave Nostr do device (`mailbox.learn_peer`) e cancela a outbox dele; o que já
     foi depositado nos relays até o TTL (ADR 0005) **segue decifrável** pela chave Nostr do ladrão (revogação não alcança relay);
     e o carimbo NIP-59 é aleatorizado, então mensagem em voo depois da revogação não se distingue de anterior: **política a
     decidir** (descartar tudo perde mensagens do T2; aceitar admite o ladrão).
   - **Armazenamento**: tombstones em tabela própria, **sem despejo** (não passam pelo `insert_bounded`, que descarta por Peer ID) e
     com **falha fechada** (na dúvida, trata como revogado).
   - **Assinatura**: o domínio de assinatura da declaração é imposto na **camada de assinatura**, não por convenção do chamador
     (`IdentityProvider::sign` hoje assina bytes arbitrários, `provider.rs`).
   - **Histórico**: cifra em repouso (P13) também no **secundário**, que guarda o histórico de todos os contatos; rever a
     sincronização do ADR 0009 para devices novos.
6. **Fatias** (cada uma com plan mode, testes primeiro, auditoria e ADR próprio quando mexer em formato). Ordem corrigida: a
   custódia e a migração vêm **antes** de remover o `kin://pair/` com a mestra.
   1. custódia e cifra em repouso da mestra do primário (P13) e a resposta ao T4 (D ou E);
   2. pareamento autenticado por pedido/certificado, **convivendo** com o `kin://pair/` antigo (marcado como obsoleto);
   3. tombstones: formato, canal novo, verificação em irmãos e contatos, derrubada do par MLS, Nostr, trinco por identidade;
   4. validade/frescor (convite com expiração e/ou prova de frescor);
   5. CLI: `/devices` e `/revoke` (no primário);
   6. migração das identidades atuais (opção A com verificação fora de banda) e **só então** remover o `kin://pair/` com a mestra;
   7. cenário de lab: revogar um irmão atrás de NAT; contato **offline**, contato **só conectado ao device roubado**, rollback de
      declaração, restauração de backup.

## Alternativas descartadas

- **"Esquecer o device" localmente**: cosmético; o device continua com a mestra e se reanuncia.
- **Revogar só no relay ou na caixa postal**: não cobre conexão direta nem o par MLS já aberto.
- **Revogar o certificado sem tirar a mestra do device** (o que "parece" revogar hoje): sem efeito, ver Contexto.
- **Lista de revogação substitutiva com "maior sequência vale"** (rascunho anterior): ver tombstones.
- **Opção C**: ver acima.

## Consequências

- Sem código nesta sessão: o risco do P24/ADR 0007 continua aberto; a CLI continua avisando que o `/pair` não tem revogação.
- Aceitar B é aceitar o **primário como alvo de maior valor**; as fatias 1 e 2 deixam de ser opcionais.
- Um contato que só alcança o device roubado pode **nunca** receber a revogação; é limite do desenho, não bug.
- Convites antigos (P18): sem validade, um link de device revogado segue discável para quem nunca viu a revogação; por isso o
  frescor é obrigatório, não "a conferir no handshake" (para identidade nunca vista não há estado a conferir).
- A migração quebra a continuidade da identidade para cada contato; é o preço de não confiar numa mestra que passou por vários
  devices. Builds v3 não leem v4: a interoperação quebra e precisa de anúncio.

## Referências (da base de conhecimento do auditor e da minha; **nenhuma especificação foi conferida nesta sessão**)

- **Matrix cross-signing**: dois níveis (mestra → *self-signing key* → devices). Precedente de D. A remoção de device no Matrix é
  mediada pelo homeserver, então é precedente **fraco** para uma declaração de revogação assinada. Confiança média-alta.
- **Signal, dispositivos vinculados**: **não é precedente de B.** Na provisão o primário entrega ao secundário o par de chaves de
  identidade, e "desvincular" é remoção da lista no servidor, não revogação criptográfica (correção do rascunho anterior, que o
  citava como precedente). Confiança média-alta (~80%).
- **Keybase**: qualquer device revoga outro por sigchain; usa `seqno`, hash do anterior e raiz Merkle publicada contra rollback.
  Precedente de C e do encadeamento por hash. Confiança média-alta.
- **X.509 / RFC 5280**: CRL com `cRLNumber` monotônico (§5.2.3) e `thisUpdate`/`nextUpdate` (frescor); a CRL sozinha não resolve o
  replay, por isso existem `nextUpdate` e OCSP. Inventar um formato próprio é "inventar protocolo": **evitar**, ou adotar a
  estrutura de um padrão e submeter a revisão externa. Também são pertinentes RFC 9420 (credenciais e *Authentication Service*) e
  RFC 9750 (arquitetura do MLS: a revogação fica a cargo do AS).
- **KERI (pre-rotation)**: base conceitual de E.

## O que não consegui validar

- Formato exato da declaração e do pedido de pareamento; codificação; domínio de assinatura. Precisa de revisão externa antes de
  virar formato de fio.
- Viabilidade de D e E com o formato de certificado atual (o auditor também não analisou).
- Se a verificação fora de banda (código a comparar) é aceitável como UX na migração e no pareamento.
- Janela de exposição de um contato offline (mesma família de P19/P21); não medida.
- Comportamento real do MLS ao apagar um par ativo e ao recusar o re-handshake (a medir na fatia 3).
- Todas as referências externas.

## Decisões que cabiam ao dono (respondidas, ver "Decisões do dono")

1. Aceitar **B** como direção (e a opção C como alternativa futura)?
2. Para o **T4** (primário perdido), qual: **D** (mestra fria + autoridade no primário) ou **E** (chave de recuperação
   pré-comprometida)? Sem uma delas o primário é ponto único sem saída.
3. Aceitar que a **identidade atual será queimada** na migração, com reconfirmação por contato?
4. **Primário obrigatório** para adicionar e revogar devices?
5. **Validade/frescor obrigatório** (convite com expiração e/ou prova de frescor)? Qual dos dois primeiro?
6. Aceitar o **limite** "contato que só alcança o device roubado pode nunca saber"?

## Parecer do `security-auditor` e como foi tratado

O auditor (somente leitura, sem rodar build ou teste) **confirmou** os fatos 1 a 3 e **refutou parcialmente** o 4.

| Achado | Gravidade | Tratamento |
|---|---|---|
| C1: a `DeviceList` não serve de canal (contato só recebe o convite; uma lista por conexão; o ladrão não repassa) | crítico | Fato 4 corrigido; payload novo sempre processado; risco aceito explícito |
| C2: sem validade, "não recebi" = "não revogado"; promessa de checar no handshake é impossível para identidade nova | crítico | `not_after` dispensado foi **revertido**; frescor obrigatório; promessa removida |
| C3: "maior sequência vale" permite des-revogar por replay e travar com `u64::MAX`; restaurar backup volta o contador | crítico | Tombstones por certificado, cumulativos, três chaves, tabela sem despejo, falha fechada |
| C4: primário sem recuperação; T4 ausente; faltavam D e E | crítico | T4 adicionado; opções D e E; fatia de custódia passou a ser a 1ª |
| I1: autorização de irmão em 4 pontos; dano persiste (histórico, `trust` via lista) | importante | Fato 5 e requisito de reconciliar a confiança herdada |
| I2: pedido de pareamento sem autenticação (MITM, prova de posse, `IdentityId` fora de banda); `CryptoDevice::build` | importante | Pareamento autenticado em B; quebra de API registrada |
| I3: downgrade para v3, prazo de K1, ordem das fatias incoerente | importante | Trinco por identidade, depreciação de K1, ordem das fatias corrigida |
| I4: Nostr/caixa postal e mensagens em voo | importante | Requisito e política a decidir |
| I5: derrubada do par MLS (sem `Remove` em 1:1; `verify` pura; grupos recarregados) | importante | Requisito explícito |
| I6: histórico em claro em todo secundário (ADR 0009) | importante | Cifra em repouso também no secundário |
| S1 a S4: RFC 5280 vs "não inventar", domínio na camada de assinatura, T5 a T7, cenários de lab | sugestão | Incorporados |
| Referência ao Signal errada; Matrix e Keybase imprecisos | — | Seção de referências corrigida |

O auditor descartou como falso positivo: o ladrão de um secundário emitir certificado novo (em B ele não tem a mestra); a
derrubada de um par derrubar o grupo dos outros devices (os pares são device↔device); replay de certificado válido (barrado por
`credential.rs`); fan-out via lista forjada (o fan-out vem de `devices_of`).
