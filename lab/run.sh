#!/usr/bin/env bash
# Laboratório de NAT (Fase 2): sobe relay + dois roteadores com NAT + dois peers e confere o que a
# escada de fallback faz. Uso: `cargo build -p kin-cli && lab/run.sh [cone|symmetric|cone-quic|symmetric-quic|lossy|relay-down|invite|invite-single-use|invite-revoked|invite-approved|invite-denied|relay-failover|multi-device|multi-device-plain|history-sync|paired-device|revoke-device|reconnect|reconnect-restart|stale-connection-rst|stale-connection-silent]...`
# (sem argumentos roda todos). Limpa containers e redes ao final.
set -uo pipefail
cd "$(dirname "$0")"

BIN=../target/debug/kin
[ -x "$BIN" ] || { echo "compile antes, no PC do dono: cargo build -p kin-cli (proibido na VM do agente, ADR 0017 do workspace)" >&2; exit 2; }

# A imagem é construída uma vez aqui (vários serviços construindo a mesma tag em paralelo brigam).
docker build -q -t kin-lab . >/dev/null || { echo "falha ao construir a imagem kin-lab" >&2; exit 2; }
compose() { docker compose "$@"; }
logs() { compose logs --no-log-prefix "$1" 2>&1; }
# Espera `padrão` aparecer no log do container, até `segundos`.
wait_log() {
  local c=$1 pat=$2 t=${3:-30}
  for _ in $(seq $((t * 2))); do
    logs "$c" | grep -qE "$pat" && return 0
    sleep 0.5
  done
  return 1
}
first() { logs "$1" | grep -oE "$2" | head -1; }
# Peer ID impresso na primeira linha do kin do container.
peer_id() { first "$1" 'peer id: [A-Za-z0-9]+' | awk '{print $3}'; }
# O container `$1` tem conexão DIRETA com o peer `$2`? (a rota para o relay também é "direta",
# por isso o padrão exige o Peer ID do outro lado.)
direct_with() { logs "$1" | grep -qE "rota com $2: direta"; }
wait_direct() { wait_log "$1" "rota com $2: direta" "${3:-45}"; }
# "Digita" uma linha no stdin do kin do container.
say() { compose exec -T "$1" sh -c "echo '$2' > /tmp/in"; }

# Transporte do relay, que os peers também usam para chegar a ele: TCP por padrão; os cenários `*-quic`
# trocam para QUIC/UDP (P17 a) com `use_quic` e voltam com `use_tcp`.
use_tcp() { RELAY_PROTO="tcp/4001"; export RELAY_LISTEN="/ip4/0.0.0.0/$RELAY_PROTO"; }
use_quic() { RELAY_PROTO="udp/4001/quic-v1"; export RELAY_LISTEN="/ip4/0.0.0.0/$RELAY_PROTO"; }
use_tcp

FAILED=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; FAILED=1; }
check() { # descrição, comando...
  local what=$1; shift
  if "$@"; then pass "$what"; else fail "$what"; return 1; fi
}

cleanup() { compose down -v --remove-orphans >/dev/null 2>&1; }
[ -n "${KEEP:-}" ] || trap cleanup EXIT   # KEEP=1 deixa os containers de pé para inspeção

# Sobe relay(s) e roteadores com o NAT pedido. Variáveis: RELAY_ADDR (e RELAY2_ADDR se pedido).
infra() { # nat [relay2]
  local nat=$1; shift
  cleanup
  export NAT_MODE=$nat A_ARGS="" B_ARGS=""
  compose up -d relay routera routerb "$@" >/dev/null || return 1
  wait_log relay "relay em /ip4/203.0.113.10/$RELAY_PROTO/p2p/" 30 || { echo "relay não subiu"; return 1; }
  RELAY_ADDR=$(first relay "/ip4/203.0.113.10/$RELAY_PROTO/p2p/[A-Za-z0-9]+")
  if [ "${1:-}" = relay2 ]; then
    wait_log relay2 "relay em /ip4/203.0.113.11/$RELAY_PROTO/p2p/" 30 || { echo "relay2 não subiu"; return 1; }
    RELAY2_ADDR=$(first relay2 "/ip4/203.0.113.11/$RELAY_PROTO/p2p/[A-Za-z0-9]+")
  fi
}

# Sobe a topologia com o NAT pedido e deixa A e B com a conversa pronta (pelo relay).
# Deixa em variáveis: RELAY_ADDR, A_CIRCUIT.
setup() {
  infra "$1" || return 1

  # Estes cenários testam NAT e transporte, não a admissão: o `b` disca o `a` sem link, então o `a` abre mão
  # do portão (P18 PR 3b). Os cenários de convite, multi-device, pareado e revogação o deixam fechado.
  # SETUP_KEY_ARGS (vazio por padrão = chaves e bancos cifrados com a senha do lab): o `protect-migration` passa
  # `--no-passphrase` para começar com tudo em claro. Não é o `KEY_ARGS` de outros cenários (o `revoke-device`
  # o deixa preenchido para os seguintes).
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --open-admission ${SETUP_KEY_ARGS:-}"
  compose up -d a >/dev/null || return 1
  wait_log a "reserva aceita no relay" 30 || { echo "a não conseguiu reserva no relay"; logs a; return 1; }
  A_CIRCUIT=$(first a "/ip4/203.0.113.10/$RELAY_PROTO/p2p/[A-Za-z0-9]+/p2p-circuit/p2p/[A-Za-z0-9]+")

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --dial $A_CIRCUIT ${SETUP_KEY_ARGS:-}"
  compose up -d b >/dev/null || return 1
  wait_log a "conversa cifrada pronta" 40 && wait_log b "conversa cifrada pronta" 40 \
    || { echo "a conversa não ficou pronta pelo relay"; logs a; logs b; return 1; }
  A_ID=$(peer_id a); B_ID=$(peer_id b)
}

delivered() { # remetente, destinatário, texto
  say "$1" "$3"
  wait_log "$2" "peer: $3" 20
}

scenario_cone() {
  echo "== NAT cone: o hole punching deve trocar o relay por conexão direta"
  setup cone || { fail "montar o laboratório"; return; }
  check "conversa E2EE pronta pelo relay" true
  check "mensagem chega pelo relay" delivered a b "oi pelo relay"
  if wait_direct a "$B_ID" 45 || wait_direct b "$A_ID" 5; then
    pass "hole punching: conexão direta entre a e b"
  else
    fail "hole punching: nenhuma conexão direta em 45s"; logs a | tail -25; logs b | tail -25
  fi
  check "mensagem chega depois do furo" delivered b a "oi direto"
}

scenario_symmetric() {
  echo "== NAT simétrico: o furo falha e a conversa continua pelo relay"
  setup symmetric || { fail "montar o laboratório"; return; }
  sleep 20
  if direct_with a "$B_ID" || direct_with b "$A_ID"; then
    fail "inesperado: conexão direta atrás de NAT simétrico"
  else
    pass "sem conexão direta (esperado)"
  fi
  check "mensagem chega pelo relay" delivered a b "oi simétrico"
  check "resposta chega pelo relay" delivered b a "volta simétrica"
}

# Mesmos cenários `cone` e `symmetric`, mas com o relay em QUIC/UDP: os peers o discam por QUIC, então
# os endereços observados (identify) e o hole punching (DCUtR) rodam sobre UDP (P17 a). O log da CLI
# não diz o transporte da conexão direta; a prova é indireta: o relay só escuta UDP, então o endereço
# de circuito que o `a` anuncia (`A_CIRCUIT`) tem de conter `quic-v1`.
quic_reserved() { case "$A_CIRCUIT" in *quic-v1*) true ;; *) false ;; esac; }

scenario_cone_quic() {
  use_quic
  echo "== NAT cone sobre QUIC/UDP"
  scenario_cone
  check "reserva no relay por QUIC" quic_reserved
  use_tcp
}

scenario_symmetric_quic() {
  use_quic
  echo "== NAT simétrico sobre QUIC/UDP"
  scenario_symmetric
  use_tcp
}

# Enlace ruim (atraso 80 ms ± 20 ms e 3% de perda em cada roteador): o NAT sintético é limpo demais, e
# o mundo real perde pacote. Exige que a conversa feche e as mensagens cheguem; o furo direto é só
# observado (com perda ele pode demorar ou não acontecer, e a conversa segue pelo relay).
netem_active() { compose exec -T routera tc qdisc show 2>&1 | grep -q netem && compose exec -T routerb tc qdisc show 2>&1 | grep -q netem; }

scenario_lossy() {
  echo "== Enlace ruim (80 ms ± 20 ms, 3% de perda): a conversa fecha e as mensagens chegam"
  export NETEM="delay 80ms 20ms loss 3%"
  setup cone || { fail "montar o laboratório"; unset NETEM; return; }
  check "atraso e perda ativos nos dois roteadores" netem_active
  check "mensagem chega pelo relay" delivered a b "oi com perda"
  if wait_direct a "$B_ID" 45 || wait_direct b "$A_ID" 5; then
    pass "hole punching mesmo com perda"
  else
    echo "  INFO  sem conexão direta em 45s (aceitável com perda; a conversa segue pelo relay)"
  fi
  check "mensagem chega no fim" delivered b a "volta com perda"
  unset NETEM
}

