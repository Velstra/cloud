//! The REST surface as an OpenAPI 3.1 document, generated from what the API
//! actually serves.
//!
//! `docs/rest-contract.md` is the contract for people. This is the same
//! surface for programs: a client generator, a Terraform provider, an IDE that
//! completes a request body. It is **derived**, never written by hand — the
//! collections come from [`crate::COLLECTIONS`], the router's own list; the
//! field shapes come from the console's schema, which is what the console's
//! forms are drawn from; the custom methods are the ones `rest.rs` dispatches.
//! A collection the router serves is in the document because the router serves
//! it, and a field the console asks for is in the document because the console
//! asks for it. Two descriptions of one surface that could disagree would be
//! worse than one.
//!
//! What it does not know it says so: `status` is the agent's report and its
//! shape is the agent's, so it is an open object with the two fields every
//! status carries (`observedGeneration`, `conditions[]`). A collection the
//! console has no screen for gets a spec that is an open object, rather than a
//! guessed one.
//!
//! Served at `GET /api/v1/openapi.json` outside the bearer-token layer — it is
//! documentation, and the console page that embeds the same schema is served
//! without a token too — and checked into `docs/openapi.json` by a test that
//! fails when the two drift, so a change to the surface is a change to the
//! document in the same commit.

use serde_json::{json, Map, Value};
use velstra_cloud_console::{Check, Collection, Field, Kind, Scope};

/// The document, as JSON.
pub fn document() -> Value {
    let mut paths = Map::new();
    let mut schemas = Map::new();
    common_schemas(&mut schemas);
    for kind in crate::COLLECTIONS {
        let screen = velstra_cloud_console::COLLECTIONS
            .iter()
            .find(|c| c.id == kind);
        let schema = schema_name(kind, screen);
        schemas.insert(format!("{schema}Spec"), spec_schema(screen));
        schemas.insert(schema.clone(), resource_schema(&schema));
        collection_paths(&mut paths, kind, screen, &schema);
    }
    fixed_paths(&mut paths);
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Velstra Cloud",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "The JSON gateway over the same handlers gRPC serves. \
                A resource is addressed the way AIP addresses one — \
                `projects/{project}/instances/{instance}` — and the HTTP path is \
                that name under `/api/v1/`. Every resource body is `meta`, `spec` \
                and `status`: what was asked for, and what the node that owns it \
                reports. Only a controller writes spec; only the owning agent \
                writes status; `generation` and `observedGeneration` say whether \
                the second has caught up with the first. docs/rest-contract.md \
                is the contract in prose; this document is derived from the \
                router and the console's schema and cannot disagree with either.",
        },
        "servers": [{ "url": "/" }],
        "security": [{ "bearer": [] }],
        "components": {
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "A session token from `POST /api/v1/sessions`, a \
                        service-account token from `POST /api/v1/users/{id}/tokens`, \
                        or a node's own credential.",
                }
            },
            "schemas": schemas,
        },
        "paths": paths,
    })
}

/// The document, indented, with a trailing newline — the form it is checked in
/// as, so a diff is a diff of lines.
pub fn pretty() -> String {
    let mut text = serde_json::to_string_pretty(&document()).expect("the document is plain data");
    text.push('\n');
    text
}

// ---- collections -----------------------------------------------------------

/// `security-groups` → `SecurityGroup`; the console's singular when it has one
/// (`Floating IP` → `FloatingIP`), because that is the word an operator uses.
fn schema_name(kind: &str, screen: Option<&Collection>) -> String {
    let words: Vec<&str> = match screen {
        Some(c) => c.singular.split([' ', '-']).collect(),
        None => kind.split('-').collect(),
    };
    let mut out = String::new();
    for w in words {
        let mut chars = w.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.extend(chars);
        }
    }
    if screen.is_none() && out.ends_with('s') {
        out.pop();
    }
    out
}

