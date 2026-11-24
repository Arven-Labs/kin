# ADR 0046: troca de senha das chaves (`kin passwd`)

Status: aceito (P13, Sessão 45).

## Contexto
A senha só podia ser definida (criação ou `--protect`). Sem troca, quem expôs a senha ou quer outra teria de recriar a identidade.

## Decisão
`kin passwd [--data-dir D] [--passphrase-file ARQ] [--new-passphrase-file ARQ]` re-sela `identity.key` ou `authority.key`, `device.key` e `db.key` com a senha nova. Não toca nos bancos (o SQLCipher usa o `db.key` aleatório; só o envelope muda) nem na mestra fria (fora do `--data-dir`, senha própria). `KIN_PASSPHRASE_FILE` é ignorada: com duas senhas ficaria ambíguo.

- **Retomável, não atômico entre arquivos**: cada arquivo é trocado por temp + `fsync` + `rename`, reabrindo antes de substituir. `rekey_open` tenta a senha **nova** primeiro, então repetir o comando pula o que já foi trocado. Antes de gravar qualquer arquivo, `change_passphrase` confere todos (nem velha nem nova abre = aborta sem gravar).
- Recusas: senha nova vazia ou igual, arquivo em claro (isso é o `--protect`), **link simbólico** (o `rename` trocaria o link e deixaria o alvo na senha velha), banco cifrado sem `db.key` (falso sucesso).
- É **re-envelopamento, não rotação**: as chaves não mudam. Quem tem cópia antiga e a senha antiga abre a mesma identidade; a saída disso é identidade nova.

## Alternativas descartadas
- Diário/staging com `rename` em lote: mais código, e o lote também não é atômico.
- Rollback para a senha velha em caso de falha: precisaria guardar a velha em disco; a repetição com a mesma senha nova basta.
- Remover a senha (voltar para claro): enfraquece sem ganho; fora de escopo.

## Consequências / limites aceitos
- **Sem trava de diretório**: dois `kin passwd` ao mesmo tempo, ou um `kin` criando `device.key`/`db.key` durante a troca, podem deixar senhas mistas (recuperável com `old`/`new` certos). O texto manda fechar o kin. Gatilho para travar: o `kin` ganhar outro escritor das chaves.
- Blocos do arquivo antigo podem seguir no disco (mesmo limite do `protect_with`).
- Repetir com **outra** senha nova depois de uma queda falha sem alterar nada; o erro explica.
- Custo: até ~6 derivações Argon2id por arquivo (não medido em tempo real).
- Senha nova sem política mínima (coerente com o ADR 0012).
