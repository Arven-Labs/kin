# O que é o Kin

Uma **camada de comunicação descentralizada** em que os próprios dispositivos participantes formam a
infraestrutura. O mensageiro é a **primeira aplicação** construída sobre ela; arquivos, chamadas e
descoberta de serviços do ecossistema vêm depois, sobre a mesma rede.

Para o usuário, é um app de chat comum ("Fabio 🟢 online — Oi!"). DHT, relays e roteamento ficam
invisíveis.

**Propriedade central — *zero-infrastructure bootstrap***: a rede não precisa existir antes dos
usuários. Ela nasce quando eles começam a participar, melhora com o crescimento, mas **funciona com 2**.

Stack planejada (ainda não decidida — ver D3 em `ARCHITECTURE.md`):
- **Núcleo de rede**: Rust (`rust-libp2p`; alternativa a avaliar: `iroh`)
- **Desktop**: Tauri (mesmo padrão do TruthID/Warden/Anchor)
- **Mobile**: Flutter via FFI/`flutter_rust_bridge` (mesmo padrão do TruthID)
- **E2EE**: MLS (RFC 9420, ex. `openmls`) vs Double Ratchet — ver D2
- **Chamadas**: WebRTC + STUN/TURN (Fase 6)
- **Identidade**: provedores (D7) — standalone (keypair Ed25519, padrão no MVP) + TruthID opcional via prova de vínculo
- **Mailbox offline**: em aberto — hipótese "outbox local + relays Nostr como fallback" (D1)

**Ecossistema**: TruthID (identidade), Warden, Anchor, Lume. O Kin é um projeto **separado** do
TruthID e o consome — não é um módulo de comunicação dentro do TruthID (ideia já descartada antes).

---

# Status Geral

```
Fase 1 — Identidade local + conexão direta + chat 1:1 E2EE    [x] Feita (falta teste em redes reais)
Fase 2 — Hole punching + relay + convite por QR/link          [x] Feita no laboratório de NAT (falta teste em redes reais)
Fase 3 — Store-and-forward (mailbox) + multi-device           [~] Em andamento (3.1 a 3.4 feitas; 3.5 multi-device: modelo, fan-out, espelho e limite de 2 membros na main; descoberta (#31) mesclada; confiança persistente e `/trust` (#32) mesclados; pareamento por link (`/pair`, `--pair`) no PR da Sessão 16; roteiro manual com 2 processos feito na Sessão 16; falta o de 3 devices e relay real)
Fase 4 — Overlay: gossip + DHT + app-relay com limites        [ ] Pendente
Fase 5 — Grupos/canais, arquivos, presença, notificações      [ ] Pendente
Fase 6 — Chamadas WebRTC + TURN opt-in; relay empacotado      [ ] Pendente
Fase 7 — (opcional) Roteamento multi-hop / privado            [ ] Pendente
Fase 8 — (opcional) Blockchain: registro, proof-of-relay      [ ] Pendente
```

Próximo passo: rodar `REPEAT=5 lab/run.sh history-sync` (P10a, fatia 3: `/history` e o cenário estão no PR #102; as fatias 1 e 2, guardar e sincronizar entre irmãos, nos PRs #84 e #92); irmãos em NATs diferentes e relay real; depois o resto do P24 (o `lab/`, 1:1 e multi-device na mesma LAN, passou 5/5 por cenário na Sessão 20). Antes de
distribuir: teste manual em redes reais, QUIC no laboratório (P17), P13/P16/P18. Detalhes em `PHASE.md`,
`PENDING.md` e `SESSIONS.md`.

## O que existe hoje (Sessão 3)

O trabalho está em **bibliotecas Rust** (o "motor"): `identity`, `transport`, `crypto`, `chat` em
`crates/`. **Ainda não há app.** O crate `cli` é só uma **ferramenta de desenvolvimento**: um programa de
terminal que liga o motor para ver tudo funcionando (conversar, `/invite`, relay) e que serve de base do
laboratório de NAT (`lab/`). O app de verdade vem na Fase 5: desktop em Tauri e mobile em Flutter (FFI),
ambos como "janelas" sobre o mesmo motor Rust.
