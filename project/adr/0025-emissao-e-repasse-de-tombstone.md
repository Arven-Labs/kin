# ADR 0025 — Emitir e repassar o tombstone (P30 fatia 3c-ii-a)

- **Status**: aceito e implementado no `chat` e no `crypto` (Sessão 26, 2026-11-15). Área de segurança (revogação de acesso e envio na
  rede, AGENTS.md §4): auditado pelo `security-auditor` (0 críticos, 3 importantes; tabela no fim) e com 9 mutações (9 pegas). Falta
  a **3c-ii-b** (reenviar ao conectar), que é **bloqueadora do P30**: sem ela a propagação tem uma tentativa só por salto.
- **Relacionados**: ADR 0011 ("repassável por qualquer device"), 0023 (store), 0024 (receber, admissão e cotas), P30, P31.

## Contexto

A 3c-i faz um `Chat` receber e gravar um tombstone. Falta ele **sair**: o dono emite para quem precisa saber e quem recebe um tombstone
novo o repassa, para o contato ou o outro device do dono que só fala com um terceiro também ficar sabendo. O dono cortou a 3c-ii em
**a (emitir e repassar)**, esta, e **b (reenviar ao conectar)**.

## Decisão

1. **`CryptoDevice::store_revocation` e `Provider::store_revocation` devolvem `Result<bool>`**: `true` se o tombstone era novo (a linha
   entrou), `false` se já estava gravado (`execute()? > 0` com `ON CONFLICT DO NOTHING`). É o que decide o repasse.
2. **`Chat::broadcast_revocation(&DeviceRevocation)`**: aceita um tombstone **já assinado** (a CLI da fatia 5 o monta; o `Chat` nunca
   tem a mestra), exige `is_trusted(identity)`, grava (confere a assinatura) e o envia a todos os destinos, **mesmo se já estava gravado**
   (reenvio explícito). Grava antes de enviar: assinatura inválida não envia nada.
3. **`on_revocation` repassa só o que o store disse ser novo**, a `revocation_targets(identity, Some(remetente))`. O repetido não repassa:
   é o que corta o eco e o loop (cada device grava uma vez, então a cascata termina).
4. **Destinos (decisão do dono, depois da auditoria I1)**: peers **conectados**, com conversa de um único peer, que **aceitam a identidade `I`
   do tombstone**, porque o receptor admite por `is_trusted(I)` e não pelo remetente:
   - **irmão** (identidade minha), sempre;
   - **device da própria `I`** (ele aceita a si mesmo);
   - **contato que aceitei, só se `I` é a minha**.
   Tombstone de **terceira** identidade não vai a outros contatos: revelaria em quem eu confio (identidade e cadeia da terceira) e gastaria a
   cota de quem o recusa com `UnauthorizedRevocation`. A primeira versão desta fatia escolhia "peers que **eu** aceito", e a auditoria mostrou
   que isso confunde as duas perguntas.
5. **Reembolso de cota com teto (decisão do dono, auditoria I2)**: erro de **armazenamento** (`Crypto(Storage)`) devolve a cota do peer e a
   global, mas só até **16 devoluções por janela de 60 s**, num contador separado (`revocation_refunds`). Sem o teto, com o disco falhando
   para sempre, o limite deixaria de existir justamente quando cada tentativa já pagou a verificação de assinatura. Assinatura inválida,
   recusa de admissão e `RateLimited` continuam gastando.
6. **`Chat::is_revoked`** público, só leitura do store (para a CLI e os testes). A consulta que derruba o par é a 3d.
7. **Sem ack nem retry**: `send_revocation` cifra e `transmit`, fora da outbox (como `announce_relays`); falha de cifra vira `Dropped`.

## Alternativas descartadas

- **Destinos = "peers que eu aceito"** (primeira versão): ver item 4.
- **Repassar também o já gravado**: reintroduz eco e loop.
- **Emitir só se novo**: um `broadcast` explícito deve poder reenviar.
- **Reembolso ilimitado** ou **sem reembolso**: ver item 5; o dono escolheu o teto.

