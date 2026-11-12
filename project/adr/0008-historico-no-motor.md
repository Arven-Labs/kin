# ADR 0008 — O motor guarda o histórico das conversas (P10a, fatia 1)

- **Status**: aceito (Sessão 20, 2026-11-11). Implementado no PR #84 (fatia 1 de 3: guardar; as fatias 2, sincronizar entre irmãos,
  e 3, CLI e laboratório, vêm depois).
- **Relacionados**: ADR 0006 (multi-device: pares device↔device, espelho `Sent`); P10(a) e P24 em `PENDING.md`; ADR 0002
  (outbox: fila de saída, apagada na entrega).

## Contexto

O P10(a) pergunta como um device que entra depois recebe o histórico. A leitura do código mostrou que **o motor não guarda
histórico**: a outbox é só a fila de envio (a linha some quando a mensagem é confirmada), `kin_seen` e `kin_seen_mirror` guardam
só ids, e o texto sai como `ChatEvent::Received`/`SentElsewhere` para uma interface que ainda não existe (Fase 5). Sem
histórico no motor não há o que sincronizar. O MLS também não ajuda: quem entra no grupo depois não decifra o que veio antes (é
de propósito, sigilo de encaminhamento). Mas o 3.5 usa **um par MLS por device↔device** (ADR 0006), então um device novo pode
pedir o histórico ao irmão pela conversa que os dois já têm, sem depender do MLS para isso.

## Decisão

1. **O motor guarda o histórico** (decisão do dono, Sessão 20): tabela `kin_history`, **schema v6**, aditiva
   (`CREATE TABLE IF NOT EXISTS`, o mesmo padrão das v3 a v5), no SQLite do `Outbox`.
   `seq` autoincremento, `contact` (protobuf da identidade), `outgoing`, `id`, `parent`, `sent_at_ms`, `text`,
   `UNIQUE (contact, id)` e índice `(contact, seq)`.
2. **A chave é a identidade do contato, não o Peer ID.** A conversa lógica atravessa devices (os dele e os meus); só a identidade
   é estável entre eles. É isso que permite, na fatia 2, juntar o histórico de dois irmãos sem duplicar.
3. **Uma linha por mensagem lógica.** `INSERT OR IGNORE` por `(contact, id)`: o mesmo `MessageId` por dois caminhos (enviei e um
   irmão espelha; reentrega do transporte) vale uma linha, e **a primeira gravação vence** (inclusive a direção).
4. **Três ganchos de gravação** em `chat.rs`:
   - recebida (`on_mls`, `Payload::Message`, depois de `mark_seen`): `contact = sender` (a minha identidade, se vier de um irmão);
   - enviada (`send` e `send_to`, **antes** de enfileirar): `send_to` grava uma vez, não por cópia; `send` só grava se a conversa
     tem exatamente uma identidade do outro lado (`sole_identity`, a mesma regra de `device_list_for`);
   - espelho (`on_sent`, depois da checagem de autoria e do `mark_seen`): `contact = to`, enviada. Um espelho recusado
     (`UnauthorizedMirror`, `Malformed`) **nunca** entra no histórico.
5. **API**: `Chat::history(&IdentityId, after: Option<u64>, limit) -> Vec<HistoryEntry>`, do mais antigo ao mais novo, com `seq`
   como cursor de paginação (`HistoryEntry { seq, outgoing, message }`, exportado em `lib.rs`).

## Alternativas descartadas

- **O app guarda e o motor só sincroniza por uma interface** (`HistoryStore`). Mantém o armazenamento na camada "certa", mas é
  abstração sem app para exercitá-la (YAGNI) e a CLI não teria histórico real para mostrar.
- **Adiar para a Fase 5.** Custo zero hoje, mas deixa o 3.5 sem histórico e o item aberto.
- **Chave por Peer ID.** Quebra quando o contato (ou eu) troca ou acrescenta um device.
- **Gravar a recebida antes do `mark_seen`.** Pelo desenho é equivalente (o `UNIQUE` já descarta o repetido), então um teste não
  distinguiria as duas ordens (dedução, não rodei essa mutação). Ficou depois do `mark_seen` para o guarda da reentrega ser
  explícito.

## Consequências

- **Texto em claro em repouso** no mesmo arquivo 0600 que já guarda a outbox (e, ao lado, as chaves MLS): o histórico inteiro
  das conversas fica legível por quem ler o arquivo. Segue o P13 (sem cifra em repouso); não é pior que o estado de hoje, mas
  cresce com o uso.
- **A tabela cresce sem teto.** Não há poda nem retenção (P26). *(Resolvido na Sessão 21: teto de linhas por contato, ADR 0010.)*
- **Sem downgrade**: um build v5 recusa o banco v6 (`migrate`), como nas v3 a v5.
- **"Notas para mim" são relativas ao device**: quem envia grava como enviada e o irmão que recebe grava como recebida (é a mesma
  linha lógica vista dos dois lados). A fatia 2 precisa tratar isso ao juntar históricos (a primeira gravação vence).
- Habilita a fatia 2: o histórico é o que o irmão antigo enviará ao device novo, dentro da conversa MLS entre os dois, e só a
  de irmão (`sender` da própria identidade, a mesma checagem do espelho). Essa fatia lê dado de rede e terá tetos e casos de borda.
