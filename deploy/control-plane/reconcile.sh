#!/usr/bin/env bash
# Reconcile a production Velstra control-plane quorum from a declarative inventory.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
inventory=${1:-}
[[ -n "$inventory" ]] || { echo "usage: $0 INVENTORY.json" >&2; exit 2; }
[[ -s "$inventory" ]] || { echo "inventory does not exist: $inventory" >&2; exit 2; }

for tool in jq ssh scp openssl; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

controls=$(jq -er '.controlPlanes | length' "$inventory")
if (( controls < 3 || controls % 2 == 0 )); then
  echo "controlPlanes must contain the complete odd quorum of at least three members" >&2
  exit 1
fi
jq -e '
  (.cell | type == "string" and length > 0) and
  (.region | type == "string" and length > 0) and
  (all(.controlPlanes[];
    (.name | type == "string" and test("^[a-z0-9][a-z0-9-]*$")) and
    (.address | type == "string" and test("^[A-Za-z0-9._-]+$")) and
    (.sshAddress | type == "string" and test("^[A-Za-z0-9._-]+$")))) and
  ([.controlPlanes[].name] | length == (unique | length)) and
  ([.controlPlanes[].address] | length == (unique | length))
' "$inventory" >/dev/null || { echo "invalid or duplicate control-plane inventory" >&2; exit 1; }

ssh_user=${VELSTRA_DEPLOY_SSH_USER:-$(jq -r '.ssh.user // "root"' "$inventory")}
ssh_key=${VELSTRA_DEPLOY_SSH_KEY:-$(jq -r '.ssh.privateKey // empty' "$inventory")}
[[ -n "$ssh_key" && -s "$ssh_key" ]] || {
  echo "set ssh.privateKey or VELSTRA_DEPLOY_SSH_KEY to a readable private key" >&2
  exit 1
}
ssh_opts=(-F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=accept-new -i "$ssh_key")

field() {
  jq -er --arg name "$1" --arg field "$2" \
    '.controlPlanes[] | select(.name == $name) | .[$field]' "$inventory"
}
remote() {
  local host
  host=$(field "$1" sshAddress)
  shift
  ssh -n "${ssh_opts[@]}" "$ssh_user@$host" "$@"
}
remote_stdin() {
  local host
  host=$(field "$1" sshAddress)
  shift
  ssh "${ssh_opts[@]}" "$ssh_user@$host" "$@"
}
copy_to() {
  local host=$1 source=$2 target=$3
  host=$(field "$host" sshAddress)
  scp "${ssh_opts[@]}" "$source" "$ssh_user@$host:$target"
}
as_root() {
  if [[ "$ssh_user" == root ]]; then printf '%s' ''; else printf '%s' 'sudo '; fi
}
root=$(as_root)

leader=$(jq -r '.controlPlanes[0].name' "$inventory")
leader_address=$(field "$leader" address)
etcd_scheme=$(jq -r '.etcd.scheme // "https"' "$inventory")
case "$etcd_scheme" in
  https) ;;
  http)
    jq -e '.etcd.allowPlaintext == true' "$inventory" >/dev/null || {
      echo "plaintext etcd requires etcd.allowPlaintext=true; production defaults to mTLS" >&2
      exit 1
    }
    ;;
  *) echo "etcd.scheme must be https or http" >&2; exit 1 ;;
esac
store_endpoints=$(jq -r --arg scheme "$etcd_scheme" '[.controlPlanes[] | $scheme + "://" + .address + ":2379"] | join(",")' "$inventory")
cluster=$(jq -r --arg scheme "$etcd_scheme" '[.controlPlanes[] | .name + "=" + $scheme + "://" + .address + ":2380"] | join(",")' "$inventory")
etcdctl="ETCDCTL_API=3"
if [[ "$etcd_scheme" == https ]]; then
  etcd_ca=$(jq -er '.etcd.ca' "$inventory")
  [[ -s "$etcd_ca" ]] || { echo "missing etcd CA: $etcd_ca" >&2; exit 1; }
  etcdctl+=' ETCDCTL_CACERT=/etc/velstra/etcd/ca.pem ETCDCTL_CERT=/etc/velstra/etcd/cert.pem ETCDCTL_KEY=/etc/velstra/etcd/key.pem'
fi

declare -A added_members=()
rollback_added_members() {
  local name id
  for name in "${!added_members[@]}"; do
    id=${added_members[$name]}
    echo "rolling back etcd member $name ($id)" >&2
    remote "$leader" "$etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 member remove '$id'" >/dev/null 2>&1 || true
    remote "$name" "${root}systemctl stop etcd" >/dev/null 2>&1 || true
  done
}
trap rollback_added_members ERR INT TERM

