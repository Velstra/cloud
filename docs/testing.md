# Development tests

Run commands from the Cloud repository root. Source tests, browser contract
tests and actual guest-boot tests are separate gates.

## Model and workspace

The model tests need only Rust:

```sh
cargo test -p velstra-cloud-model --locked
```

The full workspace also needs `protoc`, `etcd` and the Fabric controller binary.
Check out [Fabric](https://github.com/Velstra/fabric) beside this checkout as
`../fabric`, then build its host-only controller:

```sh
cargo build -p velstra-controller --manifest-path ../fabric/Cargo.toml --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Formatting uses nightly rustfmt because this repository enables nightly-only
import grouping:

```sh
rustup toolchain install nightly --component rustfmt
cargo +nightly fmt --all -- --check
```

Do not set `VELSTRA_ETCD_OPTIONAL` or `VELSTRA_FABRIC_OPTIONAL` in a qualification
run. They allow missing fixtures locally, not evidence that those cases passed.

## API contract checks

```sh
cargo test -p velstra-cloud-fabric --test fabric_proto --locked -- --nocapture
cargo test -p velstra-cloud-api --test contract_routes --locked
cargo test -p velstra-cloud-api --test openapi --locked
```

The recorded protobuf digest always runs; comparing a sibling Fabric copy
requires that checkout. The OpenAPI test compares generated output with
[openapi.json](openapi.json); do not hand-edit the generated contract.

## Browser tests

Install Node.js and Chromium/Chrome; set `CHROMIUM` if the browser is not found.
These use disposable API fixtures, not real hypervisors or production nodes:

```sh
bash velstra-cloud-console/tests/console/run.sh
npm ci --prefix velstra-cloud-console-react
npm run lint --prefix velstra-cloud-console-react
npm run build --prefix velstra-cloud-console-react
npm run test:browser --prefix velstra-cloud-console-react
```

For interactive frontend work, see the
[React console README](../velstra-cloud-console-react/README.md).

## Real guests and Nix checks

Guest tests need writable `/dev/kvm`, QEMU, Cloud Hypervisor and guest fixtures:
`/tmp/vq/alpine.raw`, `/tmp/vq/boot/vmlinuz-virt` and
`/tmp/vq/boot/initramfs-virt`. `VELSTRA_TEST_IMAGE` and `VELSTRA_TEST_BOOT` select
other prepared fixture paths. The exact download/preparation sequence is in
the `guests` job of [CI](../.github/workflows/ci.yml).

```sh
cargo test -p velstra-cloud-nodeagent --locked \
  --test qemu_boots_a_guest --test cloud_hypervisor_boots_a_guest -- --nocapture
nix flake check --no-build
```

A missing hypervisor/guest may produce a visible skip locally. Such a skip is
not a guest-boot pass. `--no-build` evaluates definitions only; choose actual
VM checks from [flake.nix](../flake.nix) to execute them. The complete CI lane
definitions remain authoritative; a model or browser pass alone does not
certify a deployed cell.
