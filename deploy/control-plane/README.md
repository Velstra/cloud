# Automated control-plane membership

`reconcile.sh` is the production deployment step for adding or replacing
Velstra control planes. The hardware provisioner installs the Velstra package,
writes `/etc/velstra/node.env` with the `control-plane` role and valid TLS
material, then runs the reconciler from an administration host.

```sh
cp deploy/control-plane/inventory.example.json /secure/velstra-cell.json
$EDITOR /secure/velstra-cell.json
deploy/control-plane/reconcile.sh /secure/velstra-cell.json
```

The inventory is the complete desired etcd voting set. It must contain an odd
number of at least three control planes. The first entry is an existing healthy
member used to coordinate admission. `address` is the private IPv4 address or
DNS name used by etcd and Cloud; `sshAddress` is the IPv4 address or DNS name
the deployment runner uses.
Keep the real inventory outside the repository because it names infrastructure
and references a private key. `VELSTRA_DEPLOY_SSH_KEY` and
`VELSTRA_DEPLOY_SSH_USER` override the SSH fields for a secret manager or CI.

etcd uses mutual TLS by default. The inventory supplies one CA and a distinct
certificate/key pair for each member; the certificates need both server and
client usage and IP/DNS SANs matching `address`. The reconciler verifies each
chain and key pair before touching membership. An isolated development lab can
set `"etcd": {"scheme":"http","allowPlaintext":true}` explicitly. There is
no implicit plaintext fallback.

Every run:

1. verifies every target already has a control-plane seed and required tools;
2. refuses an inventory that silently omits a running etcd member;
3. saves an etcd snapshot before changing membership;
4. admits missing members idempotently and writes the same complete cluster
   manifest to every control plane;
5. removes any member admitted by this run if its etcd or API startup fails;
6. publishes all client endpoints into every control-plane seed and rolls API
   and controller replicas one at a time;
7. requires every etcd endpoint and every API/controller replica to be ready.

Existing members are never removed merely because the inventory changed. For a
planned replacement, add the new member and converge first. Remove the retired
member in a separate maintenance operation after workloads and leadership have
moved; this keeps a typo in deployment input from shrinking quorum.

The reconciler does not create or copy API private keys. Certificate issuance
and secret delivery belong to the hardware provisioner or secret manager and
must complete before admission. This also means a failed certificate delivery
cannot leave a new voting member whose API will never start.