fn resource_schema(spec: &str) -> Value {
    json!({
        "type": "object",
        "description": "What was asked for (`spec`), what is (`status`), and how \
            far the second is behind the first (`meta.generation` against \
            `status.observedGeneration`).",
        "required": ["meta", "spec", "status"],
        "properties": {
            "meta": { "$ref": "#/components/schemas/Meta" },
            "spec": { "$ref": format!("#/components/schemas/{spec}Spec") },
            "status": { "$ref": "#/components/schemas/Status" },
        },
    })
}

fn spec_schema(screen: Option<&Collection>) -> Value {
    let Some(c) = screen else {
        return json!({
            "type": "object",
            "description": "The console has no form for this collection, so its \
                fields are not enumerated here; docs/rest-contract.md names them.",
            "additionalProperties": true,
        });
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    for f in c.fields {
        properties.insert(f.key.to_string(), field_schema(f));
        if f.required {
            required.push(Value::String(f.key.to_string()));
        }
    }
    let mut out = json!({
        "type": "object",
        "description": c.blurb,
        "properties": properties,
        "additionalProperties": true,
    });
    if !required.is_empty() {
        out["required"] = Value::Array(required);
    }
    out
}

fn field_schema(f: &Field) -> Value {
    let mut s = match f.kind {
        Kind::Text { check, .. } => checked_string(check),
        Kind::Lines { .. } => json!({ "type": "string" }),
        Kind::Number { unit, min, max, .. } => {
            let mut n = json!({ "type": "integer", "minimum": min, "maximum": max });
            if !unit.is_empty() {
                n["x-unit"] = json!(unit);
            }
            n
        }
        Kind::Switch => json!({ "type": "boolean" }),
        Kind::Moment { .. } => json!({
            "type": "integer",
            "description": "A moment, as milliseconds since the Unix epoch.",
        }),
        Kind::Choice { options } => json!({
            "type": "string",
            "enum": options.iter().map(|o| o.value).collect::<Vec<_>>(),
        }),
        Kind::Ref { collection, .. } => json!({
            "type": "string",
            "description": format!("The name of a `{collection}` object."),
        }),
        Kind::RefList { collection, .. } => json!({
            "type": "array",
            "items": { "type": "string", "description": format!("The name of a `{collection}` object.") },
        }),
        Kind::TextList { check, .. } => json!({ "type": "array", "items": checked_string(check) }),
        Kind::RuleList { .. } | Kind::GrantList | Kind::DiskList { .. } | Kind::ListenerList => {
            json!({ "type": "array", "items": { "type": "object", "additionalProperties": true } })
        }
        Kind::PoolList { .. } => json!({
            "type": "array",
            "items": { "type": "object", "additionalProperties": true },
        }),
    };
    let mut notes = Vec::new();
    if !f.help.is_empty() {
        notes.push(f.help.to_string());
    }
    if f.derived {
        notes.push("Derived: omitted, the API fills it in; stated, it must agree.".into());
    }
    if f.at_creation {
        notes.push("Settable at creation only.".into());
    }
    if !notes.is_empty() {
        let joined = notes.join(" ");
        match s.get("description") {
            Some(Value::String(d)) => s["description"] = json!(format!("{d} {joined}")),
            _ => s["description"] = json!(joined),
        }
    }
    if f.at_creation {
        s["x-at-creation"] = json!(true);
    }
    s
}

fn checked_string(check: Check) -> Value {
    let format = match check {
        Check::None | Check::Name => None,
        Check::Id => Some("id"),
        Check::Cidr => Some("cidr"),
        Check::Address => Some("ip"),
        Check::Mac => Some("mac"),
        Check::Digest => Some("sha256"),
        Check::Url => Some("uri"),
    };
    match format {
        Some(fmt) => json!({ "type": "string", "format": fmt }),
        None => json!({ "type": "string" }),
    }
}

/// Where a collection lives: under a project, or at the cell.
fn base_path(kind: &str, screen: Option<&Collection>) -> (String, Vec<Value>) {
    let project_scoped = match screen {
        Some(c) => c.scope == Scope::Project,
        // Without a screen the router's own rule applies: the cell's own
        // collections are the ones the contract lists as such.
        None => !matches!(
            kind,
            "projects"
                | "users"
                | "nodes"
                | "pools"
                | "ceph-clusters"
                | "flavors"
                | "bgp-peers"
                | "folders"
                | "roles"
                | "device-classes"
                | "image-sources"
                | "maintenance-windows"
                | "audit"
                | "operations"
        ),
    };
    if project_scoped {
        (
            format!("/api/v1/projects/{{project}}/{kind}"),
            vec![path_param("project", "The project the object belongs to.")],
        )
    } else {
        (format!("/api/v1/{kind}"), Vec::new())
    }
}

fn collection_paths(
    paths: &mut Map<String, Value>,
    kind: &str,
    screen: Option<&Collection>,
    schema: &str,
) {
    let (base, params) = base_path(kind, screen);
    let title = screen.map(|c| c.title).unwrap_or(kind);
    let (creatable, editable, deletable) = match screen {
        Some(c) => (c.creatable, c.editable, c.deletable),
        None => (true, true, true),
    };

    let mut list_params = params.clone();
    list_params.extend([
        query_param("pageSize", "integer", "How many to return; the response carries `nextPageToken` when there are more."),
        query_param("pageToken", "string", "Where the previous page ended."),
        query_param("labels", "string", "Only objects carrying these labels, as `key=value,key2=value2`."),
        query_param("watch", "boolean", "`true` streams changes as server-sent events instead of listing."),
        query_param("fromRevision", "string", "With `watch`: the revision a list reported, so nothing between the list and the watch is lost."),
    ]);
    let mut collection = Map::new();
    collection.insert("get".into(), json!({
        "tags": [title],
        "summary": format!("List {title}"),
        "operationId": format!("list-{kind}"),
        "parameters": list_params,
        "responses": {
            "200": {
                "description": "A page of objects. `X-Velstra-Revision` is the revision the list is consistent at.",
                "content": { "application/json": { "schema": {
                    "type": "object",
                    "required": ["items", "revision"],
                    "properties": {
                        "items": { "type": "array", "items": { "$ref": format!("#/components/schemas/{schema}") } },
                        "revision": { "type": "string" },
                        "nextPageToken": { "type": "string" },
                    },
                } } },
            },
            "default": error_response(),
        },
    }));
    if creatable {
        collection.insert("post".into(), json!({
            "tags": [title],
            "summary": format!("Create one of {title}"),
            "operationId": format!("create-{kind}"),
            "parameters": params,
            "requestBody": { "required": true, "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["id", "spec"],
                "properties": {
                    "id": { "type": "string", "description": "The last segment of the name." },
                    "spec": { "$ref": format!("#/components/schemas/{schema}Spec") },
                    "labels": { "type": "object", "additionalProperties": { "type": "string" } },
                },
            } } } },
            "responses": {
                "201": { "description": "Created.", "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{schema}") } } } },
                "default": error_response(),
            },
        }));
    }
    paths.insert(base.clone(), Value::Object(collection));

    let mut object_params = params.clone();
    object_params.push(path_param(
        "name",
        "The object's id — the last segment of its name.",
    ));
    let mut object = Map::new();
    object.insert("get".into(), json!({
        "tags": [title],
        "summary": format!("Read one of {title}"),
        "operationId": format!("get-{kind}"),
        "parameters": object_params,
        "responses": {
            "200": { "description": "The object.", "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{schema}") } } } },
            "default": error_response(),
        },
    }));
    if editable {
        object.insert("patch".into(), json!({
            "tags": [title],
            "summary": format!("Change the spec of one of {title}"),
            "description": "Only `spec` and `labels` are writable here; `status` is the owning agent's, written through `:reportStatus`. Send `If-Match` with the revision read to make the write a compare-and-swap.",
            "operationId": format!("patch-{kind}"),
            "parameters": object_params,
            "requestBody": { "required": true, "content": { "application/json": { "schema": {
                "type": "object",
                "properties": {
                    "spec": { "$ref": format!("#/components/schemas/{schema}Spec") },
                    "labels": { "type": "object", "additionalProperties": { "type": "string" } },
                },
            } } } },
            "responses": {
                "200": { "description": "The object as stored.", "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{schema}") } } } },
                "default": error_response(),
            },
        }));
    }
    if deletable {
        object.insert("delete".into(), json!({
            "tags": [title],
            "summary": format!("Delete one of {title}"),
            "description": "Marks the object for deletion; it is gone once every finalizer has released it.",
            "operationId": format!("delete-{kind}"),
            "parameters": object_params,
            "responses": {
                "202": { "description": "Deleting: the object exists until its finalizers release it." },
                "204": { "description": "Gone." },
                "default": error_response(),
            },
        }));
    }
    paths.insert(format!("{base}/{{name}}"), Value::Object(object));

    for verb in VERBS.iter().filter(|v| v.collection == kind) {
        let (path, mut parameters) = if verb.on_collection {
            (format!("{base}:{}", verb.verb), params.clone())
        } else {
            (
                format!("{base}/{{name}}:{}", verb.verb),
                object_params.clone(),
            )
        };
        for (name, what) in verb.query {
            parameters.push(query_param(name, "string", what));
        }
        let mut op = json!({
            "tags": [title],
            "summary": verb.summary,
            "operationId": format!("{}-{kind}", verb.verb),
            "parameters": parameters,
            "responses": {
                "200": { "description": "The answer.", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true } } } },
                "default": error_response(),
            },
        });
        if verb.method == "post" {
            op["requestBody"] = json!({ "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true } } } });
        }
        paths.insert(path, json!({ verb.method: op }));
    }
}