scenario_relay_down() {
  echo "== Relay cai depois do furo: a conexão direta segue viva"
  setup cone || { fail "montar o laboratório"; return; }
  if ! (wait_direct a "$B_ID" 45 || wait_direct b "$A_ID" 5); then
    fail "sem conexão direta para testar"; return
  fi
  compose stop relay >/dev/null
  sleep 2
  check "mensagem chega sem o relay" delivered a b "sem relay"
}

scenario_invite() {
  echo "== Convite por link atrás de NAT: identidade fixada, link adulterado recusado, hole punching"
  infra cone || { fail "montar o laboratório"; return; }
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR"
  compose up -d a >/dev/null
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; return; }
  say a "/invite"
  wait_log a "convite[^:]*: kin://invite/" 15 || { fail "a não gerou o convite"; logs a; return; }
  LINK=$(logs a | grep -oE 'kin://invite/[A-Za-z0-9_-]+' | tail -1)

  # Um caractere trocado no meio do certificado: a assinatura não confere.
  BAD=$(echo "$LINK" | sed 's/./X/40')
  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --accept $BAD"
  compose up -d b >/dev/null
  check "link adulterado é recusado" wait_log b "convite não aceito" 40 \
    || { echo "  link: $LINK"; echo "  ruim: $BAD"; logs b | tail -8; }
  compose rm -sf b >/dev/null 2>&1

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --accept $LINK"
  compose up -d b >/dev/null
  if wait_log a "conversa cifrada pronta" 45 && wait_log b "conversa cifrada pronta" 45; then
    pass "conversa pronta pelo link"
  else
    fail "conversa não ficou pronta pelo link"; logs a | tail -20; logs b | tail -20; return
  fi
  A_ID=$(peer_id a); B_ID=$(peer_id b)
  check "b mostra a identidade do convite" wait_log b "convite de [A-Za-z0-9]+" 5
  check "mensagem chega" delivered b a "oi pelo link"
  if wait_direct a "$B_ID" 45 || wait_direct b "$A_ID" 5; then
    pass "hole punching depois do convite"
  else
    fail "sem conexão direta depois do convite"
  fi
}

# `a` (com o portão fechado, que é o padrão) gera um convite e devolve o link em LINK e o id em INVITE_ID.
# `$1` (opcional): flags extras do `a`, como `--require-approval`.
invite_setup() {
  infra cone || { fail "montar o laboratório"; return 1; }
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR ${1:-}"
  compose up -d a >/dev/null
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; return 1; }
  A_ID=$(peer_id a)
  say a "/invite"
  wait_log a "convite[^:]*: kin://invite/" 15 || { fail "a não gerou o convite"; logs a; return 1; }
  LINK=$(logs a | grep -oE 'kin://invite/[A-Za-z0-9_-]+' | tail -1)
  say a "/invites"
  wait_log a "convite [0-9a-f]{8}: vence em" 10 || { fail "/invites não listou o convite"; logs a; return 1; }
  INVITE_ID=$(logs a | grep -oE 'convite [0-9a-f]{8}: vence em' | tail -1 | awk '{print $2}' | tr -d ':')
}

scenario_invite_single_use() {
  echo "== Convite de uso único: o primeiro abre a conversa, o segundo com o mesmo link não"
  invite_setup || return
  pass "/invites lista o convite vivo ($INVITE_ID)"

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --accept $LINK"
  compose up -d b >/dev/null
  if wait_log a "conversa cifrada pronta" 45 && wait_log b "conversa cifrada pronta" 45; then
    pass "primeiro uso: conversa pronta pelo link"
  else
    fail "primeiro uso: conversa não ficou pronta"; logs a | tail -20; logs b | tail -20; return
  fi
  say a "/invites"
  check "o token foi gasto (/invites vazio)" wait_log a "nenhum convite vivo" 10

  # Segundo convidado: identidade nova, mesma LAN que `a`, o mesmo link.
  export A2_ARGS="--data-dir /tmp --no-mdns --relay $RELAY_ADDR --accept $LINK"
  compose up -d a2 >/dev/null
  check "a2 alcançou o a (a recusa vem do convite, não da rede)" wait_log a2 "conectado a $A_ID" 45
  if wait_log a2 "conversa cifrada pronta" 25; then
    fail "segundo uso do mesmo link abriu conversa"; logs a2 | tail -20
  else
    pass "segundo uso do mesmo link não abre conversa"
  fi
  check "a avisou que o a2 não foi admitido (P34)" wait_log a "não admitido" 10
  [ "$(logs a | grep -c 'conversa cifrada pronta')" -eq 1 ] \
    && pass "a tem uma conversa só (a do primeiro convidado)" \
    || fail "a tem mais de uma conversa"
}

scenario_invite_revoked() {
  echo "== Convite revogado: /revoke-invite cancela o link antes do uso"
  invite_setup || return
  pass "/invites lista o convite vivo ($INVITE_ID)"

  say a "/revoke-invite $INVITE_ID"
  check "/revoke-invite confirma" wait_log a "convite $INVITE_ID revogado" 10
  say a "/invites"
  check "o convite sumiu da lista" wait_log a "nenhum convite vivo" 10

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --accept $LINK"
  compose up -d b >/dev/null
  check "b alcançou o a (a recusa vem do convite, não da rede)" wait_log b "conectado a $A_ID" 45
  if wait_log b "conversa cifrada pronta" 25; then
    fail "link revogado abriu conversa"; logs b | tail -20
  else
    pass "link revogado não abre conversa"
  fi
  check "a avisou que o b não foi admitido (P34)" wait_log a "não admitido" 10
  [ "$(logs a | grep -c 'conversa cifrada pronta')" -eq 0 ] \
    && pass "a não tem conversa nenhuma" \
    || fail "a abriu conversa com o link revogado"
}

# Aprovação do convidante (P18 d, ADR 0039): `a` sobe com `--require-approval`, `b` aceita o link e o pedido
# de entrada espera. Deixa B_ID preenchido. Falha (return 1) se o pedido não aparece ou se a conversa abriu sozinha.
approval_request() {
  invite_setup "--require-approval" || return 1
  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --accept $LINK"
  compose up -d b >/dev/null
  check "b alcançou o a" wait_log b "conectado a $A_ID" 45 || return 1
  B_ID=$(peer_id b)
  check "a mostra o pedido de entrada do b" wait_log a "pedido de entrada de $B_ID" 30 || { logs a | tail -20; return 1; }
  if wait_log a "conversa cifrada pronta" 10; then
    fail "a conversa abriu antes do /allow"; logs a | tail -20; return 1
  fi
  pass "o link sozinho não abre conversa: o pedido espera o /allow"
}

scenario_invite_approved() {
  echo "== Aprovação do convidante: /allow abre a conversa que o link sozinho não abriu"
  approval_request || return
  say a "/allow $B_ID"
  check "/allow confirma" wait_log a "$B_ID aprovado" 10
  if wait_log a "conversa cifrada pronta" 45 && wait_log b "conversa cifrada pronta" 45; then
    pass "depois do /allow a conversa fica pronta nos dois lados"
  else
    fail "a conversa não abriu depois do /allow"; logs a | tail -20; logs b | tail -20; return
  fi
  say a "/invites"
  check "o token foi gasto quando a conversa nasceu (/invites vazio)" wait_log a "nenhum convite vivo" 10
}

scenario_invite_denied() {
  echo "== Aprovação do convidante: /deny não abre conversa e não revoga o link"
  approval_request || return
  local live_lines="convite $INVITE_ID: vence em" before
  before=$(logs a | grep -c "$live_lines")
  say a "/deny $B_ID"
  check "/deny confirma" wait_log a "$B_ID negado" 10
  if wait_log a "conversa cifrada pronta" 25 || wait_log b "conversa cifrada pronta" 5; then
    fail "a conversa abriu depois do /deny"; logs a | tail -20; logs b | tail -20; return
  fi
  pass "depois do /deny não há conversa em nenhum dos lados"
  # Prova que o /deny dispensou o pedido (sem ele, o pedido ainda estaria pendente e o /allow o abriria).
  say a "/allow $B_ID"
  check "o pedido saiu da lista pendente" wait_log a "nenhum pedido de entrada pendente" 10
  say a "/invites"
  for _ in $(seq 20); do
    [ "$(logs a | grep -c "$live_lines")" -gt "$before" ] && break
    sleep 0.5
  done
  [ "$(logs a | grep -c "$live_lines")" -gt "$before" ] \
    && pass "o link segue vivo depois do /deny (/invites ainda lista $INVITE_ID)" \
    || fail "o /deny revogou o link"
}