## Consequências e requisitos para a 3c-ii-b e a 3d

- **Propagação de tentativa única (auditoria I3)**: gravar é a condição para repassar e o repetido nunca repassa. Se o `encrypt` falha, se o
  transporte perde o envio, se o processo cai entre gravar e enviar, ou se o peer conecta depois, aquele salto **não repete**. É um
  fail-open até a 3c-ii-b: **reenviar ao conectar** (até 8 mais recentes por conexão, decisão do dono, sem cursor), respeitando a regra de
  destino do item 4. Quem tem mais de 8 tombstones não recebe todos (limite registrado).
- **O teto global de 64 vale por tentativa recebida, inclusive duplicata** (auditoria S1): em malha de k devices aceitos cada nó recebe até
  k-1 cópias por tombstone; com k=5 o teto acaba em ~16 tombstones por minuto. Aceito; documentado.
- **Um estranho com conversa também gasta a cota global** (a conta vem antes da admissão), não só um peer `trusted`; vem da 3c-i.
- **`broadcast_revocation` aceita identidade de contato** (`is_trusted`), coerente com a recepção; a CLI da fatia 5 deveria restringir
  à própria identidade.
- **Conversa com vários membros** nunca recebe tombstone (`conversation_identity` devolve `None`): hoje tudo é 1:1; um grupo futuro ficaria
  sem revogação, em silêncio.
- **A 3d** consulta `is_revoked_or_unknown` depois do `credential::verify` (ADR 0023 I2).

## Limites conhecidos e o que não foi validado

- Lacunas de teste: a **chamada** do reembolso em `on_revocation` (só o predicado `refunds_quota` e o `refund_revocation_quota` têm teste;
  não dá para provocar erro de armazenamento no nível do `chat`); o caminho de erro do `encrypt` em `send_revocation`; `untrust` de um
  destino conectado entre dois repasses; o ciclo A→B→C→A em rede (só o teste unitário do repetido prova a terminação).
- `conversation.peers()` por conexão a cada tombstone novo: não confirmei se verifica assinatura de certificado (S3); limitado pela cota.
- Um `Dropped` por excedente numa `queue` sem teto (ADR 0024 S1).
- Sem revisão externa. O I1 e o I2 vêm de leitura estática; o I1 foi corrigido e testado, o I2 também.

## Parecer do `security-auditor` e como foi tratado

Sem críticos. Descartou como falso positivo: eco ao remetente, loop A→B→C→A (termina pelo `Ok(false)`), `changes()` de instrução anterior,
remetente provocando `Crypto(Storage)` por entrada, `broadcast` como oráculo remoto (é API local), corrida de estado.

| Achado | Gravidade | Tratamento |
|---|---|---|
| I1: destino pela pergunta errada ("eu aceito o peer"): vaza o grafo de confiança e gasta a cota de quem recusa | importante | **feito** (decisão do dono): só quem aceita a identidade do tombstone; 3 testes falharam antes (run 43714563176); mutação N1 |
| I2: reembolso de cota sem teto | importante | **feito** (decisão do dono): teto de 16 por janela; teste e mutação N2 |
| I3: propagação de tentativa única | importante | **registrado**: 3c-ii-b bloqueia o P30 |
| S1: O(k²) e cota global por duplicata | sugestão | **documentado** |
| S2: `send_revocation` engolia a ausência de conversa | sugestão | **feito**: `debug_assert!` (o ramo é inalcançável) |
| S3: custo de `peers()` por conexão | sugestão | **pendência** (não confirmado) |
| S4: conversa com vários membros | sugestão | **documentado** |
| S5: ADR 0025 inexistente | sugestão | **feito**: este arquivo |
| Gaps 1, 3, 8, 10, 11 de teste | sugestão | **feito em parte**: terceira identidade, destinos por identidade, A1→B→A2 em rede; a chamada do reembolso e o erro do `encrypt` ficam pendentes |
