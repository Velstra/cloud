# Vendored Fabric contract

[velstra.proto](velstra.proto) comes from
[Velstra Fabric's protobuf crate](https://github.com/Velstra/fabric/tree/main/velstra-proto).
It retains **MIT OR Apache-2.0**, not Cloud's AGPL license.

From the Cloud repository root, refresh from a reviewed sibling Fabric checkout:

```sh
cp ../fabric/velstra-proto/proto/velstra.proto velstra-cloud-fabric/proto/vendor/velstra.proto
sha256sum velstra-cloud-fabric/proto/vendor/velstra.proto | cut -d ' ' -f 1 > velstra-cloud-fabric/proto/vendor/velstra.proto.sha256
cargo test -p velstra-cloud-fabric --test fabric_proto --locked -- --nocapture
```

The test always checks the recorded digest and also compares the upstream copy
when available. `VELSTRA_FABRIC_PROTO_SOURCE` selects another reviewed source
file; it does not disable the digest check.