scenario_relay_failover() {
  echo "== Failover de relay: o relay reservado cai e o nó passa para o outro"
  infra cone relay2 || { fail "montar o laboratório"; return; }
  local r1=${RELAY_ADDR##*/p2p/} r2=${RELAY2_ADDR##*/p2p/}
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --relay $RELAY2_ADDR --relay-count 1"
  compose up -d a >/dev/null
  wait_log a "reserva aceita no relay $r1" 30 || { fail "a sem reserva no relay 1"; logs a; return; }
  compose stop relay >/dev/null
  check "a percebe que perdeu o relay 1" wait_log a "perdi a reserva no relay $r1" 30
  check "a reserva no relay 2" wait_log a "reserva aceita no relay $r2" 30 || { logs a | tail -20; return; }

  say a "/invite"
  wait_log a "convite[^:]*: kin://invite/" 15 || { fail "a não gerou o convite"; return; }
  LINK=$(logs a | grep -oE 'kin://invite/[A-Za-z0-9_-]+' | tail -1)
  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY2_ADDR --accept $LINK"
  compose up -d b >/dev/null
  if wait_log a "conversa cifrada pronta" 45 && wait_log b "conversa cifrada pronta" 45; then
    pass "conversa pronta pelo relay 2"
  else
    fail "conversa não ficou pronta pelo relay 2"; logs a | tail -20; logs b | tail -20; return
  fi
  check "mensagem chega" delivered b a "depois do failover"
}

# A1 (`a`) e A2 (`a2`) são devices da mesma identidade X, na mesma LAN atrás do `routera`; C (`b`) é um
# contato atrás do `routerb`. A ordem importa: a linha digitada na CLI vai à identidade da conversa
# ATUAL (a última `conversa cifrada pronta`), então A2 é ligado a A1 antes de A1 aceitar C.
# O segundo device recebe o `identity.key` de A1 por `compose cp` (modo `Master`, v3: a mestra mora nos
# dois devices). O `/pair` e o `--pair` antigos foram removidos (ADR 0035).
scenario_multi_device() {
  echo "== Multi-device atrás de NAT: descoberta, espelho e fan-out (A1 e A2 irmãos, C contato)"
  infra cone || { fail "montar o laboratório"; return; }
  # KEY_ARGS (vazio por padrão = chaves cifradas com a senha do lab; `--no-passphrase` = em claro).
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --listen /ip4/0.0.0.0/tcp/4002 ${KEY_ARGS:-}"
  compose up -d a >/dev/null
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; return; }
  A1_ID=$(peer_id a)
  local magic; magic=$(compose exec -T a head -c4 /data/identity.key 2>/dev/null)
  if [ -n "${KEY_ARGS:-}" ]; then
    check "identity.key de a está em claro (--no-passphrase)" [ "$magic" != KINK ]
  else
    check "identity.key de a está cifrado (começa por KINK)" [ "$magic" = KINK ]
  fi

  local keydir; keydir=$(mktemp -d)
  compose cp a:/data/identity.key "$keydir/identity.key" >/dev/null 2>&1 \
    || { fail "copiar o identity.key de a"; rm -rf "$keydir"; return; }
  export A2_ARGS="--data-dir /tmp --no-mdns --relay $RELAY_ADDR --dial /ip4/10.1.0.10/tcp/4002/p2p/$A1_ID ${KEY_ARGS:-}"
  compose create a2 >/dev/null 2>&1
  compose cp "$keydir/identity.key" a2:/tmp/identity.key >/dev/null 2>&1 \
    || { fail "copiar o identity.key para a2"; rm -rf "$keydir"; return; }
  rm -rf "$keydir"
  compose start a2 >/dev/null
  # O start volta antes do kin imprimir o peer id: sem esperar, A2_ID ficaria vazio e os padrões abaixo
  # ("pronta com $A2_ID") casariam com qualquer peer.
  wait_log a2 "peer id: [A-Za-z0-9]+" 15 || { fail "a2 não subiu"; logs a2; return; }
  A2_ID=$(peer_id a2)
  if wait_log a "conversa cifrada pronta com $A2_ID" 40 && wait_log a2 "conversa cifrada pronta com $A1_ID" 40; then
    pass "a2 pareado a a1 (conversa pronta)"
  else
    fail "a2 e a1 não ficaram com conversa pronta"; logs a | tail -15; logs a2 | tail -15; return
  fi
  X=$(first a "conversa cifrada pronta com $A2_ID \(identidade [A-Za-z0-9]+" | awk '{print $NF}')
  check "a2 tem a mesma identidade de a1" wait_log a2 "conversa cifrada pronta com $A1_ID \(identidade ${X}\)" 5

  contact_and_fanout
}

# Segunda metade do `multi-device` (compartilhada com `paired-device`): C (`b`) é um contato atrás do
# `routerb` que convida A1; A2 só o acha depois do `/trust` (controle negativo), com furo direto, espelho
# entre os irmãos e fan-out. Usa as globais A1_ID, A2_ID, RELAY_ADDR e KEY_ARGS, com `a` e `a2` de pé.
contact_and_fanout() {
  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR ${KEY_ARGS:-}"
  compose up -d b >/dev/null
  wait_log b "reserva aceita no relay" 30 || { fail "c sem reserva no relay"; logs b; return; }
  C_ID=$(peer_id b)
  say b "/invite"
  wait_log b "convite[^:]*: kin://invite/" 15 || { fail "c não gerou o convite"; logs b; return; }
  LINK=$(logs b | grep -oE 'kin://invite/[A-Za-z0-9_-]+' | tail -1)
  say a "/accept $LINK"
  if wait_log a "conversa cifrada pronta com $C_ID" 45 && wait_log b "conversa cifrada pronta com $A1_ID" 45; then
    pass "a1 aceita o convite de c (conversa pronta)"
  else
    fail "a1 e c não ficaram com conversa pronta"; logs a | tail -15; logs b | tail -15; return
  fi

  # Controle negativo (consentimento, ADR 0006 §5): enquanto C não confia em X, a2 não acha C.
  sleep 20
  if logs a2 | grep -q "$C_ID"; then
    fail "a2 mencionou c antes de c confiar em X (controle negativo)"
  else
    pass "a2 não acha c antes do /trust (controle negativo)"
  fi

  say b "/trust"
  if wait_log a2 "conversa cifrada pronta com $C_ID" 60; then
    pass "a2 acha c sozinho (sem --dial) depois do /trust"
  else
    fail "a2 não achou c em 60s"; logs a2 | tail -20; logs b | tail -20; return
  fi
  if wait_direct a2 "$C_ID" 45 || wait_direct b "$A2_ID" 5; then
    pass "hole punching a2 <-> c"
  else
    fail "sem conexão direta a2 <-> c em 45s"
  fi

  # Espelho: a1 e a2 falam com C pela identidade; o outro irmão imprime `eu → ...` com o mesmo MessageId.
  mirrored() { # remetente, irmão, texto
    local id_sent id_mirror id_recv
    say "$1" "$3"
    wait_log b "peer: $3" 20 && wait_log "$2" "eu → [A-Za-z0-9]+: $3" 20 || return 1
    id_sent=$(logs "$1" | grep -oE "\[[0-9a-f]+\] você: $3" | head -1 | grep -oE '[0-9a-f]{8}' | head -1)
    id_mirror=$(logs "$2" | grep -oE "\[[0-9a-f]+\] eu → [A-Za-z0-9]+: $3" | head -1 | grep -oE '[0-9a-f]{8}' | head -1)
    id_recv=$(logs b | grep -oE "\[[0-9a-f]+\] peer: $3" | head -1 | grep -oE '[0-9a-f]{8}' | head -1)
    [ -n "$id_sent" ] && [ "$id_sent" = "$id_mirror" ] && [ "$id_sent" = "$id_recv" ] \
      && [ "$(logs b | grep -c "peer: $3")" -eq 1 ]
  }
  check "a1 -> c: chega 1 vez, a2 espelha, mesmo MessageId" mirrored a a2 "oi de A1"
  check "a2 -> c: chega 1 vez, a1 espelha, mesmo MessageId" mirrored a2 a "oi de A2"

  # Fan-out: C fala com a identidade X e os dois devices recebem.
  say b "oi de C"
  check "c -> X: a1 recebe" wait_log a "peer: oi de C" 20 || {
    # P35: quem é quem nos logs (o "entregue"/"na fila" de c agora traz o Peer ID do device).
    echo "--- peers: a1=${A1_ID:-?} a2=${A2_ID:-?} a3=${A3_ID:-?} c=${C_ID:-?}"
    echo "--- log de a"; logs a | tail -40
    echo "--- log de a2"; logs a2 | tail -25
    echo "--- log de a3"; logs a3 2>/dev/null | tail -25
    echo "--- log de b (c)"; logs b | tail -40
  }
  check "c -> X: a2 recebe" wait_log a2 "peer: oi de C" 20
}

# O mesmo cenário com as chaves em claro (`--no-passphrase`): cobre o caminho sem senha.
scenario_multi_device_plain() {
  KEY_ARGS="--no-passphrase" scenario_multi_device
}