/// A custom method — AIP-136's `name:verb` — and where it lives.
struct Verb {
    collection: &'static str,
    verb: &'static str,
    method: &'static str,
    on_collection: bool,
    summary: &'static str,
    query: &'static [(&'static str, &'static str)],
}

/// The custom methods `rest.rs` dispatches, each on the collection whose
/// objects answer it. `tests/openapi.rs` checks that every verb the router
/// matches on is in this list, so a verb added there without a line here is a
/// red build rather than a hole in the document.
const VERBS: &[Verb] = &[
    Verb {
        collection: "instances",
        verb: "explainPlacement",
        method: "get",
        on_collection: false,
        summary: "Why the guest is where it is — or which rule stopped it going anywhere.",
        query: &[],
    },
    Verb {
        collection: "instances",
        verb: "explainMigration",
        method: "get",
        on_collection: false,
        summary: "Where the guest could move, and which node refused for what.",
        query: &[("mode", "`live` or `cold`; they refuse different things.")],
    },
    Verb {
        collection: "instances",
        verb: "explainRecovery",
        method: "get",
        on_collection: false,
        summary: "What would happen to the guest if its node stopped answering.",
        query: &[],
    },
    Verb {
        collection: "instances",
        verb: "console",
        method: "post",
        on_collection: false,
        summary:
            "Open a console session to the guest; the answer names the session and its ticket.",
        query: &[],
    },
    Verb {
        collection: "instances",
        verb: "consoleStream",
        method: "get",
        on_collection: false,
        summary: "The console stream itself, as a WebSocket upgrade.",
        query: &[
            ("session", "The session `:console` returned."),
            ("ticket", "Its ticket — what the node checks."),
        ],
    },
    Verb {
        collection: "floatingips",
        verb: "explainReach",
        method: "get",
        on_collection: false,
        summary: "Whether the address reaches a guest, and every hop that decides it.",
        query: &[],
    },
    Verb {
        collection: "projects",
        verb: "explainUsage",
        method: "get",
        on_collection: false,
        summary: "What the project had, and when.",
        query: &[(
            "month",
            "A month as `YYYY-MM`; the current one when absent.",
        )],
    },
    Verb {
        collection: "projects",
        verb: "explainQuota",
        method: "get",
        on_collection: false,
        summary: "What the project has left, and what it could actually start.",
        query: &[],
    },
    Verb {
        collection: "nodes",
        verb: "explainMaintenance",
        method: "get",
        on_collection: false,
        summary: "What taking the machine out of service would move, and where to.",
        query: &[],
    },
    Verb {
        collection: "nodes",
        verb: "explainCpu",
        method: "get",
        on_collection: true,
        summary: "The cell's processor generations, and what can migrate where.",
        query: &[],
    },
    Verb {
        collection: "nodes",
        verb: "explainCapacity",
        method: "get",
        on_collection: true,
        summary: "What the cell has in silicon and what it can still schedule.",
        query: &[
            ("node", "One machine rather than all."),
            ("pool", "One pool rather than all."),
        ],
    },
    Verb {
        collection: "nodes",
        verb: "issueCredential",
        method: "post",
        on_collection: false,
        summary: "Mint a fresh credential for a machine that already exists; shown once.",
        query: &[],
    },
    Verb {
        collection: "pools",
        verb: "issueCredential",
        method: "post",
        on_collection: false,
        summary: "Mint a fresh credential for a pool that already exists; shown once.",
        query: &[],
    },
    Verb {
        collection: "instances",
        verb: "reportStatus",
        method: "post",
        on_collection: false,
        summary:
            "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
        query: &[],
    },
    Verb {
        collection: "volumes",
        verb: "reportStatus",
        method: "post",
        on_collection: false,
        summary:
            "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
        query: &[],
    },
    Verb {
        collection: "attachments",
        verb: "reportStatus",
        method: "post",
        on_collection: false,
        summary:
            "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
        query: &[],
    },
    Verb {
        collection: "ports",
        verb: "reportStatus",
        method: "post",
        on_collection: false,
        summary:
            "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
        query: &[],
    },
    Verb {
        collection: "nodes",
        verb: "reportStatus",
        method: "post",
        on_collection: false,
        summary:
            "The machine's own report. A node identity only; `If-Match` carries the revision read.",
        query: &[],
    },
];

