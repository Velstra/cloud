# Join a compute node

Use this guide when the cell already has a working control plane. To create the first host, follow [Installation](install.md).

## Choose a registration method

| Method | Use when | Identity check |
| --- | --- | --- |
| Per-node join token | You can transfer a credential to the target | The seed carries the API CA and the token issued for this node |
| Approval in the console | The installer can reach the control plane | An operator compares the installer fingerprint before approval |
| Node-specific install medium | A remote site can boot a prepared image | A one-time download contains that node’s join material |

## Join using a token

1. On the control plane create a node under **Nodes**, or issue a credential for an existing node. Copy its join token into a protected local channel. Treat the token and any media that carries it as credentials.
2. On the appliance installer choose **Join a cell** and supply that token. On a Debian machine with Cloud installed, use the setup command's join option:

   ```bash
   read -rs -p 'Node join token: ' JOIN_TOKEN
   printf '\n'
   sudo velstra-cloud-node setup --join "$JOIN_TOKEN"
   unset JOIN_TOKEN
   ```

3. Follow the role-specific service instructions printed by setup. Verify the host appears under the expected ID and reports capacity. A remote pool agent uses its own pool credential.

The API must advertise an address the new node can reach and that its certificate covers. Token-based provisioning can seed a disk before the network is available; the agent must reach the API to register.

## Approve an installer

Select the online enrollment option on the target. In **Nodes**, open the pending machine and compare its fingerprint with the physical or out-of-band console. Check the cell identity, then approve. An unapproved machine cannot schedule a tenant guest. A stale request may expire and be announced again.

## Prepare install media

The node’s **Install medium** action requires a ready release containing an installer ISO. It produces a one-time download for that node. Use a device writer that preserves the complete ISO and its appended join file. An unpack-and-copy tool can drop the join file. Protect and retire the medium according to your credential handling process.

## Verify

```sh
velstra get nodes
```

If a unit is skipped, inspect `systemctl status velstra-cloud-nodeagent` for the missing seed or role. On authentication failure, first check the advertised URL, trusted CA, node ID and credential type. Never paste the join bundle into a public diagnostic report.

## Replace a lost credential

Use **Join token** on the existing node or the matching pool's credential action. The API supports `POST /api/v1/nodes/{id}:issueCredential` and the equivalent pools method. Issuing a token adds a credential; it preserves the one currently used by the agent. Deploy the new token, confirm a fresh heartbeat with it, then revoke the obsolete credential. Protect the join file: the embedded public CA is shareable, the registration secret is private.

For unattended configuration, `velstra-cloud-node setup --config FILE` reads the role-specific seed values. Deliver credentials separately and restrict file access. For an online installer, the comparison uses both a machine fingerprint and the cell's certificate identity; a mismatch requires investigation. Enrollment does not attest the machine's hardware.