# P30 2d-iii (ADR 0020 e 0021): A1 (`a`) é o PRIMÁRIO (só authority.key + authority.cert, sem a mestra) e
# A2 (`a2`) um device PAREADO (primary.id + device.key + mls.sqlite), sem copiar a mestra. O pareamento roda
# no HOST com o binário real (`kin cold init`, `kin pair request/answer/accept` são offline; o `node.sh` dá
# `exec kin`, então rodar `pair` no container exigiria mexer no entrypoint) e `compose cp` leva os arquivos,
# como o `multi-device` faz com o identity.key. O SAS é lido da saída do `request` e digitado (pipe) no `answer`.
# Depois vale a mesma conferência do `multi-device`: contato C, controle negativo do /trust, furo, espelho, fan-out.
scenario_paired_device() {
  echo "== Device pareado sem a mestra atrás de NAT (a = primário, a2 = pareado, c = contato)"
  local t out LINK SAS CERT ID_X rc f
  t=$(mktemp -d)
  install -m 600 passphrase "$t/lab.pass"   # senha do lab (autoridade e device); git não guarda o modo 0600
  (umask 077; echo "senha da mestra fria do lab" > "$t/master.pass")
  # Os dois arquivos da autoridade e o diretório pareado não podem ter nada da mestra dentro dos containers.
  absent_in_a2() { ! compose exec -T a2 sh -c 'ls /root/identity.key /root/authority.key /root/authority.cert' >/dev/null 2>&1; }

  out=$("$BIN" cold init --master-out "$t/cold/master.key" --authority-dir "$t/primary" \
    --master-passphrase-file "$t/master.pass" --passphrase-file "$t/lab.pass" 2>&1) \
    || { fail "kin cold init"; echo "$out"; rm -rf "$t"; return; }
  ID_X=$(echo "$out" | sed -n 's/^identidade: //p')
  check "cold init gerou a identidade e a autoridade" [ -n "$ID_X" ] || { rm -rf "$t"; return; }

  out=$("$BIN" pair request --data-dir "$t/paired" --identity "$ID_X" --passphrase-file "$t/lab.pass" 2>&1) \
    || { fail "kin pair request"; echo "$out"; rm -rf "$t"; return; }
  LINK=$(echo "$out" | grep -oE 'kin://pair-request/[A-Za-z0-9_-]+' | head -1)
  SAS=$(echo "$out" | grep -oE '[0-9]{6}-[0-9]{6}-[0-9]{6}-[0-9]{6}' | head -1)
  { [ -n "$LINK" ] && [ -n "$SAS" ]; } || { fail "pair request sem link ou código"; echo "$out"; rm -rf "$t"; return; }
  echo "$LINK" > "$t/req.txt"

  # Código errado: o primário não emite certificado (o SAS digitado é o único elo humano).
  out=$(printf '%s\n' "000000-000000-000000-000000" | "$BIN" pair answer --data-dir "$t/primary" \
    --request "$t/req.txt" --passphrase-file "$t/lab.pass" 2>&1); rc=$?
  check "answer com o código errado falha e não imprime certificado" \
    bash -c '[ "$1" -ne 0 ] && ! grep -q "kin://pair-cert/" <<<"$2"' _ "$rc" "$out"

  out=$(printf '%s\n' "$SAS" | "$BIN" pair answer --data-dir "$t/primary" \
    --request "$t/req.txt" --passphrase-file "$t/lab.pass" 2>&1) \
    || { fail "kin pair answer com o código certo"; echo "$out"; rm -rf "$t"; return; }
  CERT=$(echo "$out" | grep -oE 'kin://pair-cert/[A-Za-z0-9_-]+' | head -1)
  [ -n "$CERT" ] || { fail "answer sem certificado"; echo "$out"; rm -rf "$t"; return; }
  echo "$CERT" > "$t/cert.txt"
  out=$("$BIN" pair accept --data-dir "$t/paired" --certificate "$t/cert.txt" --passphrase-file "$t/lab.pass" 2>&1) \
    || { fail "kin pair accept"; echo "$out"; rm -rf "$t"; return; }
  pass "pareamento no host: request -> answer (código digitado) -> accept"

  # Um diretório com pedido pendente (sem accept) não pode virar mestra nem abrir o chat.
  "$BIN" pair request --data-dir "$t/pending" --identity "$ID_X" --passphrase-file "$t/lab.pass" >/dev/null 2>&1
  KIN_PASSPHRASE_FILE="$t/lab.pass" timeout 30 "$BIN" --data-dir "$t/pending" --no-mdns --no-nostr </dev/null >/dev/null 2>&1; rc=$?
  check "kin normal num diretório pendente falha (e não trava: 124 seria timeout)" \
    bash -c '[ "$1" -ne 0 ] && [ "$1" -ne 124 ]' _ "$rc"
  check "o diretório pendente não ganhou identity.key" [ ! -e "$t/pending/identity.key" ]

  infra cone || { fail "montar o laboratório"; rm -rf "$t"; return; }
  # Primário: os dois arquivos da autoridade, copiados antes de ligar (como o identity.key no multi-device).
  export A_ARGS="--data-dir /tmp --no-mdns --relay $RELAY_ADDR --listen /ip4/0.0.0.0/tcp/4002"
  compose create a >/dev/null 2>&1
  for f in authority.key authority.cert; do
    compose cp "$t/primary/$f" "a:/tmp/$f" >/dev/null 2>&1 || { fail "copiar $f para a"; rm -rf "$t"; return; }
  done
  compose start a >/dev/null
  wait_log a "peer id: [A-Za-z0-9]+" 15 || { fail "a não subiu"; logs a; rm -rf "$t"; return; }
  check "a abre como primário da identidade do cold init" wait_log a "primário da identidade $ID_X" 10
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; rm -rf "$t"; return; }
  A1_ID=$(peer_id a)

  # Pareado: SÓ o que o accept deixou (primary.id, device.key, mls.sqlite e o WAL do SQLite, se houver).
  # O diretório de dados do `a2` é `/root`, NÃO `/tmp`: o `compose cp` deixa os arquivos com o uid do host (1000) e
  # o SQLite do `mls.sqlite` abre com O_CREAT; em diretório com sticky bit (`/tmp`) o kernel com
  # `fs.protected_regular` ligado (Arch) nega isso até para o root ("Accessing storage failed"). Os outros
  # cenários só LEEM o arquivo copiado (identity.key), por isso não sofrem.
  export A2_ARGS="--data-dir /root --no-mdns --relay $RELAY_ADDR --dial /ip4/10.1.0.10/tcp/4002/p2p/$A1_ID"
  compose create a2 >/dev/null 2>&1
  # `db.key` (P13 fatia 3): com senha, o `mls.sqlite` do pareamento nasce cifrado e só abre com ele; sem senha
  # (`--no-passphrase`) não existe.
  for f in primary.id device.key db.key mls.sqlite mls.sqlite-wal mls.sqlite-shm; do
    [ -e "$t/paired/$f" ] || { case $f in *-wal|*-shm|db.key) continue ;; esac; fail "falta $f no diretório pareado"; rm -rf "$t"; return; }
    compose cp "$t/paired/$f" "a2:/root/$f" >/dev/null 2>&1 || { fail "copiar $f para a2"; rm -rf "$t"; return; }
  done
  rm -rf "$t"
  compose start a2 >/dev/null
  wait_log a2 "peer id: [A-Za-z0-9]+" 15 || { fail "a2 não subiu"; logs a2; return; }
  A2_ID=$(peer_id a2)
  check "a2 abre como device pareado da identidade do cold init" wait_log a2 "device pareado da identidade $ID_X" 10
  check "a2 não tem a mestra nem a autoridade" absent_in_a2

  if wait_log a "conversa cifrada pronta com $A2_ID" 40 && wait_log a2 "conversa cifrada pronta com $A1_ID" 40; then
    pass "a2 pareado a a1 (conversa pronta)"
  else
    fail "a2 e a1 não ficaram com conversa pronta"; logs a | tail -15; logs a2 | tail -15; return
  fi
  check "a2 tem a identidade do primário" wait_log a2 "conversa cifrada pronta com $A1_ID \(identidade ${ID_X}\)" 5

  contact_and_fanout
}

# Pareia um device novo no host (como o `paired-device`): `pair request` em $2, `answer` no primário $1 com o
# código digitado por pipe, `accept` em $2. Uso: pair_on_host PRIMARIO PAREADO IDENTIDADE SENHA.
pair_on_host() {
  local out link sas cert
  out=$("$BIN" pair request --data-dir "$2" --identity "$3" --passphrase-file "$4" 2>&1) || { echo "$out"; return 1; }
  link=$(echo "$out" | grep -oE 'kin://pair-request/[A-Za-z0-9_-]+' | head -1)
  sas=$(echo "$out" | grep -oE '[0-9]{6}-[0-9]{6}-[0-9]{6}-[0-9]{6}' | head -1)
  [ -n "$link" ] && [ -n "$sas" ] || { echo "$out"; return 1; }
  echo "$link" > "$2.req"
  out=$(printf '%s\n' "$sas" | "$BIN" pair answer --data-dir "$1" --request "$2.req" --passphrase-file "$4" 2>&1) \
    || { echo "$out"; return 1; }
  cert=$(echo "$out" | grep -oE 'kin://pair-cert/[A-Za-z0-9_-]+' | head -1)
  [ -n "$cert" ] || { echo "$out"; return 1; }
  echo "$cert" > "$2.cert"
  "$BIN" pair accept --data-dir "$2" --certificate "$2.cert" --passphrase-file "$4" 2>&1
}

# Última linha de `/devices` do primário `a` sobre o device $1 ("<peer>  emitido em ...  ativo|revogado[, conectado]").
device_line() { logs a | grep -E "^$1  emitido em" | tail -1; }
# Como `wait_log`, mas só nas linhas depois da `desde`-ésima (o log do container persiste entre stop e start).
wait_log_after() { # container, desde, padrão, segundos
  local c=$1 from=$2 pat=$3 t=${4:-30}
  for _ in $(seq $((t * 2))); do
    logs "$c" | tail -n +$((from + 1)) | grep -qE "$pat" && return 0
    sleep 0.5
  done
  return 1
}
note() { echo "  NOTE  $1"; }   # observação que não reprova o cenário (decisão do dono: a fase 4 só observa)

