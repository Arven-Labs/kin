# ADR 0031 — Trinco por identidade no `crypto`: v3 de identidade já em v4 é recusado (P30 fatia 3e-ii-a)

- **Status**: **aceito e implementado** (Sessão 27, 2026-11-16). Área de segurança (identidade, AGENTS.md §4): auditado pelo
  `security-auditor` (0 críticos, 3 importantes, 5 sugestões; tabela no fim). É o lado B da 3e; o lado A é o ADR 0030. A **3e-ii-b** (o `chat`
  derruba as conversas v3 já abertas e reabre limpando) segue pendente e é **parte do mesmo objetivo**: sem ela a proteção está incompleta.
- **Relacionados**: ADR 0011 (item 4), 0023, 0027, 0030, P30, P31 (ak), (al), (au).

## Contexto

Um contato C que já viu a identidade X em regime v4 (certificado `Issuer::Authority`) ainda aceitava um certificado v3 (`Issuer::Master`) de X:
quem copiou a mestra de X no `/pair` antigo emite v3 e entra como "device de X". O ADR 0011 pede um **trinco por identidade**: uma identidade em
v4 nunca volta a aceitar v3.

## Decisão

1. **Tabela `kin_regime (identity TEXT PRIMARY KEY)`** no `Provider` (`CREATE TABLE IF NOT EXISTS`, sem migração). `mark_authority_regime`
   (`INSERT OR IGNORE`, monotônico: nunca se desmarca) e `is_authority_regime` (erro de leitura é erro: **falha fechada**).
2. **`credential::verify` recebe `own`** (a identidade do próprio device) e recusa com **`Error::Downgrade`** um certificado v3 de uma identidade
   marcada, depois do tombstone e antes de `Expected`. A regra é `credential::is_downgrade`, a mesma de `CryptoDevice::is_downgraded` (que o `chat`
   usará na 3e-ii-b).
3. **A marca só é gravada depois de aceitar o par**, não em todo `verify` (`credential::mark_regime`): no `join` depois de todas as checagens e antes de
   `into_group`; no `invite` depois do `verify` (que já conferiu `expected`) e antes do `add_members`. Marcar em todo `verify` deixaria um estranho gravar
   estado permanente só por apresentar uma credencial (a mesma falha da 3b, auditoria I1).
4. **A identidade própria fica fora** (nem marcada, nem barrada): um device em modo mestra emite v3 de propósito e convive com irmãos v4 até a migração
   (fatia 6). Dois guardas independentes (`is_downgrade` e `mark_regime`), cada um com teste unitário próprio.
5. **O ramo `Add` de `decrypt` só confere** (`verify` com `own`), não marca: com `MAX_MEMBERS = 2` ele nunca é aceito (P31 (al)). Quando a Fase 5 subir o
   teto, passa a marcar. Desvio do plano, registrado aqui.

## Alternativas descartadas

- **Marcar em todo `verify`**: ver 3.
- **`kin_meta` com chave textual**, ou **derivar do que há nas conversas**: a derivada não sobrevive à conversa apagada pela 3d, que é quando o trinco precisa ficar.
- **Exigir também que X esteja em `trusted`**: amarraria o `crypto` ao conceito de confiança do `chat`; o dono aceitou o trinco a partir de **um** par v4 aceito.

## Consequências e limites (o que este ADR NÃO fecha)

- **Confiança no primeiro uso (TOFU)**: um contato que nunca viu um v4 de X aceita v3 de X. O trinco só anda a partir do primeiro v4 aceito.
- **Não resolve o roubo da mestra** (auditoria I3): quem tem a mestra também chama `certify_authority`, cria a **própria** autoridade e emite devices v4; o
  trinco só vale contra um atacante que fica no regime v3. O resto é a fatia 4 (validade/frescor) e a migração com identidade nova (ADR 0011).
- **Não protege os irmãos** (auditoria I1, decisão do dono: **fica para depois**, P31): a exceção da identidade própria vale também para um device v4, então um
  "irmão" v3 forjado com a mestra roubada entra nele. Corrigir exige acabar com a convivência v3/v4 da mesma identidade, que o teste
  `a_paired_device_belongs_to_the_master_and_talks_with_a_normal_device` fixa até a migração.
- **Estado intermediário até a 3e-ii-b** (auditoria I2): quem já aceitou um v4 antes desta versão não tem linha em `kin_regime` (sem backfill); só um handshake
  novo liga o trinco. Numa conversa v3 aberta, o commit ou `Update` do v3 vira `Downgrade` em `verify_continuity` e consome a época (P31 (ao)); a mensagem de
  aplicação segue passando. A 3e-ii-b deve fazer o backfill (marcar os v4 das conversas gravadas) e derrubar as conversas v3 da identidade.
- **A tabela só cresce** (S3): um peer com identidade nova e v4 válido grava uma linha ao ser aceito num handshake, e ela sobrevive à derrubada. Custo pequeno,
  sem teto; mesma família do P31 (au).
- **Marca antes de `add_members`/`into_group`** (S5): se a operação seguinte falhar, a marca fica (direção segura).

## Provas

- Esqueleto (fiação completa, `provider` sem lógica): 8 testes falharam pela asserção, 5 controles passaram (run 43972646600); verde (43973337183).
- Mutações (branches descartáveis, todas pegas): tirar a checagem do `verify`; marcar sem exigir v4; ignorar `own` nos dois guardas; marcar no `verify`;
  tirar a marcação do `join`. Duas tentativas barraram no compilador (variável sem uso) e foram refeitas.

## Auditoria (`security-auditor`, somente leitura)

| Achado | Gravidade | Destino |
|---|---|---|
| I1: a exceção `own` vale também para device v4 (irmão v3 forjado entra) | importante | **decisão do dono: depois**; P31 (bb) |
| I2: sem backfill; conversas v3 abertas ficam num estado intermediário | importante | 3e-ii-b; documentado acima |
| I3: o trinco não barra quem emite a própria autoridade v4 com a mestra roubada | importante | documentado; fatia 4 e migração |
| S1: guardas de `own` sem teste isolado; `join` recusado sem teste; falha de gravação no meio de `invite`/`join` sem teste; ordem das recusas sem teste | sugestão | os 2 primeiros **corrigidos** (testes); os outros em P31 (bc) |
| S2: `Add` não marca | sugestão | decisão 5 |
| S3: tabela cresce por estranhos | sugestão | P31 (bd) |
| S4: `Invite` e `DeviceList` ainda ensinam/discam v3 de identidade travada | sugestão | 3e-ii-b |
| S5: marca antes de `add_members`/`into_group` | sugestão | documentado |
