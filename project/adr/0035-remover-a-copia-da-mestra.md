# ADR 0035 — Remover a cópia da mestra (`kin://pair/`) e orientar a migração (P30 fatia 6)

- **Status**: **aceito e implementado** (Sessão 28, 2026-11-17). Área de segurança (identidade, AGENTS.md §4): auditado pelo `security-auditor`
  (0 críticos, 4 importantes, 4 sugestões; tabela no fim).
- **Relacionados**: ADR 0011 (decisões 3 e 4, fatia 6), 0006 (o pareamento por cópia da mestra, agora removido), 0034, P24, P30.

## Contexto

O P30 existe porque o `/pair` antigo copia a **chave mestra** para cada device: um device roubado é a identidade roubada e revogar um certificado não revoga nada. As fatias 1 a 5
entregaram o caminho novo (mestra fria, primário com autoridade, `kin pair request/answer/accept`, `/devices` e `/revoke`). O ADR 0011 deixou por último, de propósito, remover o caminho
velho, porque a remoção só faz sentido depois de haver o novo.

## Decisão (dono, 2026-11-17)

1. **Sai a cópia da mestra**: `StandaloneIdentity::to_pair_link`/`from_pair_link`, `Error::InvalidPairLink`, a dependência `base64` do `kin-identity`, o `--pair`, o `/pair` (e `/pair confirmo`),
   o parâmetro `pair_text` de `keys::load` e os testes que os cobriam.
2. **O modo `Master` fica**, só para 1 device (mestra no próprio device, certificado v3). Quem copiar o `identity.key` à mão continua conseguindo (o lab `multi-device` faz isso), mas o
   aviso diz que isso expõe a mestra do mesmo jeito.
3. **Migração = aviso + roteiro, sem comando novo** (`kin migrate` descartado). `keys::notice_for` devolve o aviso só no modo `Master`; ele sai na abertura (stderr). O roteiro reusa
   `/accept`, `/trust` e `/untrust`: identidade **nova** (outro ID), pareamento pelo `kin pair`, reconfirmação fora de banda com cada contato, `/untrust` da antiga e apagar o `identity.key` e seus backups.
4. **`/pair` responde conforme o modo** (`pair_reply`): Master (identidade nova e `kin cold init`), primário (`kin pair answer`), pareado (quem adiciona é o primário). Mandar o primário
   rodar `kin cold init` criaria uma segunda identidade (auditoria I1).
5. **A trava de envio fica** (`is_pair_link`: a linha com `kin://pair/` não vai ao contato), porque links antigos ainda podem estar no scrollback ou num arquivo.

## Alternativas descartadas

- **Remover o modo `Master`**: fatia bem maior (`keys.rs`, `ensure_current_issuer`, muitos testes, lab) e obriga quem usa 1 device a ter mestra fria.
- **`kin migrate`**: código novo em área de identidade; a parte difícil (o contato reconfirmar) continua manual.
- **Manter o `/pair` com um aviso mais forte**: um link `kin://pair/` ainda emitido é a mestra em texto.
- **Protocolo de sucessão K1→K2**: já descartado no ADR 0011 (o ladrão com K1 assina uma sucessão rival).

## O que isto NÃO resolve (dito com todas as letras)

- **"Prazo de depreciação de K1" automático não existe**: quem avisaria os contatos de que a K1 morreu é a própria K1, que o ladrão também tem. Vira roteiro manual: cada contato faz `/untrust`.
  **Quem não reconfirmar continua confiando na chave antiga.**
- Quem **já copiou** a mestra continua com a identidade queimada até migrar; o aviso informa, não conserta.
- No modo `Master`, copiar `identity.key` à mão dá um segundo device, e esse device tem a mestra: sem revogação. Só mudou que o **comando** não existe mais.
- A trava `is_pair_link` é **rede de proteção**: não pega o corpo base64 colado sem o prefixo nem a chave colada em pedaços.
- O fio da trava e do aviso dentro do laço da CLI (`main.rs`) **não tem teste** (o laço não é exercitável sem o `Chat`); só as funções puras (`is_pair_link`, `notice_for`, `pair_reply`) têm.
- Consumidores de `kin-identity` fora do repo: nenhum achado nos projetos do workspace; repos que não estão aqui não foram conferidos.

## Consequências

- Quebra de API pública de `kin-identity` (as duas funções e o erro) e da CLI (`--pair`, `/pair`). Devices já pareados pelo caminho novo continuam funcionando.
- `kin-identity` perde a dependência `base64` (`Cargo.toml` e `Cargo.lock`, editados à mão; `cargo metadata --locked` coerente segundo a auditoria).
- `read_pair_link` (leitura de link de arquivo ou stdin) continua em `main.rs` porque o `kin pair request/answer` a usa; o nome ficou meio impreciso e não foi renomeado.

## Auditoria (`security-auditor`, somente leitura)

| # | Achado | Gravidade | Destino |
|---|---|---|---|
| I1 | `PAIR_REMOVED` valia para todos os modos e mandava o primário rodar `kin cold init` | importante | corrigido (`pair_reply` por modo; teste novo) |
| I2 | O aviso do Master prometia mais do que entregava (copiar `identity.key` à mão, ID novo, destruir o antigo, argumentos do `cold init`) | importante | corrigido (texto e testes) |
| I3 | O fio da trava e do aviso no laço da CLI não tem teste | importante | **não feito**: documentado acima (exigiria extrair o laço) |
| I4 | A trava se burla pelo corpo sem prefixo e pelo envio em partes | importante (baixo) | **não feito**: documentado como rede de proteção |
| S1 | ADR 0035 inexistente | sugestão | este arquivo |
| S2 | Comentários velhos no lab (`--pair`) | sugestão | corrigido |
| S3 | Asserção duplicada em `keys.rs` | sugestão | corrigido |
| S4 | `/pair x` (com argumento) vai como texto | sugestão | não feito (ruído, sem segredo) |