# P30 fatia 7 (ADR 0011, decisão 6.7): revogação de irmãos pareados atrás de NAT. a = primário, a2 e a3 =
# irmãos pareados, c (`b`) = contato atrás de outro NAT. SKEL=1 pula os `/revoke` (prova do esqueleto: os
# checks que dependem da revogação têm de falhar).
scenario_revoke_device() {
  echo "== Revogar irmãos pareados (a = primário, a2 e a3 = irmãos, c = contato)"
  local t out ID_X f pfx
  t=$(mktemp -d)
  install -m 600 passphrase "$t/lab.pass"
  (umask 077; echo "senha da mestra fria do lab" > "$t/master.pass")

  out=$("$BIN" cold init --master-out "$t/cold/master.key" --authority-dir "$t/primary" \
    --master-passphrase-file "$t/master.pass" --passphrase-file "$t/lab.pass" 2>&1) \
    || { fail "kin cold init"; echo "$out"; rm -rf "$t"; return; }
  ID_X=$(echo "$out" | sed -n 's/^identidade: //p')
  [ -n "$ID_X" ] || { fail "cold init sem identidade"; rm -rf "$t"; return; }
  pair_on_host "$t/primary" "$t/p2" "$ID_X" "$t/lab.pass" >/dev/null \
    && pair_on_host "$t/primary" "$t/p3" "$ID_X" "$t/lab.pass" >/dev/null \
    || { fail "parear a2 e a3 no host"; rm -rf "$t"; return; }

  infra cone || { fail "montar o laboratório"; rm -rf "$t"; return; }
  # `/root` e não `/tmp`: o backup restaurado por `compose cp` fica com o uid do host e o sticky bit de `/tmp`
  # (fs.protected_regular) deixa o SQLite readonly (mesmo motivo do `a2` no `paired-device`).
  export A_ARGS="--data-dir /root --no-mdns --relay $RELAY_ADDR --listen /ip4/0.0.0.0/tcp/4002"
  compose create a >/dev/null 2>&1
  for f in authority.key authority.cert issued; do   # `issued/` tem os certificados emitidos (o /revoke precisa deles)
    compose cp "$t/primary/$f" "a:/root/$f" >/dev/null 2>&1 || { fail "copiar $f para a"; rm -rf "$t"; return; }
  done
  compose start a >/dev/null
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; rm -rf "$t"; return; }
  A1_ID=$(peer_id a)

  local n d
  for n in 2 3; do
    export "A${n}_ARGS=--data-dir /root --no-mdns --relay $RELAY_ADDR --dial /ip4/10.1.0.10/tcp/4002/p2p/$A1_ID"
    compose create "a$n" >/dev/null 2>&1
    d="$t/p$n"
    for f in primary.id device.key db.key mls.sqlite mls.sqlite-wal mls.sqlite-shm; do
      [ -e "$d/$f" ] || continue
      compose cp "$d/$f" "a$n:/root/$f" >/dev/null 2>&1 || { fail "copiar $f para a$n"; rm -rf "$t"; return; }
    done
  done
  compose start a2 >/dev/null
  wait_log a2 "peer id: [A-Za-z0-9]+" 15 || { fail "a2 não subiu"; logs a2; rm -rf "$t"; return; }
  A2_ID=$(peer_id a2)
  compose start a3 >/dev/null
  wait_log a3 "peer id: [A-Za-z0-9]+" 15 || { fail "a3 não subiu"; logs a3; rm -rf "$t"; return; }
  A3_ID=$(peer_id a3)
  wait_log a "conversa cifrada pronta com $A2_ID" 40 && wait_log a "conversa cifrada pronta com $A3_ID" 40 \
    || { fail "a não ficou com conversa pronta com a2 e a3"; logs a | tail -15; rm -rf "$t"; return; }
  pass "a2 e a3 pareados ao primário a (conversa pronta)"
  BACKUP="$t/backup"; mkdir -p "$BACKUP"

  # Fase 0: c entra, confia em X, e fala com os irmãos (controle: sem isso a revogação não prova nada).
  # c leva `--dial` do primário (pelo relay) para reconectar quando for reiniciado na fase 2.
  KEY_ARGS="--dial $RELAY_ADDR/p2p-circuit/p2p/$A1_ID"
  contact_and_fanout
  [ $FAILED -eq 0 ] || { echo "(fase 0 falhou: o resto não vale)"; rm -rf "$t"; return; }
  wait_log a3 "conversa cifrada pronta com $C_ID" 60 && pass "a3 também acha c" || fail "a3 não achou c"

  # Fase 1: revogar o a2 com c online.
  say a "/devices"; sleep 2
  check "/devices: a2 ativo e conectado" bash -c 'grep -qE "ativo, conectado" <<<"$1"' _ "$(device_line "$A2_ID")"
  pfx=${A2_ID:8:8}
  if [ -z "${SKEL:-}" ]; then
    say a "/revoke $pfx"
    check "/revoke sem confirmo só pergunta (c e a2 seguem normais)" wait_log a "Para confirmar: /revoke $A2_ID confirmo" 10
    say a "/revoke $A2_ID confirmo"
  fi
  check "c é avisado: device a2 revogado" wait_log b "device $A2_ID da identidade $ID_X foi revogado" 30
  say a2 "msg do a2 revogado"
  sleep 8
  check "a2 -> c depois da revogação: não chega" bash -c '! grep -q "peer: msg do a2 revogado" <<<"$1"' _ "$(logs b)"
  say b "c depois da revogação do a2"
  check "c -> X: chega em a" wait_log a "peer: c depois da revogação do a2" 20
  sleep 5
  check "c -> X: não chega no a2" bash -c '! grep -q "peer: c depois da revogação do a2" <<<"$1"' _ "$(logs a2)"
  say a "/devices"; sleep 2
  check "/devices: a2 revogado" bash -c 'grep -q "revogado" <<<"$1"' _ "$(device_line "$A2_ID")"

  # Fase 2: com c parado, revogar o a3 (e guardar o banco do primário antes, para a fase 4).
  compose stop b >/dev/null 2>&1
  compose stop a >/dev/null 2>&1                       # backup do primário ANTES da revogação do a3 (fase 4)
  for f in mls.sqlite mls.sqlite-wal mls.sqlite-shm; do compose cp "a:/root/$f" "$BACKUP/$f" >/dev/null 2>&1 || true; done
  compose start a >/dev/null
  wait_log a "primário da identidade $ID_X" 20 || { fail "a não reabriu como primário"; logs a | tail; rm -rf "$t"; return; }
  if [ -z "${SKEL:-}" ]; then
    say a "/revoke $A3_ID confirmo"
    check "a revogou o a3 (c offline)" wait_log a "device $A3_ID revogado" 15
  fi
  check "a3 recebe a revogação (está conectado ao primário)" wait_log a3 "da identidade $ID_X foi revogado" 40
  # Fase 2: c volta (o `--dial` do primário está nos argumentos dele) e deve aprender a revogação que perdeu.
  # Não coberto no lab: "contato só conectado ao device revogado" (ADR 0011). Nenhum nó religa sozinho (P19/P21) e
  # a CLI não tem comando de discar, então não dá para impor "c só alcança o a3" sem código novo.
  local nb
  nb=$(logs b | wc -l)
  compose start b >/dev/null
  check "c (estava offline) aprende a revogação do a3 ao voltar" \
    wait_log_after b "$nb" "device $A3_ID da identidade $ID_X foi revogado" 60

  # Fase 4: restaurar o banco do primário anterior à revogação do a3 (ADR 0023, S4: perda de banco "desrevoga").
  compose stop a >/dev/null 2>&1
  for f in mls.sqlite mls.sqlite-wal mls.sqlite-shm; do
    [ -e "$BACKUP/$f" ] && compose cp "$BACKUP/$f" "a:/root/$f" >/dev/null 2>&1
  done
  compose start a >/dev/null
  if wait_log a "primário da identidade $ID_X" 30; then
    say a "/devices"; sleep 2
    case "$(device_line "$A3_ID")" in
      *revogado*) note "fase 4: o banco restaurado AINDA marca o a3 como revogado (S4 do ADR 0023 não se reproduziu)" ;;
      *ativo*) note "fase 4: o banco restaurado devolveu o a3 a ativo (S4 do ADR 0023 confirmado)" ;;
      *) note "fase 4: /devices não listou o a3 depois do restauro" ;;
    esac
    [ -z "${SKEL:-}" ] && say a "/revoke $A3_ID confirmo" && sleep 3
    say a "/devices"; sleep 2
    check "repetir o /revoke deixa o a3 revogado" bash -c 'grep -q "revogado" <<<"$1"' _ "$(device_line "$A3_ID")"
  else
    note "fase 4: o primário não subiu com o banco restaurado (P16)"; logs a | tail -5
  fi
  rm -rf "$t"
}

