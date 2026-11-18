# ADR 0037 — Persistir os endereços de peer (schema v8) e rediscar depois de reiniciar (P32 a)

- **Status**: **aceito e implementado** (Sessão 34, 2026-11-18, PR #235). Toca schema (AGENTS.md §4) e lê entrada não confiável do disco: auditado pelo `security-auditor` (0 críticos, 0 altos, 0 médios, 6 baixos; o de maior valor foi corrigido).
- **Relacionados**: ADR 0036 (reconexão automática; **muda a decisão 3, "só memória"**), ADR 0002 (outbox), P13/P26b (arquivo em claro em repouso), P32 a em `PENDING.md`.

## Contexto

O ADR 0036 deixou os endereços dos contatos só em memória. Depois de reiniciar o processo, o contato com mensagem na fila ficava sem endereço até novo convite, `identify` ou mDNS. O levantamento mostrou **dois** buracos, não um:

1. os endereços se perdiam ao reiniciar;
2. mesmo com endereço salvo, nada agendava a rediscagem na abertura: `start_redial` só roda dentro de `flush`, chamado por envio novo, `PeerDisconnected` e conversa pronta. Quem só tem fila no banco depois do reinício não dispara nenhum desses.

## Decisão (dono, 2026-11-18)

1. **Tabela nova `kin_peer_addrs (peer, addrs)` na outbox**, no mesmo arquivo `*.outbox.db` (0600) do histórico e dos relays, no molde de `kin_peer_relays`. Um endereço por linha. **Schema v8**, aditivo (só `CREATE TABLE IF NOT EXISTS`, como as v3 a v7); um build antigo recusa o banco v8, sem downgrade.
2. **Só peer com conversa vai para o disco.** O estranho conectado (janela pré-conversa, #234) continua só em memória; o endereço dele vai para o disco quando a conversa nasce (`ready`). `forget_peer` apaga a linha.
3. **Gravação a cada `identify`**, sem comparar com a memória (auditoria #1: uma gravação que falhou precisa ser refeita), em melhor esforço: erro de gravação não derruba o chat.
4. **Na abertura, `restore_peer_addrs`** (depois da varredura de revogados): relê os endereços de cada conversa, passa de novo pelo `dial_addrs` e pelos tetos de quando foram aprendidos (o disco é entrada não confiável), e chama `start_redial` para cada conversa. `redial_want` já ignora quem não tem fila.
5. **Prova de ponta a ponta no lab**: cenário `reconnect-restart` (o `b` reinicia no mesmo `/data` sem `--dial`, com o `a` fora e uma mensagem na fila). `lab/node.sh` lê `/tmp/kin-args`, se existir, para trocar os argumentos no reinício.

## Alternativas descartadas

- **Continuar só em memória** (ADR 0036): o contato some até alguém agir; foi o ponto de partida.
- **Arquivo novo** ou guardar no `crypto`: mais um handle e uma migração sem ganho; não é assunto do `crypto`.
- **Não subir a versão** (a tabela já seria criada pelo `IF NOT EXISTS`): descartado para seguir a convenção das tabelas aditivas e manter a recusa de downgrade.
- **Persistir também o estranho conectado**: deixaria qualquer um escrever no disco do dono.
- **Agendar a rediscagem de todos só no primeiro envio**: não resolve o buraco 2.
- **Só persistir, sem `start_redial` na abertura**: o cenário de lab sem a chamada falha (ver Consequências).

## Consequências

- **v8 sem volta**: quem instalar um build mais velho depois do v8 não abre o banco.
- **IP de contato em claro no disco**, no mesmo arquivo 0600 do histórico (P26b já aberto). Quem lê esse arquivo já lê as mensagens.
- **Endereço velho**: o disco guarda o último `identify`; se o contato mudou de IP, a rediscagem tenta o velho e conta falha (orçamento de 100 tentativas, backoff do ADR 0036).
- Prova: testes unitários falharam no CI antes da implementação (13 de 351, todos por comportamento ausente) e o cenário `reconnect-restart` do lab passou em 5 tentativas; um único run de contraste sem `restore_peer_addrs` na abertura falhou só nele (o `reconnect` passou).
- **Aceitos sem mudança** (auditoria): (2) quem completa o handshake MLS ganha conversa e o IP dele vai ao disco (depende da admissão em `Conversation`, não lida pelo auditor); (3) a leitura do disco converte todas as linhas antes do corte do `dial_addrs` (só importa com acesso de escrita ao arquivo); (5) com mais de 8 conversas só 8 recebem endereço na abertura, escolhidas na ordem do `HashMap`, e um erro de leitura na abertura deixa aquele contato sem rediscagem até um evento; (6) um contato com conversa controla a frequência de gravações; (4) `Multiaddr` de irmão fora do filtro estrito pode virar linha ilegível, ignorada na leitura.
