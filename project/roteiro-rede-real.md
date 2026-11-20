# Roteiro: teste em rede real (P33, ADR 0038)

**Status: escrito em 2026-11-20 e NÃO executado.** Os comandos foram conferidos por leitura contra o cabeçalho de `crates/cli/src/main.rs`, `lab/README.md` e `lab/run.sh`; nenhum foi rodado fora do Docker. Antes de usar, releia o cabeçalho da CLI: as flags podem ter mudado. Quem executa é o dono, com rede própria (a execução está bloqueada até haver uma segunda rede; ver P33 no `PENDING.md`).

**Para que serve.** O ADR 0038 só libera a Fase 4 depois de validar fora do Docker. O lab já cobre NAT sintético (`iptables`), QUIC/UDP e enlace ruim (`netem`). Falta o que só a rede real prova: roteador doméstico (timeout de NAT, hairpin), CGNAT da operadora, a taxa real do DCUtR (~70%, só estimada), relay com IP público e 3 devices em redes distintas.

## Preparação

- **Binário.** O workflow `Lab` (tem `workflow_dispatch`) sobe o artefato `kin` (`target/debug/kin`, build **debug**, retenção de **1 dia**, glibc do Ubuntu 24.04): `gh run download <run-id> -n kin`. Rode na máquina de destino ou copie o arquivo (`chmod +x kin`). Build local neste workspace é barrado (ADR 0009); em outra máquina, compilar lá é decisão sua.
- **Estado limpo** por cenário: um `--data-dir` novo em cada máquina (`/tmp/kin-<cenário>-<a|b>`). Chaves cifradas por padrão (ADR 0012): para repetir sem digitar a senha, use o arquivo de senha como o `lab/passphrase`, ou `--no-passphrase` só em teste.
- **Portão de admissão fechado** (padrão): quem não é conhecido só abre conversa com o token de um convite seu. Entre os dois lados use o link do `/invite` (`--accept LINK`) ou `--dial` **com** `/p2p/<id>`. Não use `--open-admission` em rede real.
- **Anote por rodada**: data e hora, rede de cada ponta (casa, 4G, escritório), roteador e operadora, IPv4/IPv6, e se há CGNAT (IP WAN do roteador diferente do IP público visto de fora).
- **Como ler o resultado.** A CLI imprime `rota com <peer>: direta` ou `rota com <peer>: via relay` (é o que o lab procura). A rota para o **relay** também é "direta", então confira o Peer ID do par e não o do relay. A CLI não diz se a conexão direta é TCP ou QUIC.

## Cenário 1: dois peers em NATs distintos, relay público

Precisa de um relay com IP público (VPS). Sem ele, só o 1B vale.

```bash
# R (VPS, porta aberta no firewall): só relay, sem chat
kin --serve-relay --data-dir /tmp/kin-r --listen /ip4/0.0.0.0/tcp/4001 --external-addr /ip4/<IP-PÚBLICO>/tcp/4001
# anote o Peer ID impresso; o endereço do relay é /ip4/<IP-PÚBLICO>/tcp/4001/p2p/<peer-id-de-R>

# A (rede 1)
kin --data-dir /tmp/kin-a --no-mdns --relay <ENDEREÇO-DO-RELAY>
# digite /invite e passe o link a B por um canal à parte

# B (rede 2)
kin --data-dir /tmp/kin-b --no-mdns --relay <ENDEREÇO-DO-RELAY> --accept <LINK>
```

Esperado (como no `invite` do lab): `conversa cifrada pronta` nos dois e uma mensagem de cada lado chegando; depois, o DCUtR deve trocar o relay por `rota com <outro>: direta`.