# A1 (`a`) e C (`b`) conversam; SÓ DEPOIS entra A2 (`a2`), irmão de A1 com banco vazio, e deve receber o
# histórico pelo pull da sincronização (ADR 0009). Confere por MessageId e direção, não só por texto.
# Como em `multi-device`, A2 recebe o `identity.key` de A1 por `compose cp`. Para o `/history` de A2
# achar a identidade de C, C precisa confiar em X (senão A2 nunca vê C e `/history` não a resolve).
# Não cobre: reconexão sem reimportar (reiniciar o A2 esbarraria no P16, re-handshake após perda de estado).
scenario_history_sync() {
  echo "== Histórico para um device novo (A1 e C conversam; A2 entra depois)"
  infra cone || { fail "montar o laboratório"; return; }
  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --listen /ip4/0.0.0.0/tcp/4002"
  compose up -d a >/dev/null
  wait_log a "reserva aceita no relay" 30 || { fail "a sem reserva no relay"; logs a; return; }
  A1_ID=$(peer_id a)
  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR"
  compose up -d b >/dev/null
  wait_log b "reserva aceita no relay" 30 || { fail "c sem reserva no relay"; logs b; return; }
  C_ID=$(peer_id b)
  say b "/invite"
  wait_log b "convite[^:]*: kin://invite/" 15 || { fail "c não gerou o convite"; logs b; return; }
  LINK=$(logs b | grep -oE 'kin://invite/[A-Za-z0-9_-]+' | tail -1)
  say a "/accept $LINK"
  if wait_log a "conversa cifrada pronta com $C_ID" 45 && wait_log b "conversa cifrada pronta com $A1_ID" 45; then
    pass "a1 aceita o convite de c (conversa pronta)"
  else
    fail "a1 e c não ficaram com conversa pronta"; logs a | tail -15; logs b | tail -15; return
  fi
  Y=$(first a "conversa cifrada pronta com $C_ID \(identidade [A-Za-z0-9]+" | awk '{print $NF}')

  # Três mensagens: a1 -> c, c -> a1, e a resposta de a1 à de c (`/r`).
  msg_id() { logs "$1" | grep -oE "\[[0-9a-f]{8}\] $2" | head -1 | grep -oE '[0-9a-f]{8}' | head -1; }
  say a "h1"; wait_log b "peer: h1" 20 || { fail "h1 não chegou a c"; return; }
  say b "h2"; wait_log a "peer: h2" 20 || { fail "h2 não chegou a a1"; return; }
  ID1=$(msg_id a "você: h1"); ID2=$(msg_id a "peer: h2")
  say a "/r $ID2 h3"; wait_log a "você: h3" 20 && wait_log b "↳ $ID2" 20 || { fail "h3 (resposta) não chegou a c"; return; }
  ID3=$(msg_id a "você: h3")
  [ -n "$ID1" ] && [ -n "$ID2" ] && [ -n "$ID3" ] && pass "a1 e c trocaram 3 mensagens (uma resposta)" \
    || { fail "não achei os MessageIds no log de a1"; return; }

  # Só agora entra o a2, com banco vazio.
  local keydir; keydir=$(mktemp -d)
  compose cp a:/data/identity.key "$keydir/identity.key" >/dev/null 2>&1 \
    || { fail "copiar o identity.key de a"; rm -rf "$keydir"; return; }
  export A2_ARGS="--data-dir /tmp --no-mdns --relay $RELAY_ADDR --dial /ip4/10.1.0.10/tcp/4002/p2p/$A1_ID"
  compose create a2 >/dev/null 2>&1
  compose cp "$keydir/identity.key" a2:/tmp/identity.key >/dev/null 2>&1 \
    || { fail "copiar o identity.key para a2"; rm -rf "$keydir"; return; }
  rm -rf "$keydir"
  compose start a2 >/dev/null
  wait_log a2 "peer id: [A-Za-z0-9]+" 15 || { fail "a2 não subiu"; logs a2; return; }
  check "a2 recebe as 3 mensagens do irmão (histórico completo)" \
    wait_log a2 "histórico de $A1_ID: 3 novas, completo" 40 || { logs a2 | tail -20; logs a | tail -20; }

  # A2 só resolve a identidade de C depois de vê-la (`ConversationReady`), então C confia em X.
  say b "/trust"
  wait_log a2 "conversa cifrada pronta com $C_ID" 60 || { fail "a2 não achou c em 60s"; logs a2 | tail -20; return; }
  say a2 "/history $Y 10"
  wait_log a2 "histórico com $Y: 3 linhas" 15 || { fail "a2 não imprimiu o histórico"; logs a2 | tail -20; return; }
  check "a2 /history: h1 enviada, mesmo MessageId" wait_log a2 "\[$ID1\] você: h1" 5
  check "a2 /history: h2 recebida, mesmo MessageId" wait_log a2 "\[$ID2\] peer: h2" 5
  check "a2 /history: h3 é resposta a h2, mesmo MessageId" wait_log a2 "\[$ID3\] você ↳ $ID2: h3" 5

  # Mensagem nova depois da sincronização: chega 1 vez em cada irmão e entra no histórico sem duplicar.
  say b "h4"
  wait_log a "peer: h4" 20 && wait_log a2 "peer: h4" 20 || { fail "h4 não chegou aos dois irmãos"; return; }
  ID4=$(msg_id a2 "peer: h4")
  say a2 "/history $Y 2"
  wait_log a2 "histórico com $Y: 2 linhas" 15 || { fail "a2 não imprimiu o segundo histórico"; return; }
  check "h4 chegou 1 vez a cada irmão e aparece 1 vez no /history do a2" \
    test "$(logs a2 | grep -cE "\[$ID4\] peer: h4")" -eq 2 -a "$(logs a | grep -cE "\[$ID4\] peer: h4")" -eq 1
}

# Quantas vezes `padrão` aparece no log do container, e espera até `n` vezes (o log de um container
# parado e religado com `stop`/`start` é cumulativo, então "apareceu" não basta: precisa ser de novo).
count_log() { logs "$1" | grep -cE "$2"; }
wait_log_n() { # container, padrão, n, segundos
  local c=$1 pat=$2 n=$3 t=${4:-30}
  for _ in $(seq $((t * 2))); do
    [ "$(count_log "$c" "$pat")" -ge "$n" ] && return 0
    sleep 0.5
  done
  return 1
}

# Reconexão automática (P32, ADR 0036): a e b conversam; `a` cai; `b` manda uma mensagem (fica na fila);
# `a` volta COM A MESMA IDENTIDADE (`stop`/`start` mantém o /data, então não há re-handshake: P16). O
# laboratório roda com --no-mdns e ninguém digita nada depois da volta, então só a rediscagem do `b` (pelo
# endereço de circuito do relay que o `a` anunciou no identify) pode fazer a mensagem chegar. A rede
# pública é 203.0.113.0/24 justamente para esse endereço passar no filtro de endereços do chat.
scenario_reconnect() {
  echo "== Reconexão automática: o peer cai e volta, e quem tinha mensagem na fila o redisca sozinho"
  setup cone || { fail "montar o laboratório"; return; }
  check "mensagem chega antes da queda (rede viva)" delivered a b "antes da queda"

  compose stop a >/dev/null
  check "b percebe que a caiu" wait_log b "desconectado de $A_ID" 30 || { logs b | tail -20; return; }
  say b "enquanto a esteve fora"
  check "a mensagem fica na fila do b (a está fora)" wait_log b "na fila, sai quando o peer conectar" 10

  compose start a >/dev/null
  wait_log_n a "reserva aceita no relay" 2 30 \
    || { fail "a não refez a reserva no relay depois de voltar"; logs a | tail -20; return; }
  # Nenhum comando manual daqui em diante. Espera até 90 s: a rampa de espera do b é 2, 4, 8, 16, 32 s.
  if wait_log a "peer: enquanto a esteve fora" 90; then
    pass "a mensagem enfileirada chega ao a sem nenhum comando (rediscagem automática do b)"
  else
    fail "a mensagem não chegou em 90 s: o b não rediscou o a"; logs b | tail -30; logs a | tail -30; return
  fi
  check "b reconectou ao a (segunda conexão)" wait_log_n b "conectado a $A_ID" 2 10
}

# O `b` reinicia (mesmo /data, SEM o `--dial` do começo) com uma mensagem na fila para o `a`, que está fora.
# Só a rediscagem a partir dos endereços gravados no disco (P32 a, schema v8) pode trazer o `a` de volta: sem
# `--dial`, sem mDNS e sem nenhum comando depois do reinício.
scenario_reconnect_restart() {
  echo "== Reconexão depois de reiniciar: o b reinicia sem --dial e redisca o a pelo endereço gravado no disco"
  setup cone || { fail "montar o laboratório"; return; }
  check "mensagem chega antes da queda (rede viva)" delivered a b "antes da queda"

  compose stop a >/dev/null
  check "b percebe que a caiu" wait_log b "desconectado de $A_ID" 30 || { logs b | tail -20; return; }
  say b "enquanto o b reiniciava"
  check "a mensagem fica na fila do b (a está fora)" wait_log b "na fila, sai quando o peer conectar" 10

  # Mesmo container: o /data (identidade, conversa, fila e endereços) e o /tmp sobrevivem; só os argumentos mudam.
  compose exec -T b sh -c "echo '--data-dir /data --no-mdns --relay $RELAY_ADDR' > /tmp/kin-args"
  compose stop b >/dev/null
  compose start b >/dev/null
  wait_log_n b "reserva aceita no relay" 2 30 \
    || { fail "b não refez a reserva no relay depois de reiniciar"; logs b | tail -20; return; }

  compose start a >/dev/null
  wait_log_n a "reserva aceita no relay" 2 30 \
    || { fail "a não refez a reserva no relay depois de voltar"; logs a | tail -20; return; }
  # Nenhum comando manual daqui em diante. Espera até 90 s: a rampa de espera do b é 2, 4, 8, 16, 32 s.
  if wait_log a "peer: enquanto o b reiniciava" 90; then
    pass "a mensagem enfileirada chega ao a depois do reinício do b (rediscagem pelo endereço do disco)"
  else
    fail "a mensagem não chegou em 90 s: o b reiniciado não rediscou o a"; logs b | tail -30; logs a | tail -30; return
  fi
}

