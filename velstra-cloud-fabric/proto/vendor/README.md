# fabric's service contract, vendored

`velstra.proto` is a copy of `velstra-proto/proto/velstra.proto` from the
**fabric** repository (github.com/Velstra/fabric). It is here because this agent
is a *client* of fabric's orchestrator, and a gRPC client needs the schema, not
the crate.

## Its licence is not this repository's

Everything else here is AGPL-3.0-or-later. This file is not: it comes from
fabric's `velstra-proto`, which is **MIT OR Apache-2.0**, and vendoring a file
does not relicense it.

The distinction is deliberate on fabric's side rather than incidental — the wire
contract is linked into both the GPL eBPF object and the AGPL control plane, so
it has to be compatible with both, and it is meant to be reusable by anybody
writing a client. See `LICENSING.md` in the fabric repository.

Recorded here because a reader who found this file inside an AGPL repository
would reasonably assume the repository's terms, and be wrong in the direction
that makes them ask permission they do not need.

## Why a copy rather than a dependency

The two repositories are built and released independently — fabric is public and
this one is not — so a path dependency works on one laptop and breaks both CIs,
and a git dependency ties this repo's build to a revision of a repo it does not
control. Vendoring the schema is what everyone else does with somebody else's
gRPC contract, and it has one honest cost: the copy can fall behind.

## The cost is checked, not hoped for

`velstra.proto.sha256` records the digest of the file this copy was taken from.
`tests/fabric_proto.rs` re-checks it, and — when the fabric repository happens to
be checked out beside this one — compares the two files directly. It **skips**
rather than fails when fabric is not there, because a test that goes red on a
machine that simply does not have the other repo is a test people learn to
ignore.

Refreshing it is one command and the digest below moves with it:

    cp ../fabric/velstra-proto/proto/velstra.proto vendor/velstra.proto
    sha256sum vendor/velstra.proto | cut -d' ' -f1 > vendor/velstra.proto.sha256
