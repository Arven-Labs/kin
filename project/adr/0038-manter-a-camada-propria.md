# ADR 0038 — Manter a camada própria (não virar cliente de SimpleX ou Matrix)

- **Status**: **aceito** (dono, 2026-11-18). Decisão de direção, sem código.
- **Relacionados**: `docs/pesquisa/reuso/kin.md` do workspace (2026-11-13, decisões 1 e 2), `STUDY.md` (SimpleX, Matrix P2P, Nostr), `ARCHITECTURE.md` D1 e D3, ADR 0003 (interface de mailbox).

## Contexto

O diferencial do kin é o *zero-infrastructure bootstrap*: funciona com 2 dispositivos, sem nó dedicado. As alternativas que o `STUDY.md` examinou (SimpleX, Matrix, Session, Nostr, Briar) têm algum nó sempre ligado, ou exigem que os dois lados estejam online. A pesquisa do workspace levantou a pergunta: o diferencial justifica uma camada própria, ou vale construir o kin como cliente de SimpleX ou Matrix? E o custo de NAT e de entrega assíncrona é aceitável?

## Decisão

1. **Manter a camada própria** (libp2p + MLS + mailbox Nostr, como está). O kin não vira cliente nem extensão de SimpleX ou Matrix.
2. **Porta aberta (opção C)**: se a entrega assíncrona doer, SimpleX (filas sem ID) ou outro backend pode entrar **depois** como mailbox opcional atrás da interface de mailbox (ADR 0003), como módulo. Nada é feito agora.
3. **Aceitar o custo de NAT e de entrega assíncrona**, com **teste em rede real (fora do Docker) antes de começar a Fase 4**.

## Alternativas descartadas

- **B. Cliente de SimpleX ou Matrix**: anula o diferencial (as duas redes dependem de servidor), é **difícil de reverter** (muda protocolo, identidade e formato de dados) e descarta a pilha construída. A AGPL-3.0 de um derivado do SimpleX estenderia o copyleft ao serviço em rede. O Matrix é federado e pesado; o experimento P2P dele (Pinecone) ficou como prova de conceito.
- **A puro, sem porta aberta**: não custa mais, mas fecha uma saída barata (o mailbox já é uma interface).

## Consequências

- Continuam a cargo do kin: NAT (hole punching, relay), entrega assíncrona e multi-device.
- A Fase 4 só começa depois do teste em rede real; o resultado entra no `SESSIONS.md`.
- **Verificado em 2026-11-18** (nada muda a decisão): SimpleX é AGPL-3.0 e está ativo; o `matrix-rust-sdk` é Apache-2.0, ativo, mas só cliente-servidor (não serve de base para o "funciona com 2"); Berty está ativo (v2.472.2), Apache-2.0 ou MIT, com BLE e mDNS na rede local; Briar é GPL-3.0 ou posterior no Android e AGPL no desktop (fontes secundárias), e tem um repo `briar-mailbox` (dispositivo sempre ligado para entrega assíncrona), o que reforça o P33.
- **Ainda não verificado**: o que a AGPL do SimpleX exigiria de um cliente que fala o protocolo SMP (pede parecer jurídico); a FAQ do Briar; se o Berty entrega a quem está offline fora da rede local; o desenho do `briar-mailbox`. Reabrir a decisão se um deles resolver o "funciona com 2".
