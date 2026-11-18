# Velstra Cloud

<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/brand/logo-on-dark.svg">
  <img src=".github/brand/logo.svg" alt="Velstra Cloud" height="72">
</picture>

Velstra Cloud is a self-hosted IaaS control plane for tenant VMs, volumes and networks. The API separates requested configuration from observed state. When an instance cannot be placed, a cell operator can query the scheduler's per-host rejection reasons directly.

## Try the scheduler

On x86-64 Linux with Nix flakes enabled:

```sh
git clone https://github.com/Velstra/cloud.git
cd cloud
nix run .#dev
```

Open the printed console URL and use the printed development token. In project **p1**, compare **web-1** with **too-big**. The second guest asks for 64 vCPUs and 256 GiB of memory, more than either demo host can supply. The development cell runs the real API and controllers with an in-memory store, fake hypervisors and simulated storage. Stopping it discards the session.

[Follow the quickstart](https://velstra.github.io/cloud/quickstart.html) · [Inspect a placement refusal](https://velstra.github.io/cloud/operating.html#vm-placement)

## Run a cell

| Goal | Guide |
| --- | --- |
| Install an appliance, Debian node or NixOS module | [Installation](https://velstra.github.io/cloud/install.html) |
| Add compute, storage and Fabric networking | [Set up a cell](https://velstra.github.io/cloud/setup-guide.html) |
| Work from a terminal or integrate an API client | [CLI](https://velstra.github.io/cloud/cli.html) · [REST recipes](https://velstra.github.io/cloud/api-guide.html) |
| Evaluate a change, move a guest or recover a node | [Operations](https://velstra.github.io/cloud/operating.html) |

The project is under development. Check the release and exercise the chosen hypervisor, storage and migration path against your own hardware before production use. [Documentation](https://velstra.github.io/cloud/) · [Releases](https://github.com/Velstra/cloud/releases)

## Develop

The Rust workspace separates the resource model and scheduling decisions (`velstra-cloud-model`), persistence (`velstra-cloud-store`), transport (`velstra-cloud-api`), controllers, node agents and console. [Design](DESIGN.md) · [Contribution and test instructions](CONTRIBUTING.md) · [Testing](docs/testing.md) · [Console development](velstra-cloud-console-react/README.md) · [Security reporting](SECURITY.md)

Velstra Cloud is [AGPL-3.0-or-later](LICENSE). The vendored Fabric protocol retains its [upstream terms](velstra-cloud-fabric/proto/vendor/README.md).
