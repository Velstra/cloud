# Velstra Cloud

IaaS control plane with compute, storage and networking management, a web console,
and QEMU/Cloud Hypervisor node agents. Networking uses
[Velstra Fabric](https://github.com/Velstra/fabric).

## Try it locally

Use x86_64 Linux with Nix flakes:

```sh
git clone https://github.com/Velstra/cloud.git
cd cloud
nix run .#dev
```

Open the URL printed by the process and sign in with its printed token.
This is an in-memory development cell with simulated nodes and guests.
It needs no KVM and does not provision real VMs. Stop with Ctrl-C.

For real nodes and persistent state, use the
[installation guide](docs/install.md), not the development cell.
Production qualification is not complete.

## Test and develop

With Rust installed, start with the model tests:

```sh
cargo test -p velstra-cloud-model --locked
```

[Testing instructions](docs/testing.md) cover the full workspace, required
etcd/Fabric fixtures, browser tests and actual guest-boot checks.
For the frontend alone, see [console development](velstra-cloud-console-react/README.md).

## Build artifacts

```sh
nix build .#deb
nix build .#node-image
nix build .#node-iso
```

These are build outputs, not a production release certificate.
[Install and register nodes](docs/install.md) before using them in a cell.

## Documentation

[Quickstart](docs/quickstart.md) ·
[Setup](docs/setup-guide.md) ·
[CLI](docs/cli.md) ·
[REST API](docs/rest-contract.md) ·
[Operations](docs/operating.md) ·
[Upgrades](docs/upgrading.md)

## License

[AGPL-3.0-or-later](LICENSE). The vendored Fabric protobuf contract retains its
[MIT OR Apache-2.0 license](velstra-cloud-fabric/proto/vendor/README.md).
