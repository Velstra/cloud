# Use the REST API

Every HTTP resource has a name under `/api/v1/`. Project-scoped resource names start with `projects/{project}/`. Use this guide for a first request; use the [REST contract](rest-contract.md) for wire semantics or download the [OpenAPI 3.1 specification](openapi.json) for a client generator.

## Make a read-only request

Start a [development cell](quickstart.md) and set `VELSTRA_API` and `VELSTRA_TOKEN` in the shell. The development token is a cell operator.

```sh
curl -fsS -H "Authorization: Bearer $VELSTRA_TOKEN" \
  "$VELSTRA_API/api/v1/projects/p1/instances/web-1" \
  | jq '{name: .meta.name, requested: .spec.desiredState, conditions: .status.conditions}'
```

A resource separates the desired configuration under `spec` from observations under `status`. Accepted writes can take more than one reconciliation pass to appear there.

## Ask a question without changing state

```sh
curl -fsS -H "Authorization: Bearer $VELSTRA_TOKEN" \
  "$VELSTRA_API/api/v1/projects/p1/instances/too-big:explainPlacement" \
  | jq '.rejected[] | {node, why, detail}'
```

The response names each host rejected for this request. Host-level placement details require the operator role. See [VM placement](operating.md#vm-placement).

## Choose a reference

| Need | Resource |
| --- | --- |
| Resource naming, lists, filters and paging | [Names and shapes](rest-contract.md#names-and-shapes) |
| Permission model and project boundaries | [Who may do what](rest-contract.md#who-may-do-what) |
| Conflict handling and idempotency | [Client rules](rest-contract.md#the-rules-a-client-may-rely-on) |
| Waiting for asynchronous creation or deletion | [Long-running operations](rest-contract.md#long-running-operations) |
| Diagnosing API errors | [Errors](rest-contract.md#errors) |
| Authentication for an integration | [Sessions and passwords](rest-contract.md#sessions-and-passwords) · [Service accounts](rest-contract.md#service-accounts) |
| Schemas for the running build | `GET /api/v1/openapi.json` |