/// Every verb the document knows, for the drift check.
pub fn verbs() -> Vec<&'static str> {
    let mut out: Vec<&str> = VERBS.iter().map(|v| v.verb).collect();
    out.sort_unstable();
    out.dedup();
    out
}

// ---- the routes the router spells out by hand ------------------------------

fn fixed_paths(paths: &mut Map<String, Value>) {
    paths.insert("/api/v1/sessions".into(), json!({
        "post": {
            "tags": ["Sessions"],
            "summary": "Sign in with a username and password; the answer carries the token every other route wants.",
            "operationId": "sign-in",
            "security": [],
            "requestBody": { "required": true, "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["username", "password"],
                "properties": { "username": { "type": "string" }, "password": { "type": "string", "format": "password" } },
            } } } },
            "responses": {
                "200": { "description": "Signed in.", "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": { "token": { "type": "string" }, "expiresAt": { "type": "integer" } },
                } } } },
                "default": error_response(),
            },
        },
    }));
    paths.insert("/api/v1/sessions/current".into(), json!({
        "get": {
            "tags": ["Sessions"],
            "summary": "Who this token is, and what it may do (`cellAdmin`, and the strongest rung per project).",
            "operationId": "whoami",
            "responses": {
                "200": { "description": "The caller.", "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": {
                        "subject": { "type": "string" },
                        "displayName": { "type": "string" },
                        "cellAdmin": { "type": "boolean" },
                        "projects": { "type": "object", "additionalProperties": { "type": "string", "enum": ["viewer", "operator", "editor", "admin"] } },
                    },
                } } } },
                "default": error_response(),
            },
        },
        "delete": {
            "tags": ["Sessions"],
            "summary": "Sign out: end the session this token names.",
            "operationId": "sign-out",
            "responses": { "204": { "description": "Ended." }, "default": error_response() },
        },
    }));
    paths.insert("/api/v1/users/{id}/password".into(), json!({
        "put": {
            "tags": ["Users"],
            "summary": "Set a password. One's own with the current one; anybody's as a cell operator.",
            "operationId": "set-password",
            "parameters": [path_param("id", "The account.")],
            "requestBody": { "required": true, "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["password"],
                "properties": { "current": { "type": "string", "format": "password" }, "password": { "type": "string", "format": "password" } },
            } } } },
            "responses": { "204": { "description": "Set; every other session of the account is ended." }, "default": error_response() },
        },
    }));
    paths.insert("/api/v1/users/{id}/tokens".into(), json!({
        "post": {
            "tags": ["Users"],
            "summary": "Mint a token for a service account. Shown once; several may exist so rotation has no gap.",
            "operationId": "mint-token",
            "parameters": [path_param("id", "The service account.")],
            "requestBody": { "content": { "application/json": { "schema": {
                "type": "object",
                "properties": { "purpose": { "type": "string", "description": "What tells this token apart when one has to go." } },
            } } } },
            "responses": {
                "200": { "description": "The token, once.", "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": { "token": { "type": "string" }, "user": { "type": "string" }, "purpose": { "type": "string" }, "shownOnce": { "type": "boolean" } },
                } } } },
                "default": error_response(),
            },
        },
    }));
    paths.insert("/metrics".into(), json!({
        "get": {
            "tags": ["Cell"],
            "summary": "Prometheus text exposition, behind the same bearer token as everything else.",
            "operationId": "metrics",
            "responses": { "200": { "description": "The counters.", "content": { "text/plain": { "schema": { "type": "string" } } } } },
        },
    }));
    paths.insert("/api/v1/openapi.json".into(), json!({
        "get": {
            "tags": ["Cell"],
            "summary": "This document.",
            "operationId": "openapi",
            "security": [],
            "responses": { "200": { "description": "OpenAPI 3.1.", "content": { "application/json": { "schema": { "type": "object" } } } } },
        },
    }));
}

