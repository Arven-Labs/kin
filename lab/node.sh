#!/bin/sh
# Entrada dos containers `kin` (relay, a, b). Se houver GATEWAY, sai pela rota do roteador de NAT.
# O stdin do kin é uma fifo (/tmp/in): o run.sh "digita" mensagens com `echo ... > /tmp/in`.
set -eu

[ -n "${GATEWAY:-}" ] && ip route replace default via "$GATEWAY"
rm -f /tmp/in
mkfifo /tmp/in
# As chaves ficam cifradas (ADR 0012). O stdin é uma fifo (sem terminal), então a senha vem de arquivo; a
# CLI recusa arquivo de senha legível por outros, e o git não guarda o modo 0600: copia-se com `install`.
# Um cenário que quer chaves em claro passa `--no-passphrase` (que ignora esta variável).
if [ -r /lab/passphrase ]; then
  install -m 600 /lab/passphrase /tmp/kin-passphrase
  export KIN_PASSPHRASE_FILE=/tmp/kin-passphrase
fi
# O laboratório mede só a rede P2P: sem a caixa postal Nostr (que, por padrão, falaria com relays públicos).
# shellcheck disable=SC2086
exec kin --no-nostr $KIN_ARGS <> /tmp/in