- **Medir**: tempo até a conversa ficar pronta; se houve `direta` e em quantos segundos (o lab espera até 45 s); se não houve, o motivo provável (NAT simétrico/CGNAT). Repita **pelo menos 10 vezes** (reinicie os dois com o mesmo `--data-dir` ou use diretórios novos) para ter uma taxa, e anote o tipo de NAT de cada rodada. Esse é o número que o P17 (e) estima em ~70%.
- **1B (sem VPS)**: A e B na mesma LAN ou em redes que se alcancem, sem relay: `kin --data-dir /tmp/kin-a --listen /ip4/0.0.0.0/tcp/4002 --no-mdns` e, em B, `--dial /ip4/<IP-DE-A>/tcp/4002/p2p/<peer-id-de-A>`. Só prova a conexão direta, não o NAT.
- **Relay só em QUIC/UDP** (opcional, P17 a): suba R com `--listen /ip4/0.0.0.0/udp/4001/quic-v1` e `--external-addr` correspondente; repita.
- **Relay cai depois do furo** (`relay-down` do lab): com `rota ...: direta`, derrube o R e mande mensagens; a conversa deve seguir.

## Cenário 2: entrega assíncrona e reconexão

Com a conversa pronta (cenário 1):

1. Encerre A (`/quit`). Em B, digite `teste-1`: esperado `(na fila…)`; com a caixa postal, `(depositada na caixa postal: …)`.
2. Suba A de novo com o **mesmo** `--data-dir` e os mesmos argumentos: esperado `peer: teste-1`. Meça o tempo entre A voltar e a mensagem chegar, **sem** nenhum comando em B (é a rediscagem automática, P32).
3. Repita reiniciando **B** (com A fora) para exercitar a rediscagem pelos endereços gravados em disco (`reconnect-restart`).

A caixa postal Nostr **publica eventos kind 1059 em relays públicos que não dá para apagar**: use só mensagens de teste e siga o aviso do `lab/README.md` (seção "Roteiro manual: caixa postal Nostr"). Para testar sem publicar, suba com `--no-nostr`; nesse caso só a outbox local e a rediscagem entram.

## Cenário 3: 3 devices em redes distintas (multi-device)

Depende de o `paired-device`/`multi-device` já passar no lab; replique o passo a passo do `lab/run.sh` (funções dos cenários `paired-device` e `revoke-device`), com A (primário), A2 (device pareado) e C (contato) cada um numa rede. Pareamento: `kin cold init`, `kin pair request/answer/accept` (cabeçalho da CLI; o código de comparação é digitado por você). Esperado: o fan-out chega nos devices, `/trust` libera o anúncio de devices, e `/revoke PREFIXO confirmo` é aprendido por C. Anote o que muda em relação ao lab (por exemplo, A2 não achar C sem `--dial`, já que não há mDNS entre redes).

## Resultado (preencher na execução)

| Cenário | Data | Redes / NAT / operadora | Direta? (n de N) | Tempo até conectar | Observações |
|---|---|---|---|---|---|
| 1 (relay + DCUtR) | | | | | |
| 1B (direto, sem relay) | | | | | |
| 1 (QUIC/UDP) | | | | | |
| 1 (relay cai) | | | | | |
| 2 (fila e reconexão) | | | | | |
| 3 (3 devices) | | | | | |

## Critério de pronto do P33

- Cenário 1 rodado em **pelo menos duas combinações reais de rede**, com taxa de furo medida (N ≥ 10 por combinação) e a conversa **seguindo por relay** quando o furo falha.
- Cenário 2 rodado: fila + reconexão sem comando, nas duas direções.
- Resultado e os números reais em `SESSIONS.md`, e o P17 (e) atualizado com a taxa medida.
- Se algo reprovar, vira pendência nova (com o tipo de NAT) **antes** de a Fase 4 começar; se tudo passar, o ADR 0038 ganha a linha "validado em rede real em <data>".

## Fora do roteiro

Teste de carga, relay com muitos circuitos, segurança do relay (`--relay-max-bytes`, `--relay-max-circuits`) e a porta C (SimpleX como mailbox): não fazem parte do P33.
