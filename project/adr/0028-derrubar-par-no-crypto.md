# ADR 0028 — O `crypto` para derrubar o par e a continuidade do `Update` (P30 fatia 3d-ii-a)

- **Status**: aceito e implementado no `crypto` (Sessão 26, 2026-11-15). Área de segurança (revogação e autenticação, AGENTS.md §4):
  auditado pelo `security-auditor` (0 críticos, 2 importantes; tabela no fim) e com 6 mutações (6 pegas). Faltam a **3d-ii-b** (o `chat`
  derruba o par) e a **3d-ii-c** (reconsultar na reabertura), **bloqueadoras do P30**. Nada chama o que esta fatia adiciona (exceto o
  próprio `decrypt`).
- **Relacionados**: ADR 0023 (store), 0024/0025 (canal), 0027 (`verify` recusa revogado), P30, P31 (q).

## Contexto

A 3d-i recusa um device revogado **na entrada**. Derrubar quem já está numa conversa é trabalho do `chat`, mas ele precisa de três coisas
do `crypto`: ler as 3 chaves do par (a chave MLS dele não era exposta), apagar um grupo e o label dele, e saber que o par não pode "virar"
outro certificado por um `Update` (P31 q: o MLS não vincula a credencial do leaf novo à antiga).

## Decisão

1. **`Conversation::peer_certificates()`**: certificados dos outros membros, já com a assinatura da cadeia conferida (`certificate_of`,
   ex-`parse`, agora `pub(crate)`). O `chat` os passa a `CryptoDevice::is_revoked`, que confere identidade e as 3 chaves.
2. **`Conversation::delete(self)`** (`MlsGroup::delete` do openmls 0.9) e **`CryptoDevice::forget_conversation(label)`**
   (`Provider::forget`, idempotente). Ordem para quem derruba: **grupo primeiro, label depois**. Uma queda no meio deixa só um label sem
   grupo, que a reabertura já ignora (`Conversation::load` devolve `None`, "o handshake refaz"); o inverso deixaria um grupo órfão. A
   chave de assinatura do device não é tocada.
3. **`Conversation::verify_continuity`** em `decrypt`: o leaf novo de um `Update` (proposta solta, `UpdatePath` do commit e as propostas
   `Update` que o commit efetiva) passa por `credential::verify` com a **identidade e o Peer ID que o membro tem hoje no grupo**
   (`member_at`), sem tombstone (ADR 0027), e com a **chave de assinatura e a chave Nostr iguais** às atuais, senão
   `Error::DeviceKeysChanged`. Roda **antes** de `store_pending_proposal` e de `merge_staged_commit`. O `Sender` é o do enquadramento (proposta
   solta e `UpdatePath`) ou o de cada proposta (`update.sender()`); sender que não é membro vira `Unsupported`, membro ausente vira `Malformed`.

## Alternativas descartadas

- **Aceitar chave MLS diferente no `Update`** (a primeira versão do plano, "é para isso que o Update existe"): a auditoria mostrou que isso
  deixava um certificado válido da mesma identidade e do mesmo device trocar a chave Nostr (para onde o mailbox entrega) em silêncio. O
  dono escolheu as **4 chaves iguais**: a rotação de chave MLS com certificado novo ainda não existe e a fatia 4 (frescor) vai precisar de
  certificado renovado **com as mesmas chaves**, que passa. Rotação de chave exigirá relaxar esta regra com decisão própria.
- **Certificado idêntico em bytes**: bloquearia a renovação da fatia 4.
- **Casar o par por identidade + Peer ID + chave Nostr** (sem expor a chave MLS): não cobre a terceira chave e depende do teto de 2 membros.
- **Transação em volta do `delete`**: ver a lacuna abaixo; adiado.

## Consequências

- A rotação legítima (`propose_rotation`, `rotate_keys`) segue aceita: ela usa o mesmo certificado.
- O `commit_pending` do committer não reverifica as propostas guardadas (foram verificadas ao entrar na fila); um tombstone que chegue entre
  o ingest e o commit não é visto aí (a 3d-ii-b derruba o par quando o tombstone chega).
- Quem chama `delete` tem de garantir **uma instância por id**: um `Conversation::load` depois (o `CryptoDevice` clona o `Provider`) pode regravar
  linhas parciais.

## Lacunas conhecidas

- **`delete` não é transacional (auditoria I2)**: `MlsGroup::delete` faz uns 9 deletes avulsos; uma queda no meio deixa segredos de época
  órfãos que `load` não enxerga. O openmls não apaga os pares de chaves HPKE do leaf, o banco não é cifrado (P13) e o SQLite não usa
  `secure_delete` (e o WAL retém dados): **o delete não apaga os segredos de fato**. Pendência (P31 an): transação na mesma conexão (se
  o `openmls_sqlite_storage` aceitar), apagar as chaves do leaf, `PRAGMA secure_delete` e checkpoint do WAL.
- **As propostas `Update` dentro de um commit** (`staged.update_proposals()`) têm a checagem mas **nenhum teste a alcança** com 2 membros: a
  proposta já foi checada ao entrar na fila. A mutação desse ponto não é pega; vale quando a Fase 5 levantar o teto.
- **`Storage` transitório em `is_revoked` dentro de `decrypt`** descarta um commit com `UpdatePath` cujo ratchet o `process_message` já
  consumiu (o reenvio vira `Duplicate`): já existia no `Add` e a fatia amplia o alcance (P31 ao).
- Sem teste de sender que não é membro nem de que a época e a fila ficam intactas em todo caso de rejeição (só no de identidade diferente).

## Verificação (resultado real)

| Etapa | Run | Resultado |
|---|---|---|
| Testes com esqueleto | 43772348947 | 7 de 7 falharam (3 de `Update` pelo `assert!`: o código aceitava o forjado; 4 de API); o de rotação legítima passou |
| Implementação | 43772955286 | verde |
| Testes das 4 chaves com esqueleto | 43776826447 | 3 de 3 falharam; o 4º falhou por erro **meu** no teste (a Bia, dona da proposta forjada, é quem não podia cifrar): corrigido |
| Depois da auditoria | 43777160424 | verde |

**Mutações** (branches descartáveis, 6 de 6 pegas): `peer_certificates` devolve o próprio membro (43773323829, 2 testes); `delete` não
apaga (43773338458, 1); `forget` não apaga (43773349106, 1); `Update` proposto sem a checagem (43773360125, 2); `UpdatePath` sem a checagem
(43773372043, 1); sem conferir assinatura e Nostr (43777489583, 3). Duas mutações precisaram de ajuste para o clippy não barrar antes dos testes
(`mut self` sem uso e `&&` com `false`).

**Auditoria** (`security-auditor`, somente leitura): sem bypass da checagem; I1 (só identidade e Peer ID) corrigido com as 4 chaves; I2
(`delete` não transacional e segredos que sobram) virou a lacuna acima, por decisão do dono. As sugestões viraram P31 (an) a (aq).