// ---- shared pieces -----------------------------------------------------------

fn common_schemas(schemas: &mut Map<String, Value>) {
    schemas.insert("Meta".into(), json!({
        "type": "object",
        "required": ["name", "uid", "generation", "revision", "placement", "createdAt"],
        "properties": {
            "name": { "type": "string", "description": "The full resource name, `projects/p1/instances/i1`." },
            "uid": { "type": "string" },
            "generation": { "type": "integer", "description": "Bumped on every change to spec." },
            "revision": { "type": "string", "description": "The store revision; what `If-Match` and `fromRevision` carry." },
            "placement": { "type": "object", "required": ["region", "cell"], "properties": { "region": { "type": "string" }, "cell": { "type": "string" } } },
            "createdAt": { "type": "integer", "description": "Milliseconds since the Unix epoch." },
            "deletedAt": { "type": ["integer", "null"] },
            "finalizers": { "type": "array", "items": { "type": "string" } },
            "labels": { "type": "object", "additionalProperties": { "type": "string" } },
        },
    }));
    schemas.insert("Condition".into(), json!({
        "type": "object",
        "required": ["kind", "status"],
        "properties": {
            "kind": { "type": "string", "description": "`Ready`, `Scheduled`, `Applied`, …" },
            "status": { "type": "string", "enum": ["True", "False", "Unknown"] },
            "reason": { "type": "string", "description": "One word in CamelCase, for a program." },
            "message": { "type": "string", "description": "A sentence, for a person." },
            "observedGeneration": { "type": "integer" },
            "lastTransition": { "type": "integer" },
        },
    }));
    schemas.insert("Status".into(), json!({
        "type": "object",
        "description": "The owning agent's report. `Unknown` is an honest absence of knowledge — there is no `PENDING` — and the rest of the shape is the agent's.",
        "properties": {
            "observedGeneration": { "type": "integer" },
            "conditions": { "type": "array", "items": { "$ref": "#/components/schemas/Condition" } },
        },
        "additionalProperties": true,
    }));
    schemas.insert("Error".into(), json!({
        "type": "object",
        "required": ["error"],
        "properties": { "error": {
            "type": "object",
            "required": ["code", "message"],
            "properties": {
                "code": { "type": "string", "enum": ["INVALID_ARGUMENT", "NOT_FOUND", "ALREADY_EXISTS", "FAILED_PRECONDITION", "ABORTED", "RESOURCE_EXHAUSTED", "PERMISSION_DENIED", "UNAUTHENTICATED", "INTERNAL"] },
                "message": { "type": "string", "description": "A sentence for a person." },
                "field": { "type": "string", "description": "The offending path, `spec.vcpus`, when there is one." },
            },
        } },
    }));
}

