# ADR 0020 — CLI de pareamento `kin pair request | answer | accept` (P30 fatia 2d-ii-a2)

- **Status**: aceito e implementado no `cli` (Sessão 25, 2026-11-14, PR #194). Área de identidade e chaves (AGENTS.md §4): auditado
  pelo `security-auditor` (sem críticos, 2 importantes, ambos tratados; tabela no fim) e com 3 mutações pegas. **Nenhum formato de fio
  novo**: os links são prefixo + base64url dos bytes que já existiam (`KPR` e `DeviceCertificate`). O `kin` normal ainda não abre o
  diretório pareado (2d-ii-b).
- **Relacionados**: ADR 0011 (opção B/D), 0016 (pedido e SAS), 0017 (`PendingDevice`), 0018 (`Chat::open_paired`), 0019 (prazo), P30, P31.

## Contexto

As fatias 2c e 2d-i/2d-ii-a1 deram às bibliotecas o pareamento sem copiar a mestra, mas não havia como usá-lo. Esta fatia entrega só os
três comandos **offline** (sem rede e sem `Chat`, como o `kin cold`); o transporte entre as máquinas é manual (texto).

## Decisão

1. **Fluxo**: o primário mostra a sua identidade (base58) ao abrir; ela vai ao device novo **antes** do pedido.
   `kin pair request --data-dir D --identity BASE58` grava `D/primary.id`, gera `device.key` e o `mls.sqlite` pendente e imprime o
   link `kin://pair-request/...`, a identidade gravada e o SAS. `kin pair answer --data-dir P --request ARQUIVO` (no primário) lê o
   pedido, **pede o SAS no terminal** e só então imprime `kin://pair-cert/...`. `kin pair accept --data-dir D --certificate ARQUIVO|-`
   confere e instala.
2. **`primary.id` fora do `mls.sqlite`** (ADR 0017), gravado **antes** do pedido, **atomicamente** (temporário 0600 + `hard_link`, que
   nunca substitui: auditoria I1) e lido só como arquivo regular de até 256 bytes (S1). O `accept` usa **essa** identidade e nunca a
   do certificado (ADR 0016, item 4); devolve-a para ser impressa.
3. **Trocar de primário num diretório é erro** ("use um diretório novo"); diretório com `authority.key`, `authority.cert` ou
   `identity.key` (primário ou mestra) nunca vira device, e nada é criado nele.
4. **Links**: só no crate `cli` (decisão do dono; mover para uma biblioteca quando o app Tauri/Flutter precisar). Entrada por arquivo
   ou `-` (nunca por argv), teto de 4 KiB na leitura e 8192 caracteres depois do prefixo. O `answer` **não aceita `-`** porque o
   stdin é do prompt do SAS (desvio do plano, que o permitia).
5. **SAS digitado** (decisão do dono), comparado depois de `trim()`: sem hífens ou com dígito a mais não vale. Leitura com teto de
   128 bytes (S2).
6. **`IdentityId::from_base58`** no `identity` (decisão do dono, desvio do plano "sem mexer no identity"): só aceita o Peer ID de uma
   chave Ed25519 em multihash identidade, com ida e volta idêntica; erro único `InvalidIdentityId`, sem detalhe.
7. Senhas como o resto da CLI: `--passphrase-file`, senão `KIN_PASSPHRASE_FILE`, senão terminal. `base64` do workspace entrou no
   `kin-cli` (aprovado; já estava na árvore).

## Alternativas descartadas

- **Links em `identity`/`chat` (`to_link`)**: reutilizável, mas toca crates auditados e amplia o PR.
- **SAS por flag `--sas`**: convida a colar o código sem comparar, que é o que o SAS existe para evitar.
- **Gravar `primary.id` com `create_new` + `write_all`**: uma queda deixa o arquivo vazio e o diretório sem saída (I1).
- **Aceitar o SAS sem hífens**: não pedido; fica como decisão de produto se o uso mostrar fricção.
- **Apagar o arquivo do certificado depois de instalar**: não é segredo; apagar arquivo do usuário seria surpresa.

## Consequências

- Base58 de Peer ID **não tem checksum**: um erro de digitação em `--identity` vira outra identidade válida em boa parte dos casos. O
  SAS falha fechado no primário verdadeiro, mas o diretório fica preso àquela identidade; por isso o `request` imprime a identidade
  gravada para conferir e a mensagem de `primary.id` vazio ou corrompido manda apagar o arquivo e conferir a impressa.
- O `request` repetido devolve o mesmo pedido até 15 min (ADR 0019).
- O `kin` normal ainda não reconhece o diretório pareado (2d-ii-b); até lá o diretório só serve para os três comandos.

## Limites conhecidos e o que não foi validado

- **Uso real em 2 máquinas e o lab** (2d-iii); o `run` (prompt do SAS, leitura de senha) não tem teste, só as funções que ele chama.
- **`KIN_PASSPHRASE_FILE` é fallback único** para papéis diferentes: a senha do arquivo da autoridade pode selar o `device.key` novo
  sem aviso (S3).
- `accept` abre o `mls.sqlite` com `PendingDevice::open`, que cria o arquivo e um signer novo se faltar (inofensivo, sem pedido).
- Sobra de queda com o mesmo PID: o `create_new` do temporário falha com erro claro (não é apagada às cegas).
- O caminho do pedido passa por `to_string_lossy` (S7); caminho não UTF-8 viraria outro arquivo.
- `IdentityId::from_base58`: a ramificação "não Ed25519" e a checagem de tamanho são defensivas e sem teste que as isole (sem chave
  secp256k1 construível aqui); sem proptest. O `/dev/zero` em `primary.id` não tem prova de vermelho (antes da correção o teste não
  terminaria).
- Modo `--no-passphrase` não existe nestes comandos (senha sempre); um primário com `authority.key` em claro não abre.
- Sem revisão externa do formato `KPR` (ADR 0016).

## Parecer do `security-auditor` e como foi tratado

Sem bypass do SAS, sem uso da identidade do certificado, sem panic ou alocação grande alcançável por link ou base58.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: `primary.id` gravado antes do resto, vazio após queda, diretório sem saída | importante | **feito**: gravação atômica, mensagem acionável, identidade impressa; 5 testes falharam antes (run 43637478087) |
| I2: lacuna de teste em `from_base58` | importante | **parcial**: prefixos e multihash com lixo testados (caracterização, já passavam); proptest e vetores não canônicos ficam no P31 |
| S1: `primary.id` lido sem limite, segue link | sugestão | **feito**: arquivo regular, 256 bytes; teste sem vermelho |
| S2: SAS lido sem teto | sugestão | **feito**: 128 bytes |
| S3: `KIN_PASSPHRASE_FILE` único para papéis diferentes | sugestão | **documentado** (P31) |
| S4: identidade não mostrada ao device novo | sugestão | **feito**: `request` e `accept` imprimem |
| S5: ADR 0020 inexistente | sugestão | **feito**: este arquivo |
| S6: link com quebra de linha interna | sugestão | **feito**: a mensagem diz "numa linha só" |
| S7: `to_string_lossy` no caminho do pedido | sugestão | **pendência** (P31) |
| S8: lacunas de teste (`run`, symlink pendurado de `authority.key`, estado após senha errada) | sugestão | **pendência** (P31) |
