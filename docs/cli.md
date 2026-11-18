# Use the Velstra CLI

The `velstra` binary ships in the Cloud package. It can also be built from the repository with `nix build .#velstra-cloud` and run as `./result/bin/velstra`. The examples below assume it is on your PATH.

## Connect

Set the URL of your API and use a token with the required project or cell permissions. For the [development cell](quickstart.md), the default URL is `http://127.0.0.1:8080`. For an installed cell, use its advertised TLS endpoint and trust its CA.

```bash
export VELSTRA_API=http://127.0.0.1:8080
export VELSTRA_PROJECT=p1
read -rs -p 'Bearer token: ' VELSTRA_TOKEN
printf '\n'
export VELSTRA_TOKEN
velstra whoami
```

A successful response identifies your session. Use `--api`, `--project` or `--token` to override an environment setting for one command.

## Read and explain

```sh
velstra collections
velstra get instances
velstra get instances web-1 --json
velstra explain instances too-big placement
```

The list shows requested and observed state. The last command requires a cell operator token and returns the candidate or per-node rejections. A label query such as `velstra get instances --labels env=prod` may legitimately return no rows.

## Submit a change to your development cell

The seed includes pool `rbd-standard`. Create a separate test volume in project `p1`:

```sh
velstra create volumes scratch --set sizeGib=4 --set pool=rbd-standard \
  --idempotency-key=docs-volume-scratch
velstra get volumes scratch --json
```

The create returns an operation and target. The volume's status changes as the pool agent reconciles it. Reuse the same idempotency key for a retry of the same request. To clean up this example, run `velstra delete volumes scratch` and confirm the named resource.

## Use from scripts

`--set` accepts numbers, booleans and JSON as well as strings. Quote array values to protect them from shell expansion. Use `--set-file key=path` for multiline content. Collections such as nodes are cell-scoped. Instances and volumes require a project.

[REST examples](api-guide.md) · [All fields and methods](rest-contract.md)