fn error_response() -> Value {
    json!({
        "description": "Refused, with a code a program can branch on and a sentence a person can read.",
        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
    })
}

fn path_param(name: &str, what: &str) -> Value {
    json!({ "name": name, "in": "path", "required": true, "description": what, "schema": { "type": "string" } })
}

fn query_param(name: &str, kind: &str, what: &str) -> Value {
    json!({ "name": name, "in": "query", "required": false, "description": what, "schema": { "type": kind } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_served_collection_has_its_two_paths_and_its_schema() {
        let doc = document();
        for kind in crate::COLLECTIONS {
            let list = doc["paths"]
                .as_object()
                .unwrap()
                .keys()
                .find(|p| p.ends_with(&format!("/{kind}")))
                .unwrap_or_else(|| panic!("no list path for {kind}"));
            assert!(
                doc["paths"].get(format!("{list}/{{name}}")).is_some(),
                "no object path for {kind}"
            );
            let schema = schema_name(
                kind,
                velstra_cloud_console::COLLECTIONS
                    .iter()
                    .find(|c| c.id == kind),
            );
            assert!(
                doc["components"]["schemas"].get(&schema).is_some(),
                "no schema {schema} for {kind}"
            );
        }
    }

    #[test]
    fn a_project_scoped_collection_sits_under_its_project() {
        let doc = document();
        assert!(doc["paths"]
            .get("/api/v1/projects/{project}/instances/{name}")
            .is_some());
        assert!(doc["paths"].get("/api/v1/nodes/{name}").is_some());
        assert!(doc["paths"].get("/api/v1/projects/{name}").is_some());
    }

    #[test]
    fn names_read_like_the_operator_says_them() {
        assert_eq!(
            schema_name(
                "security-groups",
                velstra_cloud_console::COLLECTIONS
                    .iter()
                    .find(|c| c.id == "security-groups")
            ),
            "SecurityGroup"
        );
        assert_eq!(schema_name("no-such-things", None), "NoSuchThing");
    }

    #[test]
    fn a_required_console_field_is_required_here_too() {
        let doc = document();
        let ceph = &doc["components"]["schemas"]["CephClusterSpec"];
        assert!(
            ceph["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "publicNetwork"),
            "{ceph}"
        );
        assert_eq!(ceph["properties"]["publicNetwork"]["format"], "cidr");
    }

    #[test]
    fn the_sign_in_route_needs_no_token_and_the_rest_do() {
        let doc = document();
        assert_eq!(
            doc["paths"]["/api/v1/sessions"]["post"]["security"],
            json!([])
        );
        assert!(doc["paths"]["/api/v1/nodes"]["get"]
            .get("security")
            .is_none());
        assert_eq!(doc["security"], json!([{ "bearer": [] }]));
    }

    #[test]
    fn the_pretty_form_ends_in_one_newline_and_parses_back() {
        let text = pretty();
        assert!(text.ends_with("}\n") && !text.ends_with("\n\n"));
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["openapi"], "3.1.0");
    }
}