echo "== preflight =="
while IFS= read -r node; do
  remote "$node" "command -v etcdctl >/dev/null && command -v curl >/dev/null && test -s /etc/velstra/node.env && grep -Eq '^VELSTRA_ROLES=.*control-plane' /etc/velstra/node.env"
done < <(jq -r '.controlPlanes[].name' "$inventory")

if [[ "$etcd_scheme" == https ]]; then
  echo "== install etcd mTLS identities =="
  while IFS=$'\t' read -r node node_cert node_key; do
    [[ -s "$node_cert" ]] || { echo "missing etcd certificate for $node: $node_cert" >&2; exit 1; }
    [[ -s "$node_key" ]] || { echo "missing etcd private key for $node: $node_key" >&2; exit 1; }
    openssl verify -purpose sslserver -CAfile "$etcd_ca" "$node_cert" >/dev/null
    openssl verify -purpose sslclient -CAfile "$etcd_ca" "$node_cert" >/dev/null
    cert_pub=$(openssl x509 -in "$node_cert" -pubkey -noout | openssl pkey -pubin -outform der | sha256sum | cut -d' ' -f1)
    key_pub=$(openssl pkey -in "$node_key" -pubout -outform der | sha256sum | cut -d' ' -f1)
    [[ "$cert_pub" == "$key_pub" ]] || { echo "etcd certificate and key do not match for $node" >&2; exit 1; }
    copy_to "$node" "$etcd_ca" /tmp/velstra-etcd-ca.pem
    copy_to "$node" "$node_cert" /tmp/velstra-etcd-cert.pem
    copy_to "$node" "$node_key" /tmp/velstra-etcd-key.pem
    remote "$node" "${root}install -d -o root -g root -m 0700 /etc/velstra/etcd; ${root}install -o root -g root -m 0644 /tmp/velstra-etcd-ca.pem /etc/velstra/etcd/ca.pem; ${root}install -o root -g root -m 0644 /tmp/velstra-etcd-cert.pem /etc/velstra/etcd/cert.pem; ${root}install -o root -g root -m 0600 /tmp/velstra-etcd-key.pem /etc/velstra/etcd/key.pem; rm -f /tmp/velstra-etcd-{ca,cert,key}.pem"
  done < <(jq -r '.controlPlanes[] | [.name,.etcdCertificate,.etcdPrivateKey] | @tsv' "$inventory")
fi

members=$(remote "$leader" "$etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 member list -w json")
while IFS=$'\t' read -r member_name peer; do
  [[ -n "$peer" ]] || continue
  expected=$(jq -r --arg peer "$peer" --arg scheme "$etcd_scheme" '.controlPlanes[] | select(($scheme + "://" + .address + ":2380") == $peer) | .name' "$inventory")
  if [[ -z "$expected" ]]; then
    echo "running etcd member ${member_name:-<unstarted>} at $peer is absent from inventory; refusing an implicit removal" >&2
    exit 1
  fi
  if [[ -n "$member_name" && "$member_name" != "$expected" ]]; then
    echo "etcd peer $peer is named $member_name, inventory calls it $expected" >&2
    exit 1
  fi
done < <(jq -r '.members[] | [.name, .peerURLs[0]] | @tsv' <<<"$members")

echo "== snapshot before membership changes =="
remote "$leader" "${root}mkdir -p /var/lib/velstra/store-backups; snap=/tmp/pre-control-plane-join-\$(date +%s%N).db; $etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 snapshot save \"\$snap\" >/dev/null; ${root}mv \"\$snap\" /var/lib/velstra/store-backups/"

echo "== reconcile membership and local etcd configuration =="
while IFS=$'\t' read -r name address; do
  peer="$etcd_scheme://$address:2380"
  members=$(remote "$leader" "$etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 member list -w json")
  existing_id=$(jq -r --arg peer "$peer" '.members[]? | select(.peerURLs | index($peer)) | .ID // empty' <<<"$members")
  if [[ -z "$existing_id" ]]; then
    remote "$leader" "$etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 member add '$name' --peer-urls='$peer' -w json" >/dev/null
    # JSON represents the uint64 member ID as a decimal number, while
    # `member remove` accepts the hexadecimal spelling printed by the normal
    # member list. Keep that exact spelling so rollback really can remove a
    # half-admitted voter.
    added_members[$name]=$(remote "$leader" "$etcdctl etcdctl --endpoints=$etcd_scheme://$leader_address:2379 member list | awk -F, -v peer='$peer' '\$4 ~ peer { gsub(/ /, \"\", \$1); print \$1 }'")
    [[ -n "${added_members[$name]}" ]]
  fi

  env_file=$(mktemp)
  cat >"$env_file" <<EOF
