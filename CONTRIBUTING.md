# Contributing

Contributions are welcome — bug reports, fixes, features, docs.

## Licence and CLA

Velstra Cloud is **AGPL-3.0-or-later** ([`LICENSE`](LICENSE)). One file is not:
`velstra-cloud-fabric/proto/vendor/velstra.proto` is vendored from the fabric
repository under MIT OR Apache-2.0, and vendoring does not relicense it.

To keep **dual-licensing** possible — so the project can fund itself by offering
commercial terms to organisations that cannot accept the AGPL — contributors
agree to a **Contributor License Agreement** before their first contribution is
merged. The CLA:

- lets you keep the copyright to your contribution, **and**
- grants the maintainer the right to license it under the AGPL *and* under
  separate commercial terms.

Without it the project could never be relicensed: that would need every past
contributor's permission, and one unreachable contributor is enough to make it
impossible for ever. The same approach Qt and Grafana use.

This has to exist *before* the first outside pull request, not after. A CLA
introduced later applies to nothing already merged.

> Enforced with [CLA Assistant](https://github.com/cla-assistant/cla-assistant).
> Set it up on the repository before accepting external pull requests.

## Working on it

```sh
cargo test --workspace              # 1180 tests, no root, no network
cargo clippy --workspace --all-targets
cargo +nightly fmt --all            # nightly: rustfmt.toml uses unstable options
velstra-cloud-console/tests/console/run.sh    # the browser suite, in real Chrome
```

**Run `cargo +nightly fmt` before you push.** CI checks it with a nightly
toolchain because `rustfmt.toml` sets `group_imports` and `imports_granularity`,
which stable rustfmt silently ignores — so a stable `cargo fmt --check` is a gate
that passes whatever the imports look like.

### The Nix checks

```sh
nix build .#checks.x86_64-linux.single-node -L    # one box that is a whole cell
nix flake show                                    # all twelve
```

Four need only a CPU (`deb`, `setup`, `console`, `dev-smoke`) and run on every
pull request. Eight boot a real machine and run nightly. Every one of them must
be named by a lane in `.github/workflows/ci.yml` — a guard enforces that,
because a lane that lists eight names and runs eight names is green.

Working on cloud and sentinel at once:

```sh
nix build .#checks.x86_64-linux.guest --override-input sentinel path:../sentinel
```

### The SRv6 end-to-end case

`velstra-cloud-nodeagent/tests/fabric_datapath.rs` fails — deliberately — when
the fabric controller is not built, because that case would otherwise skip and a
skipped run looks exactly like one that proved SRv6 works. Either build it:

```sh
git clone https://github.com/Velstra/fabric ../fabric
cargo build -p velstra-controller --manifest-path ../fabric/Cargo.toml
```

or set `VELSTRA_FABRIC_OPTIONAL=1` to accept the gap on purpose.

## What the code is like

Two things are worth knowing before reading a diff:

**Comments say why, not what.** A comment that restates the line above it is
noise; one that records the failure a line exists to prevent is the most
valuable thing in the file. Several of this codebase's rules are written down
exactly once, in a comment, next to the code that keeps them.

**Every claim is executed.** The recurring bug in this project is a feature that
is modelled, unit-tested, and run by nobody — backups that were never written,
a data plane whose agent nothing started, a pool whose unreachable backend was
reported nowhere. A test that proves the shape of a command is worth less than
one that runs it, and a green check that could not have failed is worth nothing.