# P36 (ADR 0043: o `Hello` leva o `state_id` da instância de estado): o `b` some sem avisar (o roteador dele descarta tudo,
# sem RST), então a conexão direta a↔b fica meio-aberta no `a`; o `b` volta com a MESMA identidade e SEM o
# mls.sqlite (perdeu o banco) e o `a` manda uma mensagem. Mede quando a conexão antiga cai no `a`, quando a
# conversa é refeita, se o `a` imprime `(entregue ...)` e se o `b` recebe.
# Modos: `rst` = o mapeamento do NAT do `b` continua vivo, então o primeiro pacote do `a` na conexão antiga chega ao
# `b` novo, que responde RST; `silent` = o `b` novo não responde RST (como um firewall que descarta), o caso
# verdadeiramente meio-aberto. Todos os marcos saem em segundos desde o corte da rede (`MEDIDO[modo]:`).
stale_connection() { # modo: rst | silent
  local mode=$1
  setup cone || { fail "montar o laboratório"; return; }
  wait_direct a "$B_ID" 60 || { fail "sem conexão direta a↔b: não há o que medir (o hole punching varia; rode de novo)"; return; }
  check "mensagem chega antes (rede viva)" delivered a b "antes" || return

  local d0 r0 p0 e0
  d0=$(count_log a "desconectado de $B_ID"); r0=$(count_log a "rota com $B_ID")
  p0=$(count_log a "conversa cifrada pronta"); e0=$(count_log a "\\(entregue .* a $B_ID\\)")

  # O `b` "perde o banco": mesma identidade (e Peer ID), sem o mls.sqlite (cópia do /data sem ele para um diretório novo).
  compose exec -T b sh -c 'mkdir -p /data2 && for f in /data/*; do case "$f" in /data/mls.sqlite*) ;; *) cp -r "$f" /data2/ ;; esac; done' \
    || { fail "copiar as chaves do b"; return; }
  compose exec -T b sh -c "echo '--data-dir /data2 --no-mdns --relay $RELAY_ADDR --dial $A_CIRCUIT' > /tmp/kin-args"

  # O `b` some sem avisar: nenhum FIN nem RST chega ao `a`.
  compose exec -T routerb iptables -I FORWARD 1 -j DROP || { fail "cortar a rede do b"; return; }
  local t0=$SECONDS
  compose stop b >/dev/null
  compose exec -T routerb iptables -D FORWARD 1
  compose start b >/dev/null
  # O netns do container é recriado no `start`: a regra do modo `silent` entra depois dele.
  [ "$mode" = silent ] && compose exec -T b iptables -A OUTPUT -p tcp --tcp-flags RST RST -j DROP
  wait_log_n b "reserva aceita no relay" 2 30 || { fail "b não voltou à rede"; logs b | tail -20; return; }
  local t_back=$((SECONDS - t0))

  say a "no limbo"
  # 90 s: acima dos 60 s do `idle_connection_timeout` (node.rs). Medido com 180 s em 4 rodadas (2 por modo): o `b` nunca
  # recebeu e a conexão antiga nunca caiu; o padrão fica em 90 s para não pesar o CI (STALE_WAIT=180 repete a medição).
  local t_disc="" t_deliv="" t_recv="" t_ready="" i wait_s=${STALE_WAIT:-90}
  for i in $(seq "$wait_s"); do
    [ -z "$t_disc" ] && [ "$(count_log a "desconectado de $B_ID")" -gt "$d0" ] && t_disc=$((SECONDS - t0))
    [ -z "$t_deliv" ] && [ "$(count_log a "\\(entregue .* a $B_ID\\)")" -gt "$e0" ] && t_deliv=$((SECONDS - t0))
    [ -z "$t_ready" ] && [ "$(count_log a "conversa cifrada pronta")" -gt "$p0" ] && t_ready=$((SECONDS - t0))
    [ -z "$t_recv" ] && logs b | grep -q "peer: no limbo" && t_recv=$((SECONDS - t0))
    [ -n "$t_ready" ] && break
    sleep 1
  done
  local r1; r1=$(( $(count_log a "rota com $B_ID") - r0 ))
  echo "  MEDIDO[$mode]: a=$A_ID b=$B_ID (o log não diz qual é o menor; comparar os IDs depois)"
  echo "  MEDIDO[$mode]: b de volta à rede em ${t_back}s; a conexão antiga caiu no a em ${t_disc:-nunca (${wait_s} s)}s; conversa refeita no a em ${t_ready:-nunca (${wait_s} s)}s"
  echo "  MEDIDO[$mode]: o a imprimiu (entregue ...) em ${t_deliv:-nunca}s; o b recebeu 'no limbo' em ${t_recv:-nunca}s; rotas novas vistas no a: $r1"
  if [ -n "$t_deliv" ] && { [ -z "$t_recv" ] || [ "$t_deliv" -lt "$t_recv" ]; }; then
    echo "  MEDIDO[$mode]: (entregue) ANTES de o b receber: ack de transporte falso (a janela do P36 existe)"
  fi
  # P36 corrigido (ADR 0043): a conversa é refeita sozinha, e uma mensagem mandada DEPOIS da troca chega. O "no limbo"
  # (mandado antes de o `a` saber da perda) pode se perder: o ack de transporte é de antes da troca; só é medido, não exigido.
  check "o a refez a conversa em ${wait_s} s (conversa cifrada pronta)" test -n "$t_ready"
  [ -n "$t_ready" ] || return
  say a "depois da troca"
  check "o b recebe a mensagem mandada depois da troca" wait_log b "peer: depois da troca" 30
}
scenario_stale_connection_rst() {
  echo "== P36 (medição): conexão antiga meio-aberta, o b novo responde RST"
  stale_connection rst
}
scenario_stale_connection_silent() {
  echo "== P36 (medição): conexão antiga meio-aberta, o b novo NÃO responde RST (descarte silencioso)"
  stale_connection silent
}

# run_scenario NOME: roda um cenário (FAILED fica 1 se algo falhou).
# ---- P13 fatia 4 (ADR 0045): bancos cifrados em repouso, olhados pelos BYTES dentro do container ----
DB_TEXT="segredo-p13-lab"
# Os dois bancos têm conteúdo e NÃO começam por "SQLite format 3" (o SQLCipher cifra até o cabeçalho), e o
# `db.key` e o `identity.key` são selados (`KINK`). Só a CLI abrir não prova nada: um build sem SQLCipher abriria igual.
db_is_encrypted() { # container
  compose exec -T "$1" sh -c '
    for f in /data/mls.sqlite /data/mls.outbox.db; do
      [ -s "$f" ] || exit 1
      [ "$(head -c15 "$f")" != "SQLite format 3" ] || exit 1
    done
    [ "$(head -c4 /data/db.key)" = KINK ] && [ "$(head -c4 /data/identity.key)" = KINK ]'
}
db_is_plain() { # container: o controle, os dois bancos em claro e sem db.key
  compose exec -T "$1" sh -c '
    [ "$(head -c15 /data/mls.sqlite)" = "SQLite format 3" ] && [ "$(head -c15 /data/mls.outbox.db)" = "SQLite format 3" ] \
      && [ ! -e /data/db.key ]'
}
text_in_dbs() { compose exec -T "$1" sh -c "grep -qa -- '$2' /data/mls.sqlite* /data/mls.outbox.db*"; }
text_absent_from_dbs() { ! text_in_dbs "$@"; }

scenario_encrypted_db() {
  echo "== Banco cifrado desde o início: bytes no disco, reinício e histórico (P13 fatia 4)"
  setup cone || { fail "montar o laboratório"; return; }
  check "mensagem chega com os bancos cifrados" delivered b a "$DB_TEXT"
  Y=$(first a "conversa cifrada pronta com $B_ID \(identidade [A-Za-z0-9]+" | awk '{print $NF}')
  check "a: db.key e identity.key selados e os dois bancos não são SQLite em claro" db_is_encrypted a
  check "b: o mesmo" db_is_encrypted b
  check "a: o texto conhecido não está nos bancos nem no -wal" text_absent_from_dbs a "$DB_TEXT"

  # Reinício no mesmo /data: o estado MLS e o histórico têm de voltar do banco cifrado.
  local n; n=$(count_log a "reserva aceita no relay")
  compose stop a >/dev/null
  compose start a >/dev/null
  wait_log_n a "reserva aceita no relay" $((n + 1)) 30 \
    || { fail "a não reabriu os bancos cifrados depois de reiniciar"; logs a | tail -20; return; }
  say b "depois do reinicio"
  check "a mensagem nova chega ao a reiniciado (estado MLS veio do banco cifrado)" \
    wait_log a "peer: depois do reinicio" 90
  say a "/history $Y 10"
  check "/history do a reiniciado lista as duas mensagens (outbox cifrado)" \
    wait_log a "histórico com $Y: 2 linhas" 15
  check "a: o texto continua fora dos bancos depois do reinício" text_absent_from_dbs a "$DB_TEXT"
}

