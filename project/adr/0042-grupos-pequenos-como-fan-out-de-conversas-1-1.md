# ADR 0042 — Grupos pequenos como fan-out de conversas 1:1 (P9 b)

- **Status**: aceito como **direção de desenho** (2026-11-22, escolha do dono); **nada implementado**. A Fase 5.1 parte daqui; o formato de mensagem de grupo e o teto de membros ainda serão decididos com medição.
- **Relacionados**: ADR 0001 (committer designado, 1:1), ADR 0006 (multi-device: pares e fan-out; o `crypto` recusa mais de 2 membros), ADR 0039 (portão de convite), ADR 0040 e 0041 (re-handshake e `Stale` por par), P9 (b), P10 (b: papéis e log de membership; a: histórico), P33.
- **Área de segurança** (criptografia, AGENTS.md §4): este ADR **não muda código**; só registra a direção. Não inventa protocolo: reusa a conversa 1:1 já auditada.

## Contexto

O P9 (b) perguntava como um **grupo** MLS se ordena sem Delivery Service: troca de committer quando o committer some, `Add`/`Remove` por proposta (hoje recusados), e `Stale` forçado por membro malicioso. O 1:1 resolve isso por construção (ADR 0001: committer é o de menor Peer ID, nunca há dois committers) e o multi-device também (ADR 0006: pares device↔device, cada par com o próprio grupo MLS de 2 membros; `MAX_MEMBERS = 2` em `crates/crypto/src/conversation.rs:20`). A Fase 5 ("uso diário real por um grupo pequeno") precisa de grupos.

O RFC 9420 pressupõe um DS que dê ordem total aos commits; o RFC 9750 (arquitetura) diz que, sem DS, o cliente precisa implementar essa lógica. As duas propostas públicas para o caso descentralizado são **Internet-Drafts individuais, não adotados** (conferir o estado no datatracker antes de depender de qualquer um):

- **DMLS** (Kohbrok, `draft-kohbrok-mls-dmls`, baseado no FREEK de Alwen et al.): guarda forks e fica com o cujo primeiro commit divergente tem a *confirmation tag* maior; o custo declarado é guardar chaves antigas, o que reduz o sigilo para a frente.
- **Distributed-MLS** (Xue et al., `draft-xue-distributed-mls`): cada participante é dono de um *send group* e decide a ordem dos próprios commits; dispensa consenso global de ordem.

Nenhuma tem implementação no `openmls`.

## Critérios (em ordem)
Correção e segurança (não inventar protocolo, §4); simplicidade; custo de reverter; manutenção; desempenho; dificuldade de testar.

## Decisão

**Um grupo pequeno é um conjunto de conversas 1:1**: cada par de membros usa a conversa MLS de 2 membros que já existe. Enviar ao grupo é um **fan-out**: a mesma `Message` (mesmo `MessageId`, para `parent` e dedupe valerem entre membros) entra na outbox de cada membro e é cifrada na conversa daquele par, exatamente como o ADR 0006 já faz entre devices de uma identidade.

Consequências diretas para o P9 (b):
- **Troca de committer**: some. Cada conversa tem 2 membros e o committer é o de menor Peer ID.
- **`Add`/`Remove` por proposta**: some. Entrar no grupo é abrir uma conversa 1:1 com cada membro (convite e portão do ADR 0039); sair é fechá-la. O MLS nunca vê um terceiro membro, então o limite de 2 do `crypto` fica.
- **`Stale` forçado por membro malicioso**: fica **por par**, com a defesa já feita (ADR 0041: janela de 600 s, só com o peer conectado, identidade da conversa antiga fixada). Um membro só consegue derrubar a conversa dele com cada outro membro, não a dos demais.

## Alternativas

| Opção | Prós | Contras | Custo de reverter |
|---|---|---|---|
| **A. Fan-out de 1:1 (escolhida)** | Reusa `crypto`, outbox, caixa postal, `Stale`/P16 e o portão de convite; nenhum commit concorrente; nada novo em cripto | N-1 cifragens e depósitos por mensagem; sem estado de grupo compartilhado (ver equivocação); sem ordem total | **Fácil agora** (só este ADR); **médio/difícil depois** de existir formato de mensagem de grupo (vira protocolo) |
| B. Grupo MLS com committer designado e rodízio (estende o ADR 0001) | Barato por mensagem (O(log N)); consistência de grupo | Committer offline trava `Add`/`Remove`; permissão de proposta (P10 b); `Stale` forçado por membro derruba o grupo todo; rejoin por External Commit; tirar o limite de 2 do `crypto` (§4) | Difícil (cripto e protocolo) |
| C. MLS fork-resiliente (DMLS) | Sem committer; tolera fork | Draft individual sem implementação; guardar chaves antigas reduz o sigilo para a frente; seria protocolo quase novo (§4) | Difícil |
| D. Não fazer grupos | Zero custo | Não atende a Fase 5 | — |

**Por que A**: é a única que não adiciona superfície criptográfica nova e que o projeto já provou em outro lugar (multi-device). B e C trocam custo de banda por complexidade de protocolo exatamente na parte em que o §4 manda preferir o simples e auditável.

## Consequências

- **Equivocação (limite aceito, não resolvido)**: sem estado de grupo compartilhado, um remetente malicioso pode contar histórias diferentes a membros diferentes (um grupo MLS de verdade impediria). A mitigação é um log de membership e de mensagens encadeado por hash (P10 b); até lá, grupo só entre quem já confia no outro.
- **Custo**: N-1 cifragens e depósitos por mensagem. Cada peer tem a janela `MAILBOX_WINDOW = 8` de depósitos sem ack (`chat.rs:223`) e a varredura por idade recifra por par; o **teto de membros** (hipótese: 10 a 20) **será medido na Fase 5**, não é decidido aqui.
- **Sem ordem total**: cada membro vê as mensagens do grupo na ordem em que chegam. Thread por `parent` e dedupe por `MessageId` seguem valendo.
- **Sigilo**: o pós-comprometimento (PCS) é por par, não do grupo inteiro; o sigilo para a frente é o do 1:1 já auditado.
- **Membros e papéis**: a lista de membros e as permissões ficam para o P10 (b); o histórico de quem entra depois, para o P10 (a).
- **Reversão preparada**: o formato de mensagem de grupo deve separar o **identificador do grupo e o `MessageId`** do **transporte por par**, para um grupo MLS (opção B) poder substituir o fan-out depois sem mudar o que a aplicação vê.

## O que continua aberto

Equivocação e o log de membership (P10 b); teto de N e custo medido (Fase 5); como anunciar a lista de membros e quem pode convidar; histórico para quem entra (P10 a); grupos grandes e canais (o O(log N) do MLS pode voltar como otimização, opção B).

## Não verificado

Nada foi rodado; é decisão de desenho. O estado atual dos drafts do IETF vem de uma busca na web (não li os textos completos). O custo real de N-1 depósitos com a janela de 8 não foi medido.
