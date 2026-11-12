# ADR 0010 — Retenção do histórico: teto de linhas por contato (P26a)

- **Status**: aceito (Sessão 21, 2026-11-12). Implementado no PR #105.
- **Relacionados**: ADR 0008 (o motor guarda o histórico); ADR 0009 (sincronização entre irmãos); P26 e P27 em `PENDING.md`.

## Contexto

A `kin_history` (ADR 0008) recebe uma linha por mensagem lógica (enviada, recebida, espelho de um irmão ou importada de um irmão)
e nunca apaga nada. Sem teto o arquivo cresce para sempre, e o texto está em claro em repouso (P13). O P26(a) pedia retenção
antes de distribuir.

## Decisão

1. **Teto de linhas por contato** (decisão do dono): cada conversa, chaveada pela identidade do contato, guarda no máximo `N`
   linhas, as mais novas por `seq` (ordem de chegada). Padrão **10 000** (`DEFAULT_HISTORY_LIMIT`), **ligado**; configurável por
   `Chat::set_history_limit(Option<NonZeroUsize>)`, com `None` = sem teto.
2. **A poda roda em `Outbox::record_history`**, só quando a linha foi de fato inserida. É o único funil de gravação (os 4 ganchos
   de `chat.rs` e a importação do irmão), então uma mudança cobre tudo, sem temporizador:
   `DELETE FROM kin_history WHERE contact = ?1 AND seq <= (SELECT seq ... WHERE contact = ?1 ORDER BY seq DESC LIMIT 1 OFFSET N)`.
   Usa o índice `(contact, seq)`; com menos de `N + 1` linhas a subconsulta dá `NULL` e nada sai.
3. **Baixar o teto não apaga na hora**: o excesso sai na próxima gravação **daquela** conversa. Apagar dado do usuário dentro de um
   setter seria surpresa.
4. **Sem schema novo.** Só `DELETE`; o `seq` é `AUTOINCREMENT` e não volta atrás, então os cursores dos irmãos
   (`kin_history_cursor`) continuam válidos.

## Alternativas descartadas

- **Idade máxima.** Usaria `sent_at_ms`, que vem do remetente (data antiga some na hora, futura nunca some), ou exigiria uma
  coluna "gravado em" (schema v8).
- **Só manual (`/prune`).** Não fecha o crescimento enquanto ninguém rodar.
- **Poda periódica em tarefa à parte.** Precisa de temporizador e de mais um caminho de código.
- **Teto desligado por padrão.** Nada some sem pedir, mas o crescimento sem teto continua para quem não configurar.

## Consequências

- **Apaga dado do usuário, sem volta, e vem ligado**: quem passar de 10 000 linhas numa conversa perde as mais antigas, sem
  aviso. É por contato, não global. (Esta é uma operação destrutiva no sentido do AGENTS.md §4.)
- **Sincronização**: a "união" dos históricos dos irmãos passa a ser a união **dos retidos**. Se um irmão recriar o banco (cursor
  volta a 0), o pull reimporta linhas que este device já podou; o mesmo `record_history` as apaga de novo (trabalho jogado fora,
  não corrupção).
- **Ordem por `seq`, não por `sent_at_ms`**: um device que importa 500 linhas do irmão com teto 100 guarda as últimas 100 **na
  ordem do irmão**.
- **Fora do escopo**: flag ou comando na CLI para o teto; apagar mensagem (P26e); cifra em repouso (P26b); poda de `kin_seen*`.
