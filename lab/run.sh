#!/usr/bin/env bash
# Laboratório de NAT (Fase 2): sobe relay + dois roteadores com NAT + dois peers e confere o que a
# escada de fallback faz. Uso: `cargo build -p kin-cli && lab/run.sh [cone|symmetric|relay-down|invite|relay-failover|multi-device|multi-device-plain|history-sync]...`
# (sem argumentos roda todos). Limpa containers e redes ao final.
set -uo pipefail
cd "$(dirname "$0")"

BIN=../target/debug/kin
[ -x "$BIN" ] || { echo "compile antes: cargo build -p kin-cli" >&2; exit 2; }

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
  wait_log relay "relay em /ip4/172.30.0.10/tcp/4001/p2p/" 30 || { echo "relay não subiu"; return 1; }
  RELAY_ADDR=$(first relay '/ip4/172.30.0.10/tcp/4001/p2p/[A-Za-z0-9]+')
  if [ "${1:-}" = relay2 ]; then
    wait_log relay2 "relay em /ip4/172.30.0.11/tcp/4001/p2p/" 30 || { echo "relay2 não subiu"; return 1; }
    RELAY2_ADDR=$(first relay2 '/ip4/172.30.0.11/tcp/4001/p2p/[A-Za-z0-9]+')
  fi
}

# Sobe a topologia com o NAT pedido e deixa A e B com a conversa pronta (pelo relay).
# Deixa em variáveis: RELAY_ADDR, A_CIRCUIT.
setup() {
  infra "$1" || return 1

  export A_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR"
  compose up -d a >/dev/null || return 1
  wait_log a "reserva aceita no relay" 30 || { echo "a não conseguiu reserva no relay"; logs a; return 1; }
  A_CIRCUIT=$(first a '/ip4/172.30.0.10/tcp/4001/p2p/[A-Za-z0-9]+/p2p-circuit/p2p/[A-Za-z0-9]+')

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR --dial $A_CIRCUIT"
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
  wait_log a "convite: kin://invite/" 15 || { fail "a não gerou o convite"; logs a; return; }
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
  wait_log a "convite: kin://invite/" 15 || { fail "a não gerou o convite"; return; }
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
# O `/pair` da CLI recusa saída que não é terminal (o log do Docker é um pipe), então o segundo device
# recebe o `identity.key` de A1 por `compose cp` (é o que o `--pair` grava).
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

  export B_ARGS="--data-dir /data --no-mdns --relay $RELAY_ADDR ${KEY_ARGS:-}"
  compose up -d b >/dev/null
  wait_log b "reserva aceita no relay" 30 || { fail "c sem reserva no relay"; logs b; return; }
  C_ID=$(peer_id b)
  say b "/invite"
  wait_log b "convite: kin://invite/" 15 || { fail "c não gerou o convite"; logs b; return; }
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
  check "c -> X: a1 recebe" wait_log a "peer: oi de C" 20
  check "c -> X: a2 recebe" wait_log a2 "peer: oi de C" 20
}

# O mesmo cenário com as chaves em claro (`--no-passphrase`): cobre o caminho sem senha.
scenario_multi_device_plain() {
  KEY_ARGS="--no-passphrase" scenario_multi_device
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
  wait_log b "convite: kin://invite/" 15 || { fail "c não gerou o convite"; logs b; return; }
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

# run_scenario NOME: roda um cenário (FAILED fica 1 se algo falhou).
run_scenario() {
  case "$1" in
    cone) scenario_cone ;;
    symmetric) scenario_symmetric ;;
    relay-down) scenario_relay_down ;;
    invite) scenario_invite ;;
    relay-failover) scenario_relay_failover ;;
    multi-device) scenario_multi_device ;;
    multi-device-plain) scenario_multi_device_plain ;;
    history-sync) scenario_history_sync ;;
    *) echo "cenário desconhecido: $1" >&2; exit 2 ;;
  esac
}

SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -eq 0 ] && SCENARIOS=(cone symmetric relay-down invite relay-failover multi-device multi-device-plain history-sync)

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
