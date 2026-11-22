# ADR 0041 — Recuperar a conversa depois de `Stale` (P9 c)

- **Status**: **implementado e mesclado (PR #281, 2026-11-21); pendências fechadas em 2026-11-22 (PRs #285, #286 e #288)**; ver as atualizações no fim e os limites aceitos. Área de segurança (criptografia, estado de sessão, AGENTS.md §4).
- **Relacionados**: ADR 0001 (ordenação de commits, buffer de época futura), ADR 0040 (re-handshake do P16), P9 (c), P35 e P36 (gate `hello_pending`, conexão antiga).

## Contexto

Quando um lado perde um commit MLS do outro (commit nunca entregue, ou além de `max_past_epochs` = 3, `crates/crypto/src/conversation.rs:18`), tudo o que vem depois chega numa época que ele não alcança. `Chat::hold_future` guarda até `FUTURE_LIMIT` (32) dessas mensagens e, ao estourar, descarta o buffer e emite `ChatEvent::Stale`. Depois disso **nada recupera a conversa**: os dois lados dizem "tenho conversa" no `Hello`, então o P16 não dispara (ADR 0040, "Conversas divergentes sem perda"), e os comentários do `Stale` diziam "recuperar é P16" (circular; corrigido nos docs no PR #280).

## Decisão (dono, 2026-11-21: opção A)

No `Stale`, o **lado atrasado** (o que recebeu a época futura) abandona a conversa e deixa o P16 refazê-la:

1. apaga a conversa e o grupo locais (mesmo padrão da revogação: `conversations.remove` + `Conversation::delete` + `device.forget_conversation`) e limpa os estados por peer como `replace_conversation`; a **outbox não é tocada**;
2. pede ao transporte para **fechar a conexão** com o peer (API nova `Node::disconnect`, sobre `Swarm::disconnect_peer_id`);
3. na reconexão, o `Hello` sai com "não tenho conversa"; o outro lado, que tinha a conversa na abertura da conexão (`hello_pending`, P35), marca `lost_state`, exige a **identidade da conversa antiga** e refaz com `ConversationReset`. Nada disso muda.

**Limite**: no máximo **uma recuperação automática por peer por janela** (proposta: 10 min, só em memória). Dentro da janela o `Stale` só avisa, como hoje. **Sem formato de fio novo e sem schema novo.**

## Alternativas descartadas

- **Mensagem `Reset` no fio (B)**: muda o formato (§4); o ADR 0040 já descartou uma versão; qualquer mensagem de época futura do par poderia forçá-la.
- **Só `/reset` manual pelo dono (C)**: sem risco de apagar sozinho, mas a conversa fica parada até o dono agir (o custo que o ADR 0040 recusou para o P16).
- **Reenviar o `Hello` na mesma conexão**: o gate do P35 só marca `lost_state` com o `Hello` da abertura da conexão; relaxá-lo é a alternativa que o PR #267 mostrou regredir 8 cenários do Lab. Fechar a conexão explicitamente também evita a janela do P36 (a conexão antiga não fica meio-aberta).

## Consequências e riscos

- **Perda de dado**: o buffer descartado e o que estava em voo se perdem; a outbox segue e sai pela conversa nova.
- **Quem pode forçar o `Stale`** (por leitura, **não medido**): `UnknownEpoch` vem de `ValidationError::WrongEpoch` do openmls (`conversation.rs:442`), levantado na validação do enquadramento, **antes de decifrar e de autenticar o remetente**. Então bastam 32 mensagens MLS bem formadas, com o `group id` certo e uma época futura, para disparar o `Stale`, sem chave nenhuma. Pela conexão direta isso só o próprio par consegue (Noise autentica o Peer ID, e `on_mls` é por peer). **Não li** se o caminho da caixa postal (`MailboxEvent::Retrieved`, `chat.rs:1641`) deixa um terceiro injetar com o `from` de outro; se deixar, o risco é maior e muda a decisão.
- **O que o par ganha** ao forçar: este lado refaz a conversa, mas o P16 só aceita a **mesma identidade** da conversa antiga (e revogação/regime v4 conferidos), então não há troca de interlocutor; o custo é a mensagem em voo. A janela de 1 por peer limita a repetição.
- **Device pareado** não se recupera de um banco perdido (ADR 0040, não emite certificado). **Decisão do dono (2026-11-21)**: no `Stale` ele **só avisa, como hoje** (não apaga a conversa), até se confirmar no código que o `Hello(false)` dele é aceito pelo outro lado. Apagar sem poder refazer o deixaria sem conversa.
- **Caixa postal**: **decisão do dono (2026-11-21)**: verificar na etapa de testes (ler `MailboxEvent::Retrieved`, `chat.rs:1641`, e como o `from` é autenticado) antes de implementar. Se um terceiro puder injetar, a recuperação automática vale só para `Stale` vindo de mensagens da **conexão direta**; o `Stale` por caixa postal só avisa.
- **Interação com o P36**: depende de a reconexão ocorrer; se o outro lado tiver a conexão antiga meio-aberta, vale o que já está registrado no P36.

## Atualização: implementação e auditorias (PR #281, 2026-11-21)

- **O desenho mudou** em relação ao acima, por leitura do código e por duas auditorias (`security-auditor`):
  - **`recovering`** (`HashMap<PeerId, (IdentityId, u64)>`) e a identidade antiga em `expected`: sem isso o portão fechado barrava o retorno (`knows`), ninguém rediscava (`redial_want` exige conversa e fila) e o par escolheria a identidade.
  - **Só com o peer conectado**, sem irmão (`household` ou `is_sibling`), sem contato desconfiado e **não** quando a mensagem veio da caixa postal (`from_mailbox`): a caixa postal entrega fora de ordem e o commit atrasado ainda salvaria a conversa; o P16 só roda pela conexão direta.
  - **Janela** de 600 s por peer (`stale_recovered`) e **TTL** de 24 h da marca (`expire_recovering`); `untrust` a cancela; `DialFailed` não apaga o `expected` de quem está em recuperação; o `ConversationReset` só sai se a identidade nova é a antiga.
  - **`Node::disconnect`** (`Swarm::disconnect_peer_id`).
- **Decisões do dono**: pareado só avisa (cumprido via `is_sibling` e `household`; a premissa "pareado não se recupera de banco perdido" é outra coisa); caixa postal: verificado e restrito (o `from` é autenticado pelo seal, não há injeção de terceiro; o risco era a ordem).
- **Provas** (CI): testes primeiro (run 44854814614 e seguintes: 8 do `chat` e 2 do transporte falharam); depois da reauditoria, 3 testes novos falharam antes da correção e passaram depois (commit `a9317c1`). **Mutação de propósito** (PRs descartáveis #282 e anteriores): a da janela derrubou `a_second_stale_inside_the_window_only_warns`; a das duas mutações (sem `connected`, sem `expected`) derrubou o CI, mas **o log não identificou quais testes falharam**.
- **Auditorias**: 1ª (0 críticos, 1 alto, 4 médios, 3 baixos) e 2ª (0 críticos, 0 altos, 3 médios, 8 baixos); os 3 médios da 2ª estão corrigidos, **sem nova auditoria** sobre a correção.
- **Limites aceitos**: o par conectado ainda apaga a conversa com 33 mensagens de época futura (sem chave), no máximo uma vez a cada 10 min, perdendo o que estava em voo (a outbox e o histórico ficam); a marca é só em memória (reiniciar antes de reconectar perde o pino da identidade); resíduo do P36 se o outro lado tem a conexão antiga meio-aberta; peer revogado durante a recuperação fica rediscado até 24 h (`drop_peer` não limpa `recovering`).
- **Atualização (2026-11-22, PR #285)**: pendências **(3) e (4) fechadas**: `ChatEvent::Stale { peer, recovering }` diz se a conversa foi abandonada (a CLI tem duas frases) e `crates/chat/tests/stale_recovery.rs` cobre o ciclo (Stale, queda, discagem, `ConversationReset` com a mesma identidade nos dois lados, mensagens nos dois sentidos). Duas ressalvas: a rediscagem automática não foi exercitada (loopback não é discado; o teste disca à mão) e a prova do `is_sibling` com `household` vazio ficou de fora. Faltam (1) e (2).
- **Atualização (2026-11-22, PR #286)**: pendência **(5) fechada**: dois testes do transporte provam que `Node::disconnect` fecha **todas** as conexões com o peer (duas discagens ao mesmo endereço: uma queda por lado, e nada sobra) e também a conexão por **circuito de relay** (os dois lados veem `PeerDisconnected`). Eram testes de caracterização, sem código de produção; só loopback e relay local. Faltam (1) e (2).
- **Atualização (2026-11-22, PR #288)**: pendências **(1) e (2) fechadas**; **o ADR 0041 não tem mais pendência aberta** (só os limites abaixo).
  - **(1) Mutação lida** (um commit por mutação, nomes lidos no log completo): sem `connected` → falharam `stale_without_a_direct_connection_only_warns` e `the_future_buffer_is_bounded_and_reports_stale`; sem `expected` → falharam `a_failed_dial_keeps_the_pinned_identity_of_a_recovering_peer`, `a_recovering_peer_still_passes_the_closed_gate`, `disconnecting_keeps_the_addresses_of_a_recovering_peer` e `stale_drops_the_conversation_and_keeps_the_outbox`. O teste de integração passou nas duas: ele só prova o caminho feliz.
  - **(2) Reauditoria** (`security-auditor`, 0 críticos, 0 altos, 0 médios, 3 baixos, 2 informativos): os 3 médios do #281 estão **fechados**. **Achado 1 (baixo), corrigido**: `on_device_list` apagava o `expected` de um peer em recuperação se `dial_peer` falhasse de forma síncrona; agora `release_expected` só remove fora de recuperação (o teste cobre o helper; o `Err` real do dial não dá para forçar em teste unitário).
  - **Limites aceitos**: (2) a 33ª mensagem pela conexão direta abandona a conversa mesmo com 32 vindas da caixa postal (o `from_mailbox` só olha a última; corrigir pede origem por entrada do buffer, junto do P9 d); (3) se `Conversation::delete` ou `forget_conversation` falharem, a conversa volta no reinício e o `Stale` dispara de novo sem janela (a marca é só memória, e o evento diz "abandonada" mesmo assim); (4) a janela de 600 s e o TTL de 24 h usam `now_secs()` (relógio de parede) em vez de `Instant`; (5) o retorno de `Node::disconnect` é ignorado; `accept()` sobrescreve o `expected` por ação do dono; cada device da mesma identidade tem janela própria (N devices, N abandonos por janela).
  - **Não verificado pelo auditor**: `crates/chat/tests/stale_recovery.rs` e os testes do #286 não lidos; `credential::verify` tomado como correto; backend da caixa postal (o `from` autenticado pelo selo é afirmação do ADR).
- **Pendências (abertas na `main`; o #281 foi mesclado sem fechá-las; ver a atualização acima)**: (1) refazer a mutação **lendo quais testes falham**; (2) reauditar a correção dos 3 médios; (3) teste de integração do ciclo completo (Stale, queda, reconexão, P16 refaz, `ConversationReset` com a mesma identidade) e prova do `is_sibling` com `household` vazio; (4) o aviso da CLI (`main.rs`, `ChatEvent::Stale`) diz sempre "abandonada", falso quando só avisa: o evento precisa dizer se recuperou; (5) o `Node::disconnect` com várias conexões e por relay não foi testado.

## Atualização: o teto de 32 medido com a ordem de entrega da caixa postal (P9 d, 2026-11-22)

- **Modelo**: o relay Nostr devolve o backlog por `created_at` decrescente e o NIP-59 sorteia o `created_at` do gift wrap (até 2 dias atrás), então a ordem de chegada é aleatória em relação à de envio. A ordem do teste vem de gift wraps **reais** (`nostr::wrap`); o **relay não foi medido** (fica no P33).
- **Medido** (CI, 30 execuções por K; o teste definitivo roda 12): commit seguido de K mensagens, entregues nessa ordem, peer desconectado.

| K | `Stale` falso | máx. retido | entregues na 1ª passada | `TooOld` (contado) | após o reenvio |
|---|---|---|---|---|---|
| 8 | 0/30 | 8 | 8,0 de 8 | 0,0 | 8 de 8 |
| 32 | 0/30 | 32 | **12,9 de 32** | 19,1 | 32 de 32 |
| 48 | 10/30 | 32 | 10,5 de 48 | 26,5 | 48 de 48 |
| 64 | 12/30 | 32 | 10,5 de 64 | 40,3 | 64 de 64 |
| 128 | 25/30 | 32 | 10,4 de 128 | 65,9 | 128 de 128 |

- **Achado 1 (o limite real é 10, não 32)**: `OUT_OF_ORDER_TOLERANCE = 10` (`conversation.rs:15`) e `on_mls` trata `CryptoError::TooOld` como "seguro ignorar". Na entrega embaralhada, a mensagem mais de 10 gerações atrás da maior já decifrada some **sem `Dropped`** e sem `Stale`. Com K = 32: 12,9 entregues + 19,1 `TooOld` = 32. O contador (temporário, já removido) confirma o mecanismo que eu só tinha por leitura.
- **Achado 2 (`Stale` falso)**: com K >= 48 o teto de 32 estoura sem ninguém ter perdido commit. Com o peer conectado e a mensagem vinda da caixa postal só avisa (médio 1 da reauditoria); com o peer conectado e a 33ª pela conexão direta, abandona a conversa (limite aceito 2, acima).
- **Achado 3 (a outbox recupera)**: o `Delivered` vem do ack do transporte, e o depósito na caixa postal só vira `Deposited`; a mensagem fica na outbox. Reenviada com bytes novos e o mesmo `MessageId`, chegou **sempre** e **sem repetição** (`mark_seen`, que só marca depois de decifrar). A mensagem perdida por `TooOld` **não é perda definitiva**, desde que a outbox do remetente a reenvie: o custo é **latência** (nova varredura ou reconexão direta), não medida.
- **Não medido**: relay real (ordem, `limit` e `since` do REQ); o tempo até o reenvio; backlog que atravessa **mais de uma** rotação; o remetente sem a outbox (banco perdido).
- **(Superado pela correção abaixo: o K > 8 não é alcançável e o P37 foi rebaixado.) Decisão pendente (P37)**: ordenar o backlog (por época e geração) antes de entregar ao MLS, ou subir a tolerância. Subir `OUT_OF_ORDER_TOLERANCE` guarda mais chaves antigas (menos sigilo para a frente): toca cripto (§4), pede ADR próprio e a confirmação do dono. Esta atualização não muda código de produção.
- **Teste**: `out_of_order_mailbox_backlog_loses_messages_silently_but_the_resend_recovers_them` (`chat.rs`) fixa o estado atual (K = 8 chega inteiro; K = 32 perde sem `Dropped` e sem `Stale`; o reenvio recupera tudo sem repetir); a asserção `lost_sum > 0` é para **inverter** na correção do P37.

## Correção da atualização acima: o cenário medido não é alcançável (P37, 2026-11-22)

- **Erro meu**: a tabela de K = 32 a 128 modela uma entrega que um remetente `Chat` **não produz**. `MAILBOX_WINDOW = 8` (`chat.rs:223`) limita a 8 os depósitos sem ack por peer, de propósito abaixo das 10 gerações de tolerância do MLS (o comentário da constante diz isso). Só o K = 8 da tabela é alcançável, e ele chegou inteiro. Eu li o `OUT_OF_ORDER_TOLERANCE`, mas não o `MAILBOX_WINDOW`, antes de afirmar "perde 60% na 1ª passada" e de abrir o P37 como médio; o teste ponta a ponta com 40 mensagens (que eu ia escrever para validar a correção) nem conseguiu depositar as 40, e foi isso que expôs o erro.
- **O que a tabela ainda vale**: o limite do `Chat` diante de uma entrega que ignore a janela (outro cliente, remetente hostil). Os números de `TooOld` e de `Stale` falso estão certos para essa entrada, que é só inalcançável pelo uso normal.
- **Medido o cenário alcançável** (CI, 200 execuções por caso): 8 mensagens e a varredura por idade recifrando-as (`SWEEP_BUDGET` = 8 rodadas; com retenção de 7 dias, ~5 cópias de cada coexistem no relay), entregues em ordem aleatória: com 1, 2, 3, 5 e 8 cópias (até 64 eventos) **0 mensagens perdidas em 1.000 execuções**. Cada mensagem tem várias cópias e basta uma decifrar. Teste: `redeposited_copies_in_random_order_never_lose_a_message` (`chat.rs`).
- **Decisão do dono** (2026-11-22, depois da medição): **não implementar** a ordenação do backlog pelo horário do rumor (tag de ordem, lote até o `EOSE`, prazo e teto), que ficaria como defesa em profundidade com superfície grande (formato do rumor, sessão do relay, relay hostil) para um risco que o `Chat` já evita pela janela. **P37 rebaixado para baixa**; só reabre se aparecer perda real (relay real no P33, irmãos, outro cliente).
- **Não medido**: relay real (ordem e `limit` do REQ), irmãos (cada device tem a própria cadeia de gerações), backlog com mais de uma rotação de chave.
- **Aprendizado**: antes de medir um limite, ler quem o impõe do outro lado (aqui, o remetente). A janela de 8 já existia no código e no comentário.

## Não verificado

Nada foi rodado. O comportamento do `disconnect_peer_id` (fecha todas as conexões? emite `PeerDisconnected` em ambos os lados?), o caminho da caixa postal e o caso do device pareado ficam para a etapa de testes.
