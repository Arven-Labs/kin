# ADR 0015 — O primário roda com a chave de autoridade, sem a mestra (P30 fatia 2b)

- **Status**: aceito e implementado (Sessão 23, 2026-11-13, PR #153). Área de chaves e identidade (AGENTS.md §4): auditado pelo
  `security-auditor` (sem críticos; 1 importante corrigido; tabela no fim) e com 6 mutações pegas. Nenhum formato novo.
- **Relacionados**: ADR 0011 (D), ADR 0012 (cifra em repouso), ADR 0013 (`kin cold`), ADR 0014 (certificado v4), P30 e P31.

## Contexto

O `kin cold init --authority-dir DIR` grava em DIR o `authority.key` (cifrado com a "senha da autoridade") e o `authority.cert`. A
2a criou o `AuthorityIdentity`, que certifica devices em v4. Faltava a CLI normal usar esse diretório: até aqui ela só abria o
`identity.key`, ou seja, a mestra quente.

## Decisão

1. **Detecção pelo conteúdo do diretório**, sem flag: com `authority.key` ou `authority.cert` em `--data-dir`, a CLI roda como
   **primário** (`keys::LocalIdentity::Primary`). Sem eles, roda como sempre, com a mestra. O `Chat` não mudou: já recebia
   `&dyn IdentityProvider`.
2. **Recusas, antes de criar o `device.key`**: `--pair` no primário; `identity.key` junto da autoridade (a mestra não pode ficar no
   primário); só metade da autoridade; `authority.cert` que não é arquivo regular ou passa de 4 KiB **na leitura**; certificado de
   autoridade inválido, de outra chave ou **da própria mestra** (`Error::AuthorityIsMaster`, no `identity`, decisão do dono).
3. **Presença por `symlink_metadata`**: um link quebrado (volume não montado) conta como presente e falha, em vez de o diretório
   parecer novo e ganhar uma mestra nova.
4. **Uma senha**: `files_state` conta o `authority.key`, então a mesma senha abre a autoridade e cifra o `device.key`.
5. **Emissor do certificado em uso**: depois do `Chat::open`, `ensure_current_issuer` exige que o certificado guardado no
   `mls.sqlite` seja da autoridade **atual** (mesmo `KAUT`). Um v3 da mestra (migração no mesmo diretório) ou um v4 de uma
   autoridade anterior (rotate) são recusados (decisão do dono): o primário pede um `--data-dir` novo.
6. **`/pair` no primário** recusa e explica (não há mestra; o pareamento autenticado vem na 2c/2d). Ao abrir, a CLI avisa que o
   certificado é v4 e que builds antigos não o leem (A6 do ADR 0014).

## Alternativas descartadas

- Flag `--primary`: redundante com o conteúdo do diretório e fácil de esquecer.
- Aceitar mestra e autoridade juntas, com aviso: anula o ganho da mestra fria.
- Só avisar quando o certificado em uso é de outro emissor: o primário ficaria fora da cadeia atual quando a fatia 3 revogar a
  autoridade antiga, sem ninguém perceber.

## Consequências

- Migrar um diretório antigo (mestra quente) para primário **no mesmo diretório** não funciona: o `mls.sqlite` tem o v3 e é
  recusado. É preciso um `--data-dir` novo, o que perde as conversas daquele diretório até a fatia 6 (migração).
- Rodar `rotate-authority` no mesmo diretório também exige um `--data-dir` novo para o primário.
- Um link quebrado em `identity.key` ou `device.key` agora também falha no `files_state` do modo mestra (antes contava como
  ausente). É mais seguro e muda uma mensagem de erro.

## O que não consegui validar

- O teste do `/dev/zero` não tem prova de vermelho (antes da correção ele não terminaria).
- FIFO no `authority.cert`: a recusa vem do `is_file`; não há teste.
- Abrir o primário num terminal real, com `KdfParams::DEFAULT` e o prompt (os testes usam `MIN` e senha direta).

## Parecer do `security-auditor` e como foi tratado

Sem críticos. Confirmou a ordem das recusas no `load_primary`, a interação com `decide`/`files_state`, a comparação do
`AuthorityIsMaster` (bytes canônicos) e que o `/pair`/`--pair` não alcançam a mestra.

| Achado | Gravidade | Tratamento |
|---|---|---|
| 1: o primário reaproveitava o certificado guardado sem conferir o emissor; o aviso de v4 podia ser falso | importante | **feito**: `ensure_current_issuer` recusa (decisão do dono); 2 testes que falharam antes |
| 2: teto do `authority.cert` só no `stat` (`/dev/zero`, FIFO, troca entre `stat` e leitura) | sugestão | **feito**: só arquivo regular e `take(LIMITE+1)`; teste sem prova de vermelho |
| 3: `device.key` criado antes de uma recusa do `Chat::open` | sugestão | comentário corrigido; **pendência** (P31), igual no modo mestra |
| 4: `--protect` regrava antes de validar | sugestão | **pendência** (P31), igual no modo mestra |
| 5: `exists()` trata link quebrado como ausente e o diretório ganha uma mestra nova | sugestão | **feito**: `symlink_metadata`; teste que falhou antes |
| 6: `--serve-relay` decifra a autoridade sem precisar | sugestão | **pendência** (P31), igual no modo mestra |
| 7: ADR 0015 citado e inexistente | sugestão | este arquivo |
| Corrida no primeiro uso (dois `device.key`) | observação | **pendência** (P31): `save_sealed_new` |
