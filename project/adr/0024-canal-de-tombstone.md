# ADR 0024 — Canal do tombstone: receber por MLS, admissão e limite (P30 fatia 3c-i)

- **Status**: aceito e implementado no `chat` (Sessão 26, 2026-11-15). Área de segurança (revogação de acesso e entrada de rede,
  AGENTS.md §4): auditado pelo `security-auditor` (1 crítico e 4 importantes, todos tratados ou registrados; tabela no fim) e com 6
  mutações (6 pegas). É só **receber e gravar**: o envio e o repasse são a 3c-ii; a consulta e a derrubada do par são a 3d.
- **Relacionados**: ADR 0011 (canal "payload novo, sempre processado, repassável"), 0022 (formato), 0023 (store; I1 pedia admissão e
  limite), P30, P31.

## Contexto

A 3a deu o formato e a 3b o store, mas nada entrega o tombstone a um device. A 3c foi cortada pelo dono em **3c-i (receber)** e
**3c-ii (enviar e repassar)**. O ADR 0023 deixou como requisito da 3c a política de admissão e o limite de taxa, porque identidades
custam zero e a tabela não tem despejo.

## Decisão

1. **`Payload::Revocation(Box<DeviceRevocation>)`**, tag 7, dentro do MLS (`7 | bytes KRV`). O decode usa `DeviceRevocation::from_bytes`
   (forma canônica e teto de 2048 já são dele); qualquer erro vira `Malformed`, sem detalhe. `Box` porque a variante carrega a cadeia
   da autoridade e é bem maior que as outras (clippy `large_enum_variant`). Um build antigo trata a tag 7 como `Message` e dá `Malformed`.
2. **`Chat::on_revocation(peer, revocation)`**, nesta ordem: limite (por peer e global) → admissão → gravação.
3. **Admissão: só `is_trusted(revocation.identity())`**, isto é, a minha identidade ou uma aceita (`trust`/`accept`). A identidade vem
   da **cadeia assinada** do tombstone, nunca do remetente: o tombstone pode ser repassado por um terceiro, e o `store_revocation` confere a
   assinatura depois, então afirmar uma identidade sem a chave dela falha.
   - **Decisão do dono revista pela auditoria (C1):** o plano aprovado dizia "própria, `trusted` ou de conversa existente". O auditor
     mostrou que **conversa não filtra ninguém**: o Welcome só exige a identidade quando ela está em `expected`, então qualquer estranho que
     conecte ganha uma conversa MLS. O dono trocou para só `is_trusted`. Consequência aceita: um contato com conversa e sem `trust` não consegue me
     revogar um device.
4. **Limite de taxa: 16 por peer e 64 no total, por janela de 60 s**, em memória. Conta **toda tentativa** (inclusive a recusada pela
   admissão e a repetição de um tombstone já gravado), antes da admissão e da assinatura, porque verificar assinatura custa CPU. O excedente
   dá `RateLimited` e o `on_envelope` o transforma em `Dropped`.
   - **A cota do peer não zera ao desconectar** (auditoria I2): reconectar custa só um handshake, e "16 por conexão" viraria inundação por
     reconexão. Ela expira pela janela; as vencidas são podadas a cada chamada. O teto global cobre quem abre várias conexões.
5. **Sem consulta nem derrubada**: o tombstone gravado ainda não revoga ninguém (3d). `CryptoDevice::store_revocation` e `is_revoked`
   ficaram públicos (o `is_revoked_or_unknown`, o ponto fail-closed, segue interno até a 3d).

## Alternativas descartadas

- **Admitir identidade com conversa** (plano original): ver item 3.
- **Exigir remetente = dono da identidade**: mataria o repasse que o ADR 0011 quer.
- **Limite só por peer, zerado ao desconectar**: ver item 4.
- **Cachear a identidade de cada conversa para o ramo "conversa"**: desnecessário depois de tirar o ramo; a admissão virou O(1) (a versão
  anterior chamava `conversation.peers()` por conversa, que verifica assinatura de certificado: CPU amplificável, auditoria I1).

## Consequências e requisitos para a 3c-ii e a 3d

- **Perda silenciosa do tombstone (auditoria I4)**: o excedente de limite e o erro de armazenamento viram `Dropped` e **nada reenvia**
  nesta fatia. É um fail-open até a 3c-ii: a 3c-ii precisa **reenviar ao conectar** (idempotente pelo `ON CONFLICT`), e o erro de
  armazenamento não deve consumir a cota (hoje consome). O contato offline que nunca reconecta continua sem receber (sem outbox/caixa postal).
- **O teto global é uma arma de dois gumes**: um peer `trusted` malicioso gasta os 64 e atrasa uma revogação legítima em até 60 s.
  Aceito (um teto com recusa permanente seria pior; ver ADR 0023).
- **A 3d** consulta `is_revoked_or_unknown` depois do `credential::verify`, num ponto único, com teste de ordem (ADR 0023 I2).
- **Reidratar após perda de banco** (ADR 0023 S4) continua requisito da 3c-ii.
- Um tombstone da **própria** identidade vindo de um irmão é admitido como qualquer outro `trusted`; o ladrão com a mestra v3 (T7) grava
  tombstones de devices que nunca existiram, dentro das cotas.

## Limites conhecidos e o que não foi validado

- Sem revisão externa. Nada rodou fora do CI.
- Lacunas de teste que a auditoria apontou e ficaram: erro de armazenamento no caminho do `chat` (só o `crypto` o cobre); o caminho
  `handle(MessageReceived)` → `Dropped` (os testes chamam `on_mls` direto); janela real (os testes forçam `ZERO`); o `Dropped` por mensagem
  numa `queue` sem teto (S1: um flood excedente gera um evento por mensagem, como qualquer descarte).
- `PeerDisconnected` seguido de `MessageReceived` pode reinserir uma entrada pequena em `revocations_seen`; ela expira pela janela.

## Parecer do `security-auditor` e como foi tratado

| Achado | Gravidade | Tratamento |
|---|---|---|
| C1: "conversa existente" não filtra (qualquer estranho conecta), DoS de disco no `mls.sqlite` | crítico | **feito**: só `is_trusted` (decisão do dono); teste da conversa sem `trust` falhou antes (run 43709041641) |
| I1: `is_known` varria as conversas, verificando assinatura por membro | importante | **feito**: some com o C1 (admissão O(1)) |
| I2: reconexão zerava a cota | importante | **feito**: a cota expira só pela janela, mais teto global de 64; mutação E |
| I3: `untrust` não cortava a admissão | importante | **feito**: some com o C1; teste novo |
| I4: tombstone perdido sem reenvio | importante | **registrado**: comentário corrigido; requisito da 3c-ii |
| S1: um `Dropped` por excedente; referência ao ADR | sugestão | **registrado** (acima); este ADR existe agora |
| Gaps 1, 4, 6, 8, 9, 10 de teste | sugestão | **feito**: teste da cota gasta por recusa, `untrust`, variante `Crypto` da assinatura adulterada, autoridade v4 no `chat`, `trust` isolado |
| Gaps 5, 7 (erro de armazenamento no `chat`; `handle` → `Dropped`) | sugestão | **pendência** (acima) |
