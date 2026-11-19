# Laboratório de NAT

Prova a escada de fallback do Kin (direto → relay → hole punching) com NAT de verdade, na sua máquina:

```text
a (10.1.0.10) -- lana --[routera NAT]-- pub (203.0.113.0/24) --[routerb NAT]-- lanb -- b (10.2.0.10)
                                           |
                                         relay (203.0.113.10)
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
| `cone-quic` | o mesmo do `cone`, com o relay só em **QUIC/UDP** (`/udp/4001/quic-v1`): os peers o discam por QUIC e o hole punching roda sobre UDP (P17 a). A CLI não imprime o transporte da conexão direta; a prova é indireta (o relay não escuta TCP e o endereço de circuito do `a` contém `quic-v1`) |
| `symmetric-quic` | o mesmo do `symmetric` sobre QUIC: sem conexão direta, a conversa segue pelo relay |
| `lossy` | enlace ruim: `router.sh` aplica `tc netem` (`NETEM="delay 80ms 20ms loss 3%"`) na saída de cada roteador; exige que a conversa feche e as mensagens cheguem nos dois sentidos. O furo direto só é **observado** (`INFO` se não ocorrer). Precisa do `sch_netem` no kernel do host; sem ele o roteador não sobe e o cenário falha |
| `relay-down` | depois do furo, o relay cai e a conexão direta segue viva |
| `invite` | `a` gera o link (`/invite`), `b` o aceita (`--accept`) atrás de NAT: link adulterado é recusado, conversa pronta, hole punching |
| `invite-single-use` | **convite de uso único** (P18, ADR 0039): `a` sobe com o portão fechado (o padrão) e gera o link; `/invites` lista o convite vivo; `b` aceita e a conversa fica pronta; `/invites` fica vazio (o token foi gasto); `a2` (identidade nova, mesma LAN) usa **o mesmo link** e alcança o `a`, mas **não** abre conversa; o `a` termina com uma conversa só |
| `invite-revoked` | **revogação**: `a` (portão fechado, o padrão) gera o link, `/revoke-invite ID` o cancela e `/invites` fica vazio; `b` aceita o link revogado e alcança o `a`, mas **não** abre conversa; o `a` fica sem nenhuma conversa. Quem prova que o caminho feliz funciona com o portão ligado é o `invite-single-use` |
| `relay-failover` | dois relays; o reservado cai, `a` passa para o outro e um convite novo funciona por ele |
| `multi-device` | `a` e `a2` (mesma identidade, mesma LAN) e `b` como contato atrás de outro NAT: `a2` **não** acha `b` antes do `/trust` (controle negativo) e acha sozinho depois, furo direto `a2` ↔ `b`, espelho nos dois sentidos com o mesmo `MessageId`, e `b` falando com a identidade chega nos dois |
| `multi-device-plain` | o mesmo cenário com `--no-passphrase` (chaves em claro), para cobrir o caminho sem senha |
| `history-sync` | `a` e `b` trocam 3 mensagens (uma resposta com `/r`) e **só depois** entra o `a2` (irmão, banco vazio): recebe `3 novas, completo`; depois do `/trust` de `b`, o `/history` do `a2` mostra as 3 linhas com os mesmos `MessageId` e direções, e uma mensagem nova de `b` chega 1 vez em cada irmão sem duplicar no histórico. **Não cobre** reconectar o `a2` sem reimportar (reiniciar esbarra no P16) |
| `paired-device` | **sem copiar a mestra** (P30 2d-iii, ADR 0020 e 0021): `a` é o **primário** (só `authority.key` + `authority.cert`) e `a2` um device **pareado** (`primary.id`, `device.key`, `mls.sqlite`). O pareamento roda no **host** com o binário real (`kin cold init`, `kin pair request/answer/accept`, com o código de comparação lido da saída e digitado por pipe; um código errado não emite certificado; um diretório com pedido pendente não abre o chat nem ganha `identity.key`) e `compose cp` leva os arquivos. Depois vale a conferência do `multi-device`: `a2` não acha o contato `c` antes do `/trust`, acha sozinho depois, furo direto, espelho com o mesmo `MessageId` e fan-out |
| `revoke-device` | **revogação de irmãos pareados** (P30 fatia 7, ADR 0011/0034): `a` primário, `a2` e `a3` pareados no host, `c` contato atrás de outro NAT. Com `c` online, `/revoke` sem `confirmo` só pergunta; com `confirmo`, `c` imprime a revogação, mensagem do `a2` não chega, `c → X` chega só em `a`, `/devices` mostra `revogado`. Com `c` parado, o `a3` é revogado e `c`, ao voltar (com `--dial` do primário), aprende a revogação. **Fase 4 (observa, não reprova)**: restaurar o `mls.sqlite` do primário anterior à revogação devolve o `a3` a `ativo` (S4 do ADR 0023); repetir o `/revoke` o revoga de novo. **Não coberto**: "contato só conectado ao device revogado" (nenhum nó religa sozinho, P19/P21, e a CLI não tem comando de discar); rollback de declaração (só nos testes de unidade) |
| `reconnect` | **rediscagem automática** (P32): o `a` cai e volta com a mesma identidade; o `b`, que tinha uma mensagem na fila, redisca o `a` sozinho pelo endereço de circuito do relay que o `a` anunciou no `identify`, sem nenhum comando depois da volta |
| `reconnect-restart` | **rediscagem depois de reiniciar** (P32 a, schema v8): com o `a` fora e uma mensagem na fila, o `b` reinicia no mesmo `/data` **sem** `--dial` (os argumentos novos entram por `/tmp/kin-args`, lido pelo `node.sh`). Só os endereços gravados no disco (`kin_peer_addrs`) e a rediscagem agendada na abertura podem trazer o `a` de volta quando ele reaparece |

No `paired-device`, o `a2` usa `--data-dir /root` (não `/tmp`): o `compose cp` deixa o `mls.sqlite` com o uid do host e o SQLite o abre
com `O_CREAT`; em diretório com sticky bit (`/tmp`) um kernel com `fs.protected_regular` ligado nega isso até ao root ("Accessing
storage failed"). Cenários que só leem o arquivo copiado (como o `identity.key` do `multi-device`) não sofrem.

`REPEAT=5 lab/run.sh` roda cada cenário 5 vezes e imprime `x/5` por cenário (o hole punching varia de rodada para rodada).

No `multi-device`, o `a2` recebe o `identity.key` de `a` por `compose cp`: é o modo `Master` (v3), em que a mestra mora nos dois
devices. O `/pair` e o `--pair` antigos foram removidos (ADR 0035); o caminho com revogação é o cenário `paired-device`.

**Chaves cifradas (ADR 0012):** por padrão os nós do lab guardam `identity.key` e `device.key` cifrados. O stdin é uma fifo
(sem terminal), então a senha vem de `lab/passphrase` (senha só do laboratório, montada em `/lab/passphrase`); o `node.sh` a
copia para um arquivo 0600 e exporta `KIN_PASSPHRASE_FILE`, porque a CLI recusa arquivo de senha legível por outros e o git não
guarda o modo. Como os dois devices da mesma identidade usam a mesma senha, o `identity.key` copiado por `compose cp` abre no `a2`.
O `multi-device` confere que o arquivo de `a` começa por `KINK`; o `multi-device-plain` confere que não.

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
R="--nostr-relay wss://relay.primal.net --nostr-relay wss://offchain.pub"   # é o padrão; --nostr-relay ou relays.txt trocam
kin --data-dir /tmp/a --listen /ip4/127.0.0.1/tcp/4102 --no-mdns $R           # A: anote o peer id
kin --data-dir /tmp/b --no-mdns --dial /ip4/127.0.0.1/tcp/4102/p2p/<peer-id-de-A> $R   # B
```

1. Espere `conversa cifrada pronta` nos dois (o recebimento pela caixa postal exige essa conversa prévia).
2. Encerre A (`/quit`). Em B, digite `teste-1`: esperado `(na fila…)` e `(depositada na caixa postal: …)`.
3. Encerre B. Suba A de novo só com `$R` (sem B no ar): esperado `peer: teste-1`.

A CLI imprime uma linha por relay: `(relay wss://…: aceitou)` ou `(relay wss://…: recusou: motivo)`. Se o
passo 2 não imprimir `depositada`, todos os relays recusaram (`depósito recusado: motivo`).
