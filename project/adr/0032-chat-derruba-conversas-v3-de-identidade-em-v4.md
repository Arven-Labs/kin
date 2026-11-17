# ADR 0032 — O `Chat` derruba as conversas v3 de uma identidade que virou v4 (P30 fatia 3e-ii-b)

- **Status**: **aceito e implementado** (Sessão 27, 2026-11-16). Área de segurança (identidade, AGENTS.md §4): auditado pelo `security-auditor` (0 críticos,
  2 importantes, 6 sugestões; tabela no fim). Fecha a 3e (lado B, parte do `chat`); o trinco em si é o ADR 0031.
- **Relacionados**: ADR 0011 (item 4), 0029 (derrubada da 3d), 0031, P30, P31 (ar), (bb) a (bd).

## Contexto

A 3e-ii-a (ADR 0031) fez o `crypto` recusar, em handshakes **novos**, um v3 de identidade já aceita em v4. Ficaram dois buracos no `chat`: (1) conversas v3 já abertas
com essa identidade seguem vivas, e nenhum caminho de mensagem as revalida (`decrypt` de mensagem de aplicação não chama `verify`); (2) quem aceitou um v4 antes de a
tabela `kin_regime` existir não tem a marca (auditoria I2 da 3e-ii-a).

## Decisão

1. **Reuso da derrubada da 3d** (`scan_revoked`, `drop_peer`, mesma ordem: fila, memória, caixa postal, só então conversa e grupo). `Scan` ganha `to_drop` com um
   **motivo** (`Reason::{Revoked, Downgraded}`); a varredura testa `is_revoked` e, se não revogado, `CryptoDevice::is_downgraded`. **Revogado vence.**
2. **Evento próprio `ChatEvent::PeerDowngraded`** (e texto próprio no `SendFailed` e na CLI): não há tombstone, é uma regra local; dizer "revogado" enganaria.
3. **Backfill na reabertura** (`open_with_device`), **antes** da varredura: para cada conversa recarregada, `CryptoDevice::learn_regime` dos certificados dos peers (marca
   a identidade se o certificado é v4 e não é a própria). Sem marcar primeiro, um banco antigo responderia "não" e a conversa v3 sobreviveria. Erro de leitura ou
   escrita **falha a abertura** (falha fechada), como a 3d-ii-c.
4. **Varredura em `ready()`** (conversa nova pronta), mas **só se o par novo é v4 de outra identidade** (auditoria I1: varrer tudo a cada par custaria O(N) por conexão de
   estranho). Em erro ao ler os certificados do par novo, varre (falha fechada).
5. Nomes `scan_revoked`/`drop_revoked_peers` mantidos (sem renome oportunista, AGENTS.md §3 Etapa E); o campo é `to_drop` (não `drop`, que sombreia `std::mem::drop`).

## Alternativas descartadas

- **Reaproveitar `PeerRevoked`**: engana o usuário e a CLI.
- **Migração de schema para o backfill**: o dado já está nas conversas gravadas; uma migração teria de abri-las de qualquer jeito.
- **Varrer a cada mensagem**: só a aceitação de um v4 e a reabertura mudam o conjunto de regimes.

## Consequências e limites

- **Perda real**: um contato legítimo de X que ainda usa um device v3 perde a conversa quando um v4 de X é aceito (é o objetivo do ADR 0011).
- **Janela até o próximo gatilho** (auditoria I2): `mark_regime` grava a marca em `join`/`invite` **antes** de a conversa existir; se `add_members`, `into_group` ou `remember`
  falharem depois, a marca fica e `ready()` não roda, então a conversa v3 sobrevive até a próxima aceitação de v4 ou a reabertura. Em execução, uma falha de `drop_peer` vira
  `Dropped` (o reparo é adiado, igual à 3d). Não revalido `is_downgraded` em `on_mls` (P31).
- **v4 revogado ainda conta como regime v4** (S2): o backfill marca pela conversa v4 mesmo que o device tenha tombstone, e a marca é monótona.
- **Um grupo corrompido impede abrir** (S1): o backfill propaga o erro (falha fechada), como a varredura.
- **O device v3 derrubado segue conectado** (S3): pode reenviar `Hello`/`KeyPackage`; cada tentativa dá `Downgrade` e `Dropped`, e o `on_hello` do lado de maior Peer ID ainda responde.
  Mesma família do P31 (ar); fechar a conexão e ignorar peer derrubado é outra fatia.
- **`Invite`/`DeviceList` ainda ensinam e discam v3 de identidade travada** (S4 da 3e-ii-a): só disponibilidade, o handshake recusa.
- Os limites do ADR 0031 (I1 irmão v3, I3 mestra roubada emite a própria autoridade v4) seguem abertos.

## Provas

- Esqueleto (evento, CLI e `learn_regime` vazios): 5 testes falharam, controles 4 e 6 passaram (run 43981782288). O teste 2 não é controle puro: assere que o v3 cai.
  Verde com a implementação (43982997350). O teste do store quebrado usa uma tabela de **outro formato**, porque `Provider::open` recria a tabela apagada.
- Mutações (todas pegas, branches apagadas): sem a varredura em `ready()`; sem o backfill; `is_downgraded` ignorado; evento trocado; `learn_regime` marcando v3;
  backfill depois da varredura (fixa a ordem); e, depois da auditoria, a condição de `may_turn_latch` invertida.

## Auditoria (`security-auditor`, somente leitura)

| Achado | Gravidade | Destino |
|---|---|---|
| I1: varredura O(N) a cada `ready()` (DoS de CPU por estranhos) | importante | **corrigido** (decisão 4); a mutação H o fixa |
| I2: falha parcial depois de marcar deixa a v3 viva até o próximo gatilho | importante | documentado acima; P31 (be) |
| S1: um grupo corrompido impede abrir | sugestão | documentado |
| S2: v4 revogado ainda marca | sugestão | documentado |
| S3: v3 derrubado segue conectado e reenvia `Hello` | sugestão | P31 (ar) |
| S4: duplicidade de eventos só no resíduo da 3d | sugestão | ADR 0029 |
| S5: `drop` sombreia `std::mem::drop` | sugestão | **corrigido** (`to_drop`) |
| S6: o ADR 0032 não existia | sugestão | **corrigido** (este arquivo) |
| Testes: papéis aleatórios do handshake (quem convida), falha de escrita do backfill, texto novo do `SendFailed`, precedência revogado > downgraded, "sem ressurreição" | lacuna | P31 (bf) |