scenario_protect_migration() {
  echo "== --protect: diretório legado em claro vira cifrado sem perder a conversa; sem db.key não abre (P13 fatia 4)"
  SETUP_KEY_ARGS="--no-passphrase" setup cone || { fail "montar o laboratório"; return; }
  check "mensagem chega com tudo em claro" delivered b a "$DB_TEXT"
  Y=$(first a "conversa cifrada pronta com $B_ID \(identidade [A-Za-z0-9]+" | awk '{print $NF}')
  # Controle: a mesma busca ACHA o texto em claro. Sem isto, "o texto sumiu" não provaria nada.
  check "controle: o a está em claro (cabeçalho SQLite, sem db.key)" db_is_plain a
  check "controle: o grep acha o texto nos bancos em claro" text_in_dbs a "$DB_TEXT"

  # Reinicia o `a` com --protect (a senha vem do KIN_PASSPHRASE_FILE do node.sh): cifra as chaves e migra os bancos.
  local n; n=$(count_log a "reserva aceita no relay")
  compose exec -T a sh -c "echo '--data-dir /data --no-mdns --relay $RELAY_ADDR --open-admission --protect' > /tmp/kin-args"
  compose stop a >/dev/null
  compose start a >/dev/null
  wait_log_n a "reserva aceita no relay" $((n + 1)) 30 \
    || { fail "a não subiu com --protect"; logs a | tail -20; return; }
  check "a: chaves e os dois bancos cifrados depois do --protect" db_is_encrypted a
  check "a: o texto conhecido sumiu dos bancos" text_absent_from_dbs a "$DB_TEXT"
  say b "depois do protect"
  check "a conversa sobreviveu à migração: a mensagem nova chega ao a" wait_log a "peer: depois do protect" 90
  say a "/history $Y 10"
  check "/history do a mantém as mensagens de antes da migração" wait_log a "histórico com $Y: 2 linhas" 15

  # Sem o db.key o banco cifrado não abre, e nunca vira "chave nova" (o requisito da auditoria da fatia 2).
  local t; t=$(mktemp -d)
  compose cp a:/data/db.key "$t/db.key" >/dev/null 2>&1 || { fail "copiar o db.key de a"; rm -rf "$t"; return; }
  compose exec -T a sh -c "echo '--data-dir /data --no-mdns --relay $RELAY_ADDR --open-admission' > /tmp/kin-args; rm /data/db.key"
  compose stop a >/dev/null
  compose start a >/dev/null
  check "sem o db.key o a recusa subir" wait_log a "não está neste --data-dir" 20
  compose cp a:/data/mls.sqlite "$t/mls.sqlite" >/dev/null 2>&1
  check "o mls.sqlite continua cifrado (não foi trocado por um novo)" \
    test -s "$t/mls.sqlite" -a "$(head -c15 "$t/mls.sqlite")" != "SQLite format 3"

  # Restaura o db.key e o a volta.
  n=$(count_log a "reserva aceita no relay")
  compose cp "$t/db.key" a:/data/db.key >/dev/null 2>&1 || { fail "devolver o db.key ao a"; rm -rf "$t"; return; }
  rm -rf "$t"
  compose start a >/dev/null
  check "com o db.key de volta o a sobe de novo" wait_log_n a "reserva aceita no relay" $((n + 1)) 30
}

contains() { case "$1" in *"$2"*) return 0 ;; esac; return 1; } # texto, trecho

scenario_passwd() {
  echo "== kin passwd com o binário real: troca a senha das chaves, a conversa sobrevive, a senha velha não abre (P13)"
  setup cone || { fail "montar o laboratório"; return; }
  check "mensagem chega com as chaves e os bancos na senha do lab" delivered b a "$DB_TEXT"
  check "controle: os arquivos estão cifrados antes da troca" db_is_encrypted a

  local before after out out2 n
  before=$(compose exec -T a sh -c 'cksum /data/identity.key /data/device.key /data/db.key')
  # A senha atual é a do lab (/tmp/kin-passphrase, do node.sh); a nova vai para outro arquivo 0600.
  compose exec -T a sh -c "printf 'senha nova do lab\n' > /tmp/new.pass; chmod 600 /tmp/new.pass"
  out=$(compose exec -T a kin passwd --data-dir /data --passphrase-file /tmp/kin-passphrase \
    --new-passphrase-file /tmp/new.pass 2>&1) || { fail "kin passwd falhou: $out"; return; }
  check "passwd diz que trocou as três chaves" contains "$out" "senha trocada em: identity.key, device.key, db.key"
  after=$(compose exec -T a sh -c 'cksum /data/identity.key /data/device.key /data/db.key')
  check "os três arquivos mudaram no disco (6 linhas distintas antes e depois)" \
    [ "$(printf '%s\n%s\n' "$before" "$after" | sort | uniq -u | wc -l)" = 6 ]
  check "continuam cifrados (chaves e bancos)" db_is_encrypted a
  # Retomada: repetir o mesmo comando não acha nada para trocar (o que uma queda no meio deixaria pela metade).
  out2=$(compose exec -T a kin passwd --data-dir /data --passphrase-file /tmp/kin-passphrase \
    --new-passphrase-file /tmp/new.pass 2>&1) || { fail "kin passwd (repetido) falhou: $out2"; return; }
  check "repetir o comando não troca nada" contains "$out2" "nada a trocar"

  # Reinicia o `a` na senha nova: a identidade e o estado MLS (db.key) sobrevivem, e a conversa continua.
  n=$(count_log a "reserva aceita no relay")
  compose exec -T a sh -c "echo '--data-dir /data --no-mdns --relay $RELAY_ADDR --open-admission --passphrase-file /tmp/new.pass' > /tmp/kin-args"
  compose stop a >/dev/null
  compose start a >/dev/null
  wait_log_n a "reserva aceita no relay" $((n + 1)) 30 \
    || { fail "a não subiu com a senha nova"; logs a | tail -20; return; }
  say b "depois do passwd"
  check "a conversa sobreviveu: a mensagem nova chega ao a" wait_log a "peer: depois do passwd" 90

  # A senha velha não abre mais: sem o --passphrase-file do args, o node.sh volta à senha do lab.
  compose exec -T a sh -c "echo '--data-dir /data --no-mdns --relay $RELAY_ADDR --open-admission' > /tmp/kin-args"
  compose stop a >/dev/null
  compose start a >/dev/null
  check "com a senha velha o a recusa subir" wait_log a "Error: WrongPassphraseOrCorrupt" 20
}

run_scenario() {
  case "$1" in
    passwd) scenario_passwd ;;
    cone) scenario_cone ;;
    symmetric) scenario_symmetric ;;
    lossy) scenario_lossy ;;
    cone-quic) scenario_cone_quic ;;
    symmetric-quic) scenario_symmetric_quic ;;
    relay-down) scenario_relay_down ;;
    invite) scenario_invite ;;
    invite-single-use) scenario_invite_single_use ;;
    invite-revoked) scenario_invite_revoked ;;
    invite-approved) scenario_invite_approved ;;
    invite-denied) scenario_invite_denied ;;
    relay-failover) scenario_relay_failover ;;
    multi-device) scenario_multi_device ;;
    multi-device-plain) scenario_multi_device_plain ;;
    history-sync) scenario_history_sync ;;
    paired-device) scenario_paired_device ;;
    revoke-device) scenario_revoke_device ;;
    reconnect) scenario_reconnect ;;
    reconnect-restart) scenario_reconnect_restart ;;
    encrypted-db) scenario_encrypted_db ;;
    protect-migration) scenario_protect_migration ;;
    stale-connection-rst) scenario_stale_connection_rst ;;
    stale-connection-silent) scenario_stale_connection_silent ;;
    *) echo "cenário desconhecido: $1" >&2; exit 2 ;;
  esac
}

SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -eq 0 ] && SCENARIOS=(cone symmetric cone-quic symmetric-quic lossy relay-down invite invite-single-use invite-revoked invite-approved invite-denied relay-failover multi-device multi-device-plain history-sync paired-device revoke-device reconnect reconnect-restart encrypted-db protect-migration passwd)

# REPEAT=N roda cada cenário N vezes e resume x/N (o hole punching varia de rodada para rodada).
REPEAT=${REPEAT:-1}
if [ "$REPEAT" -gt 1 ]; then
  SUMMARY=()
  for s in "${SCENARIOS[@]}"; do
    ok=0
    for i in $(seq "$REPEAT"); do
      FAILED=0
      echo "##### $s: rodada $i de $REPEAT"
      run_scenario "$s"
      [ $FAILED -eq 0 ] && ok=$((ok + 1))
    done
    SUMMARY+=("$s: $ok/$REPEAT")
  done
  echo "== Resumo (REPEAT=$REPEAT)"
  printf '  %s\n' "${SUMMARY[@]}"
  for line in "${SUMMARY[@]}"; do
    [ "${line##*: }" = "$REPEAT/$REPEAT" ] || exit 1
  done
  exit 0
fi

for s in "${SCENARIOS[@]}"; do
  run_scenario "$s"
done

[ $FAILED -eq 0 ] && echo "TUDO OK" || echo "HÁ FALHAS"
exit $FAILED
