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
: "${VELSTRA_FABRIC_UNDERLAY:?Fabric requires the underlay interface for tunnel decapsulation}"
if [ ! -x "$agent" ]; then
  echo "Fabric data-plane agent is not installed at $agent" >&2
  exit 1
fi
check_mode=${1:-}
set -- run --node-id "$VELSTRA_NODE" --iface "$VELSTRA_FABRIC_UNDERLAY"
remaining=$VELSTRA_FABRIC_CONTROL
transport=
while :; do
  case "$remaining" in
    *,*) endpoint=${remaining%%,*}; remaining=${remaining#*,} ;;
    *) endpoint=$remaining; remaining= ;;
  esac
  case "$endpoint" in
    *@*) echo 'fabric config service URLs must not contain embedded credentials' >&2; exit 1 ;;
  esac
  case "$endpoint" in
    https://?*) scheme=https ;;
    http://?*) scheme=http ;;
    *) echo 'VELSTRA_FABRIC_CONTROL must contain HTTP or HTTPS URLs with hosts' >&2; exit 1 ;;
  esac
  if [ -n "$transport" ] && [ "$scheme" != "$transport" ]; then
    echo 'fabric config service URLs must use the same scheme' >&2
    exit 1
  fi
  transport=$scheme
  set -- "$@" --controller "$endpoint"
  [ -n "$remaining" ] || break
done
case "$VELSTRA_FABRIC_CONTROL" in
  *,) echo 'VELSTRA_FABRIC_CONTROL has an empty final URL' >&2; exit 1 ;;
esac
case "$transport" in
  https)
    : "${VELSTRA_FABRIC_AGENT_CA:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_CA}" \
      "${VELSTRA_FABRIC_AGENT_CERT:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_CERT}" \
      "${VELSTRA_FABRIC_AGENT_KEY:?HTTPS fabric requires VELSTRA_FABRIC_AGENT_KEY}"
    set -- "$@" --tls-ca "$VELSTRA_FABRIC_AGENT_CA" \
      --tls-cert "$VELSTRA_FABRIC_AGENT_CERT" --tls-key "$VELSTRA_FABRIC_AGENT_KEY"
    ;;
  http)
    if [ -n "${VELSTRA_FABRIC_AGENT_CA:-}${VELSTRA_FABRIC_AGENT_CERT:-}${VELSTRA_FABRIC_AGENT_KEY:-}" ]; then
      echo 'fabric client credentials require HTTPS' >&2
      exit 1
    fi
    ;;
esac

if [ "$check_mode" = --check ]; then
  exit 0
fi

exec "$agent" "$@"