ETCD_NAME=$name
ETCD_DATA_DIR=/var/lib/etcd/default
ETCD_LISTEN_PEER_URLS=$peer
ETCD_INITIAL_ADVERTISE_PEER_URLS=$peer
ETCD_LISTEN_CLIENT_URLS=$etcd_scheme://127.0.0.1:2379,$etcd_scheme://$address:2379
ETCD_ADVERTISE_CLIENT_URLS=$etcd_scheme://$address:2379
ETCD_INITIAL_CLUSTER=$cluster
ETCD_INITIAL_CLUSTER_STATE=existing
ETCD_INITIAL_CLUSTER_TOKEN=velstra-cloud
ETCD_AUTO_COMPACTION_MODE=periodic
ETCD_AUTO_COMPACTION_RETENTION=1h
ETCD_QUOTA_BACKEND_BYTES=8589934592
EOF
  if [[ "$etcd_scheme" == https ]]; then
    cat >>"$env_file" <<'EOF'
ETCD_CERT_FILE=/etc/velstra/etcd/cert.pem
ETCD_KEY_FILE=/etc/velstra/etcd/key.pem
ETCD_TRUSTED_CA_FILE=/etc/velstra/etcd/ca.pem
ETCD_CLIENT_CERT_AUTH=true
ETCD_PEER_CERT_FILE=/etc/velstra/etcd/cert.pem
ETCD_PEER_KEY_FILE=/etc/velstra/etcd/key.pem
ETCD_PEER_TRUSTED_CA_FILE=/etc/velstra/etcd/ca.pem
ETCD_PEER_CLIENT_CERT_AUTH=true
EOF
  fi
  copy_to "$name" "$env_file" /tmp/velstra-etcd.env
  rm -f "$env_file"
  remote "$name" "${root}install -o root -g root -m 0644 /tmp/velstra-etcd.env /etc/default/etcd; rm -f /tmp/velstra-etcd.env; ${root}systemctl enable --now etcd; for i in \$(seq 1 30); do $etcdctl etcdctl --endpoints=$etcd_scheme://$address:2379 endpoint health >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1"
done < <(jq -r '.controlPlanes[] | [.name,.address] | @tsv' "$inventory")

echo "== publish all client endpoints and roll control-plane services =="
while IFS= read -r name; do
  remote_stdin "$name" "${root}bash -s" <<REMOTE
set -euo pipefail
seed=/etc/velstra/node.env
tmp=\$(mktemp)
awk '!/^VELSTRA_STORE=/ && !/^VELSTRA_STORE_CA=/ && !/^VELSTRA_STORE_CERT=/ && !/^VELSTRA_STORE_KEY=/' "\$seed" >"\$tmp"
printf '%s\n' 'VELSTRA_STORE=$store_endpoints' >>"\$tmp"
$(if [[ "$etcd_scheme" == https ]]; then cat <<'EOF'
printf '%s\n' 'VELSTRA_STORE_CA=/etc/velstra/etcd/ca.pem' >>"$tmp"
printf '%s\n' 'VELSTRA_STORE_CERT=/etc/velstra/etcd/cert.pem' >>"$tmp"
printf '%s\n' 'VELSTRA_STORE_KEY=/etc/velstra/etcd/key.pem' >>"$tmp"
EOF
fi)
install -o root -g root -m 0644 "\$tmp" "\$seed"
rm -f "\$tmp"
systemctl enable velstra-cloud-api velstra-cloud-controller
systemctl restart velstra-cloud-api
for i in \$(seq 1 30); do curl -kfsS https://localhost:8443/readyz >/dev/null 2>&1 && break; sleep 1; done
curl -kfsS https://localhost:8443/readyz >/dev/null
systemctl restart velstra-cloud-controller
REMOTE
done < <(jq -r '.controlPlanes[].name' "$inventory")

echo "== verify quorum and replicas =="
health_endpoints=$store_endpoints
remote "$leader" "$etcdctl etcdctl --endpoints=$health_endpoints endpoint health"
while IFS=$'\t' read -r name address; do
  remote "$name" "curl -kfsS https://localhost:8443/readyz >/dev/null; systemctl is-active --quiet velstra-cloud-controller"
  echo "$name ($address) is ready"
done < <(jq -r '.controlPlanes[] | [.name,.address] | @tsv' "$inventory")

added_members=()
trap - ERR INT TERM
echo "control-plane cluster converged"
