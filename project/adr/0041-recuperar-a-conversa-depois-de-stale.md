# ADR 0041 — Recuperar a conversa depois de `Stale` (P9 c)

- **Status**: **implementado e mesclado (PR #281, 2026-11-21), com pendências**: o dono mesclou antes de fechá-las; ver "Atualização: implementação e auditorias" no fim. Área de segurança (criptografia, estado de sessão, AGENTS.md §4).
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
- **Pendências (abertas na `main`; o #281 foi mesclado sem fechá-las; ver a atualização acima)**: (1) refazer a mutação **lendo quais testes falham**; (2) reauditar a correção dos 3 médios; (3) teste de integração do ciclo completo (Stale, queda, reconexão, P16 refaz, `ConversationReset` com a mesma identidade) e prova do `is_sibling` com `household` vazio; (4) o aviso da CLI (`main.rs`, `ChatEvent::Stale`) diz sempre "abandonada", falso quando só avisa: o evento precisa dizer se recuperou; (5) o `Node::disconnect` com várias conexões e por relay não foi testado.

## Não verificado

Nada foi rodado. O comportamento do `disconnect_peer_id` (fecha todas as conexões? emite `PeerDisconnected` em ambos os lados?), o caminho da caixa postal e o caso do device pareado ficam para a etapa de testes.
