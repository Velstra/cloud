# Control-plane membership

Adds/replaces Debian/Ubuntu systemd control-plane members from a desired
inventory. This is not the NixOS deployment path.

Prepare at least three members, an odd voting set, valid mTLS certificates,
verified SSH host keys and an existing healthy coordinator.
Joining etcd data directories must be empty; the reconciler never erases them.

From the repository root:

```sh
cp -i deploy/control-plane/inventory.example.json "$HOME/velstra-cell.json"
chmod 600 "$HOME/velstra-cell.json"
"$EDITOR" "$HOME/velstra-cell.json"
deploy/control-plane/reconcile.sh "$HOME/velstra-cell.json"
```

Keep the populated inventory and credentials outside the checkout.
Review the [membership runbook](../../docs/control-plane-membership.md) before
running: this changes etcd membership and rolls services.

The reconciler snapshots etcd, admits learners, checks readiness and preserves
existing members. Removal of retired members is a separate maintenance step.
[Example inventory](inventory.example.json) · [Installation](../../docs/install.md)
