# ADR 0043 — Conexão antiga meio-aberta: o `a` nunca sabe que o `b` perdeu a conversa (P36)

- **Status**: **proposto** (2026-11-22). Desenho por leitura de código; **nada implementado, nada rodado**. A opção 4 toca handshake e autenticação (AGENTS.md §4) e muda o formato de fio: só vira código com a confirmação explícita do dono, em plan mode, com testes primeiro.
- **Relacionados**: ADR 0040 (re-handshake quando um lado perde o estado; adendos P35 e P36), ADR 0041 (`Stale` e `Node::disconnect`), P16, P35, P36, P33. Medição: PRs #264 (testes `..._not_rehandshaked_yet`), #267 (tentativa, fechado) e #293 (cenários `stale-connection-rst` e `-silent` do lab).

## Contexto

O `b` perde o `mls.sqlite`, reinicia com a mesma identidade e reconecta enquanto o `a` ainda tem a conexão antiga de pé (queda sem FIN, troca de NAT). Medido no lab (#293, 6 rodadas, as duas ordens de Peer ID, idêntico nas 6): a conexão antiga **não cai em 180 s**, a conversa **nunca é refeita**, o `a` imprime `(entregue ...)` e o `b` imprime `(descartado ...: sem conversa com o peer)`. Como a outbox do `a` remove a entrada no ack, **a mensagem se perde de vez**.

Mecanismo, por leitura (confiança alta para o trecho do código, média para a causa de cada cenário):

1. O transporte só emite `PeerConnected` quando `num_established == 1` (`crates/transport/src/node.rs:499`); a conexão nova é só `PeerRoute`.
2. O `Hello(false)` do `b` ("não tenho conversa") só é aceito se o peer está em `hello_pending` (`crates/chat/src/chat.rs:1905`), e `hello_pending` só é preenchido na 1ª conexão. É o gate do P35: sem ele, um `Hello` velho (montado antes de a conversa existir, chegado depois: os substreams não têm ordem) marcava `lost_state` e travava a fila.
3. Logo o `Hello(false)` do `b` chega e é **ignorado**. O `a` segue no grupo antigo, e o ack de `request-response` é de transporte: o `b` responde ack mesmo descartando a mensagem.
4. Agravante: o `libp2p-request-response` 0.30 manda o pedido para a conexão `request_id % nº de conexões`, então metade dos pedidos cai na conexão morta e só falha no timeout (10 s).

A conclusão que importa: **o defeito não é a conexão morta, é a falta de um evento no chat** ("o peer perdeu o estado"). Hoje esse evento só nasce na 1ª conexão.

## Critérios (em ordem)
Correção e segurança (§4); não regredir o que o Lab já prova (o #267 derrubou 8 cenários); simplicidade; custo de reverter; dificuldade de testar.

## Alternativas

| Opção | Prós | Contras | Veredito |
|---|---|---|---|
| 1. Fechar as conexões mais antigas no `node.rs` quando chega uma extra | Mexe só no transporte; resolve o round-robin | **Não cura sozinha**: o `Hello(false)` já foi ignorado e ninguém o reenvia; só curaria se o fechamento levasse a `PeerDisconnected` e a uma reconexão (padrão do ADR 0041). Mata a conexão extra legítima (hole punching troca relay por direta). Na discagem simultânea cada lado pode eleger uma conexão diferente e os dois ficam com zero (desempate por Peer ID não serve: a morta pode ser a mantida) | Descartada |
| 2. `PeerRoute` marca `hello_pending` e reenvia `Hello`/`KeyPackage` | Pouco código | É a alternativa do ADR 0040 (adendo P35), **implementada no #267 e regrediu** `chats_find_each_other_via_mdns_and_talk` e 8 cenários do Lab: um `Hello(false)` velho volta a marcar `lost_state` | Descartada (medido) |
| 3. Discriminador de **conexão** no `Hello` | Muda pouco o chat | A conexão não é o que separa um `Hello` velho de um legítimo (os dois vêm de conexões novas); muda o fio sem resolver | Descartada |
| 5. `ping` do libp2p mais evento "fechou, ainda há outra" | Encurta a morte da conexão morta | `PeerDisconnected` só sai com `num_established == 0` (`node.rs:513`); o evento novo muda o contrato transporte/chat e se aproxima da opção 2 (reavaliado em 2026-11-21) | Descartada |
| **4. Identidade da instância de estado (`state_id`) no `Hello`** | Corrige o `Hello` **por conteúdo**, em qualquer conexão e nas duas ordens de Peer ID; não depende de `hello_pending` para detectar; o `Hello` velho do P35 traz o mesmo id e continua ignorado | Muda o fio e a persistência; toca §4; não cobre estado antigo restaurado (ver limites) | **Direção proposta** |

## Decisão proposta (opção 4)

- O `b` gera um `state_id` aleatório de 16 bytes (CSPRNG do projeto, como o token do convite) **gravado dentro do `mls.sqlite`**: se o banco some, o id some junto, e o `b` novo gera outro.
- O `Hello` da tag 5 ganha o **bit 8** ("tem `state_id`", 16 bytes depois do token): hoje só os bits 1, 2 e 4 existem e o decoder recusa o resto (`wire.rs:~108`), então um build antigo recusa o `Hello` novo como malformado e o formato antigo continua válido para quem não manda o bit. Compatibilidade com builds antigos: a avaliar no plano (o bit novo pode quebrar um par antigo; hoje, entre builds do mesmo projeto, aceita-se, como no ADR 0040 B7).
- O `a` grava, junto da conversa com o `b`, o `state_id` que o `b` apresentou quando a conversa foi formada (tabela nova na outbox, schema v10, `outbox.rs:29`; hoje v9).
- Regra em `on_hello`: `Hello` com `state_id` **diferente** do gravado, numa conversa 1:1 existente = o `b` perdeu o estado: marca `lost_state` pelo mesmo caminho do P16 (`lost_state_candidate`, `reset_done`, identidade da conversa antiga fixada, tetos). `Hello` com o **mesmo** id, ou sem id (build antigo), não marca. Não depende de `hello_pending`.
- O P35 continua protegido: o `Hello` velho que o P35 ignora foi montado pelo mesmo `b`, na mesma instância, logo traz o `state_id` que o `a` gravou ao formar a conversa.

### Por que acredito que funciona, e o que não sei
Por leitura de `on_hello` (`chat.rs:1891`) e de `wire.rs`. Não rodei nada. Pontos a provar antes de aceitar: (a) o `a` precisa conhecer o `state_id` do `b` **antes** de a conversa nova se formar e gravá-lo na formação, senão a 1ª conexão depois de uma atualização do build não tem referência (regra: sem id gravado, o `Hello` com id grava e não marca); (b) irmãos (multi-device: cada device tem o próprio banco e a própria cadeia) pedem um `state_id` por device, não por identidade; (c) o id **não é segredo**, mas um atacante que o `Hello` forjado consiga fazer passar por "diferente" só consegue o que o P16 já permite hoje (marcar suspeita), e a troca de conversa continua exigindo `KeyPackage` da **mesma identidade** (ADR 0040); isso precisa de auditoria (`security-auditor`) e de teste de entrada maliciosa.

## Limites

- **Backup restaurado** (mesmo `mls.sqlite` antigo, mesmo `state_id`, época antiga): não é detectado por esta regra; é o caso do `Stale` (ADR 0041, P9 c).
- A **janela de 10 s do round-robin** (um pedido cai na conexão morta) continua: esta decisão cura a conversa, não acelera a morte da conexão antiga.
- Só a conexão direta foi medida (#293); relay real, QUIC como conexão antiga e frequência real seguem no P33.

## Testes que vêm primeiro (quando houver código)

1. Os 2 testes `..._not_rehandshaked_yet` (#264), com a asserção invertida: o reset acontece nas duas ordens de Peer ID.
2. Os cenários `stale-connection-rst` e `stale-connection-silent` do lab (#293) invertidos: o `b` recebe a mensagem depois da troca.
3. Guarda de não-regressão do P35: `Hello(false)` velho, com o mesmo `state_id`, **não** marca `lost_state` (os testes das linhas ~7171 a ~7227 de `chat.rs` seguem verdes); discagem simultânea por mDNS e os 8 cenários que o #267 derrubou.
4. Entrada maliciosa: bit 8 sem os 16 bytes, bytes a mais, `state_id` repetido ou trocado entre peers, e fuzz/propriedade do decoder do `Hello`.
5. Depois de implementar: mutação lida (trocar `!=` por `==` na comparação do id) e o nome dos testes que falharam, lido no log.

## Consequências

- Habilita curar o P36 sem mexer no transporte e sem reabrir a corrida do P35.
- Custa: formato de fio novo, schema v10, auditoria de segurança e um id de instância por device.
- Reverter depois de implementado é médio (protocolo e schema); **agora** é grátis (só este ADR).
- O P36 sobe para **Média**: há perda de mensagem medida e a conversa não se recupera sozinha.
