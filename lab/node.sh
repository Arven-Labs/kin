#!/bin/sh
# Entrada dos containers `kin` (relay, a, b). Se houver GATEWAY, sai pela rota do roteador de NAT.
# O stdin do kin é uma fifo (/tmp/in): o run.sh "digita" mensagens com `echo ... > /tmp/in`.
set -eu

[ -n "${GATEWAY:-}" ] && ip route replace default via "$GATEWAY"
rm -f /tmp/in
mkfifo /tmp/in
# O laboratório mede só a rede P2P: sem a caixa postal Nostr (que, por padrão, falaria com relays públicos).
# shellcheck disable=SC2086
exec kin --no-nostr $KIN_ARGS <> /tmp/in
