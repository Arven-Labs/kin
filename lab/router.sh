#!/bin/sh
# Roteador com NAT. NAT_MODE=cone: o MASQUERADE do Linux preserva a porta de origem (mapeamento
# independente do destino) e o conntrack só deixa entrar o que já foi iniciado de dentro, o caso que o
# hole punching resolve. NAT_MODE=symmetric: porta de origem aleatória por conexão, que o hole
# punching não vence (a conversa precisa continuar pelo relay).
set -eu

iface_for() { ip -o -4 addr show | awk -v p="$1" 'index($4, p) == 1 { print $2; exit }'; }
LAN_IF=$(iface_for "$LAN_PREFIX")
WAN_IF=$(iface_for "$WAN_PREFIX")

# Como um roteador doméstico: pacote de fora sem mapeamento é DESCARTADO em silêncio (nada de RST), e
# é isso que deixa o SYN reenviado passar quando o outro lado abre o mapeamento (hole punching).
iptables -P INPUT DROP
iptables -A INPUT -i lo -j ACCEPT
iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -P FORWARD DROP
iptables -A FORWARD -i "$LAN_IF" -o "$WAN_IF" -j ACCEPT
iptables -A FORWARD -i "$WAN_IF" -o "$LAN_IF" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

case "$NAT_MODE" in
  cone) iptables -t nat -A POSTROUTING -o "$WAN_IF" -j MASQUERADE ;;
  symmetric) iptables -t nat -A POSTROUTING -o "$WAN_IF" -j MASQUERADE --random-fully ;;
  *) echo "NAT_MODE inválido: $NAT_MODE" >&2; exit 1 ;;
esac

# NETEM (opcional): atraso, jitter e perda na saída para a rede pública, como um enlace ruim, ex.:
# NETEM="delay 80ms 20ms loss 3%". Cada roteador aplica na sua saída, então o caminho a↔b paga duas vezes.
# Se o kernel do host não tiver o sch_netem, o `tc` falha e o roteador não sobe (falha visível).
if [ -n "${NETEM:-}" ]; then
  # shellcheck disable=SC2086  # NETEM é uma lista de argumentos do tc, de propósito sem aspas
  tc qdisc add dev "$WAN_IF" root netem $NETEM
fi

echo "router pronto ($NAT_MODE): lan=$LAN_IF wan=$WAN_IF${NETEM:+ netem=\"$NETEM\"}"
exec sleep infinity
