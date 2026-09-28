#!/bin/sh
# Keep the Cloud CLI (`velstra`) distinct from Fabric's executable of the same
# upstream name. Debian installs the latter at an explicit operator-owned path.
set -eu

agent=${VELSTRA_FABRIC_AGENT_BINARY:-/usr/local/libexec/velstra-fabric-agent}
case "$agent" in
  /*) ;;
  *) echo 'VELSTRA_FABRIC_AGENT_BINARY must be an absolute path' >&2; exit 1 ;;
esac
if [ -z "${VELSTRA_FABRIC_CONTROL:-}" ]; then
  echo 'no VELSTRA_FABRIC_CONTROL in the seed: this cell has no data plane' >&2
  exit 1
fi
if [ ! -x "$agent" ]; then
  echo "Fabric data-plane agent is not installed at $agent" >&2
  exit 1
fi
if [ "${1:-}" = --check ]; then
  exit 0
fi

set -- run --controller "$VELSTRA_FABRIC_CONTROL" --node-id "$VELSTRA_NODE"
case "$VELSTRA_FABRIC_CONTROL" in
  https://*)
    : "${VELSTRA_FABRIC_AGENT_CA:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_CA}" \
      "${VELSTRA_FABRIC_AGENT_CERT:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_CERT}" \
      "${VELSTRA_FABRIC_AGENT_KEY:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_KEY}"
    set -- "$@" --tls-ca "$VELSTRA_FABRIC_AGENT_CA" \
      --tls-cert "$VELSTRA_FABRIC_AGENT_CERT" --tls-key "$VELSTRA_FABRIC_AGENT_KEY"
    ;;
  http://*)
    if [ -n "${VELSTRA_FABRIC_AGENT_CA:-}${VELSTRA_FABRIC_AGENT_CERT:-}${VELSTRA_FABRIC_AGENT_KEY:-}" ]; then
      echo 'fabric client credentials require HTTPS' >&2
      exit 1
    fi
    ;;
  *) echo 'VELSTRA_FABRIC_CONTROL must use HTTP or HTTPS' >&2; exit 1 ;;
esac

exec "$agent" "$@"
