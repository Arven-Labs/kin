# ADR 0007 — `untrust` de contato e a lista negra persistente (P24)

- **Status**: aceito (Sessão 19, 2026-11-10). Implementado no PR #69.
- **Relacionados**: ADR 0006 §5 (consentimento: só se processa a lista de, e só se anuncia a, identidades aceitas e
  irmãos; a confiança é compartilhada pela família); ADR 0004 (chave mestra e certificados de device); P24 em `PENDING.md`;
  D7 em `ARCHITECTURE.md` (provedores de identidade).

## Contexto

Até aqui só se confiava (`Chat::trust`, `Chat::accept`), nunca se desconfiava. Confiar não é só local: em
`on_device_list`, um device de contato repassado por um **irmão** faz este device passar a confiar na identidade dele
(`self.trust(owner)`), porque a família compartilha a confiança. Então um `untrust` que só apagasse a confiança local seria
desfeito no próximo anúncio do irmão.

Havia também o pedido de "revogação de device". O `/pair` (ADR 0006 §7) **copia a chave mestra** para o device novo; um
device que eu quisesse revogar continuaria com a mestra e poderia assinar certificados novos. Revogar o *certificado* de um
device meu não revoga nada de verdade sem rotacionar a mestra.

## Decisão

1. **`Chat::untrust(&IdentityId)`** para identidades de **contato**: grava a identidade numa tabela `kin_distrusted` e a tira de
   `kin_trusted`; esquece os devices dela em `contact_devices` e `expected`. A minha própria identidade é recusada
   (`Error::OwnIdentity`): os meus devices são família.
2. **Lista negra persistente** (`kin_distrusted`, schema **v5**, aditiva: `CREATE TABLE IF NOT EXISTS`, mesmo padrão das v3 e
   v4). O caminho do irmão em `on_device_list` **pula** o item de identidade desconfiada (não guarda, não disca, não confia).
   `Chat::trust` e `Chat::accept` (atos explícitos) tiram a identidade da lista. Ao abrir o banco, a desconfiança vence a
   confiança (um `untrust` interrompido entre os dois passos não "ressuscita" a confiança).
3. **O `untrust` não bloqueia mensagens.** A conversa MLS existente continua e as mensagens chegam; ele só para de processar
   a lista de devices da identidade e de anunciar-lhe o meu convite. É o contrário simétrico de `trust`, não um *block*.
4. **Escolha por device, não pela família.** Cada device decide; um irmão que ainda confia segue anunciando, e este device o
   ignora. Não há payload novo entre irmãos.
5. **CLI**: `/untrust [prefixo]`, com a mesma resolução do `/trust` (função pura `resolve_identity`, só sobre identidades
   vistas em `ConversationReady` nesta execução).
6. **Revogação de device próprio: fora**, vai para D7. Exige rotação da mestra ou certificados delegados (o provedor TruthID),
   o que muda o formato do certificado e o pareamento: área de cripto (§4), ADR e auditoria próprios.

## Alternativas descartadas

- **Lista negra só em memória.** Mais simples e sem migração, mas some ao reiniciar e o irmão faz a identidade voltar.
- **Propagar o `untrust` à família** (novo `Payload` entre irmãos). Muda o formato de fio e abre conflito entre devices
  (um confia, outro desconfia, qual vence?); fase maior.
- **Revogar o certificado de um device próprio com uma lista assinada.** Inútil enquanto o device revogado tiver a mestra.
- **Apagar só `kin_trusted`** (sem lista negra). O irmão restaura a confiança.

## Consequências

- **Sem downgrade de build**: `Outbox::migrate` recusa `user_version` maior que o conhecido; um build v4 não abre um banco v5.
- A confiança continua sendo **por device** do ponto de vista de quem desconfia: para a família toda desconfiar é preciso
  rodar `/untrust` em cada device.
- O que a identidade já sabe de mim (endereços anunciados antes) **continua sabido**: o `untrust` não é retroativo.
- `/untrust` só alcança identidades vistas na execução atual; desconfiar de alguém que só está no banco (de uma sessão
  anterior) precisa que ele apareça de novo em `ConversationReady`. Evolução: listar as confiadas.
- Habilita: um futuro "bloquear" (descartar mensagens) e a revogação de device, que reaproveitarão a lista negra.
