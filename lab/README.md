# Laboratório de NAT

Prova a escada de fallback do Kin (direto → relay → hole punching) com NAT de verdade, na sua máquina:

```text
a (10.1.0.10) -- lana --[routera NAT]-- pub (172.30.0.0/24) --[routerb NAT]-- lanb -- b (10.2.0.10)
                                           |
                                         relay (172.30.0.10)
```

```bash
cargo build -p kin-cli          # o binário é montado do host (nada de compilar Rust no container)
lab/run.sh                      # todos os cenários
lab/run.sh cone                 # só um cenário
KEEP=1 lab/run.sh cone          # deixa os containers de pé para olhar os logs
```

| Cenário | Esperado |
|---|---|
| `cone` | conversa E2EE pronta pelo relay → DCUtR abre conexão **direta** → mensagem chega |
| `symmetric` | NAT com porta aleatória: o furo falha e a conversa **continua pelo relay** |
| `relay-down` | depois do furo, o relay cai e a conexão direta segue viva |
| `invite` | `a` gera o link (`/invite`), `b` o aceita (`--accept`) atrás de NAT: link adulterado é recusado, conversa pronta, hole punching |
| `relay-failover` | dois relays; o reservado cai, `a` passa para o outro e um convite novo funciona por ele |

Os roteadores descartam em silêncio pacote de fora sem mapeamento (como um roteador doméstico), o que
deixa o SYN reenviado passar quando o outro lado abre o mapeamento.

**Disco**: a imagem `kin-lab` tem ~133 MB (sobre `debian:stable-slim`). `run.sh` remove containers e redes
ao terminar; para apagar a imagem: `docker rmi kin-lab`.

Limitação: o relay do laboratório escuta só TCP, então o hole punching por QUIC/UDP não é exercitado (P17).

## Roteiro manual: caixa postal Nostr com relay real

Fora do `run.sh` (depende da internet e de relays de terceiros). **Publica eventos kind 1059 em relays
públicos, que não dá para apagar**: use só mensagens de teste e peça autorização antes. Sem Docker, em dois
terminais, com a CLI (`cargo build -p kin-cli`):

```bash
R="--nostr-relay wss://nos.lol --nostr-relay wss://relay.damus.io"
kin --data-dir /tmp/a --listen /ip4/127.0.0.1/tcp/4102 --no-mdns $R           # A: anote o peer id
kin --data-dir /tmp/b --no-mdns --dial /ip4/127.0.0.1/tcp/4102/p2p/<peer-id-de-A> $R   # B
```

1. Espere `conversa cifrada pronta` nos dois (o recebimento pela caixa postal exige essa conversa prévia).
2. Encerre A (`/quit`). Em B, digite `teste-1`: esperado `(na fila…)` e `(depositada na caixa postal: …)`.
3. Encerre B. Suba A de novo só com `$R` (sem B no ar): esperado `peer: teste-1`.

Se o passo 2 não imprimir `depositada`, todos os relays recusaram (`depósito recusado: motivo`). A CLI ainda
não mostra qual relay aceitou (P23(g)).
