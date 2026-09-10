//! The command line for a cell.
//!
//! Until this existed, everything outside the web console had to be written by
//! hand against the REST surface: no way to script a cell, nothing to put in a
//! CI job, and nothing to hand somebody who asks "how do I list my machines".
//!
//! ## Driven by the same schema as the console
//!
//! Every collection, field and column comes from
//! [`velstra_cloud_console::COLLECTIONS`] — the one description of this
//! platform's surface that the API, the console and its OpenAPI document all
//! already read. So a collection added to the schema is a command here without
//! anybody writing one, the table columns are the console's columns, and
//! `velstra get instances` and the Instances board cannot drift apart.
//!
//! ## What it deliberately does not do
//!
//! It holds no state beyond the token: no contexts, no cached objects, no
//! local notion of "current project" other than the flag and the environment.
//! A tool that remembered which cell you were pointed at is a tool that
//! deletes the wrong thing on the day you forget.

use std::io::Write;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};
use velstra_cloud_console::{COLLECTIONS, Cell, Collection, Scope};

#[derive(Parser)]
#[command(
    name = "velstra",
    about = "Run a Velstra cell from a terminal.",
    long_about = "Run a Velstra cell from a terminal.\n\nEvery collection the API serves is a \
                  noun here: `velstra get instances`, `velstra create volumes data-1 \
                  --set sizeGib=100`. `velstra collections` lists them."
)]
struct Cli {
    /// Where the cell answers, as a URL.
    #[arg(long, env = "VELSTRA_API", default_value = "https://127.0.0.1:8443")]
    api: String,

    /// The bearer token. Minted for a service account with
    /// `POST /api/v1/users/<id>/tokens`, or taken from a sign-in.
    #[arg(long, env = "VELSTRA_TOKEN")]
    token: Option<String>,

    /// The project to work in, for the collections that live in one.
    #[arg(long, env = "VELSTRA_PROJECT")]
    project: Option<String>,

    /// Trust a certificate this machine cannot verify.
    ///
    /// For a cell serving a self-signed certificate, which is what a fresh
    /// install does. Named for what it costs rather than for what it enables:
    /// it turns off the check that would notice somebody in the middle.
    ///
    /// As an environment variable, anything but `0`, `false` or empty counts —
    /// `VELSTRA_INSECURE=1` is what a person writes, and refusing it because
    /// it is not the word `true` would be a tool arguing about spelling.
    #[arg(long, env = "VELSTRA_INSECURE", value_parser = truthy, default_value = "false")]
    insecure: bool,

    #[command(subcommand)]
    what: What,
}

/// Whether a person meant yes.
fn truthy(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => Err(format!("`{other}` is not a yes or a no")),
    }
}

#[derive(Subcommand)]
enum What {
    /// Every collection this cell serves, and what it is called.
    Collections,
    /// List a collection, or read one object.
    Get {
        /// `instances`, `volumes`, `nodes` — see `velstra collections`.
        collection: String,
        /// One object's id. Omitted, the whole collection is listed.
        name: Option<String>,
        /// As JSON rather than a table.
        #[arg(long, short)]
        json: bool,
        /// Only objects carrying these labels, as `env=prod,tier=web`.
        #[arg(long)]
        labels: Option<String>,
    },
    /// Make one object.
    Create {
        collection: String,
        /// The id it gets. The rest of its name is the project and collection.
        id: String,
        /// A field, as `key=value`. Repeatable. Values that parse as a number,
        /// a boolean or JSON are sent as that; everything else as a string.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// A field whose value is a file, as `key=path`. Repeatable.
        ///
        /// For the fields that are documents rather than words — cloud-init
        /// above all. A shell mangles a multi-line value on the way past, and
        /// a `#cloud-config` that arrives as one line is a guest that boots
        /// with none of it.
        #[arg(long = "set-file", value_name = "KEY=PATH")]
        set_file: Vec<String>,
        /// A key of your own invention, so this create can be retried safely.
        ///
        /// Send the same one again and the second attempt is answered with the
        /// first attempt's operation instead of making a second object. Worth
        /// it in a script: without a key, a create whose answer was lost has
        /// to be resolved by looking, which is a race.
        #[arg(long = "idempotency-key", value_name = "KEY")]
        idempotency_key: Option<String>,
    },
    /// Change one object.
    Patch {
        collection: String,
        name: String,
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        #[arg(long = "set-file", value_name = "KEY=PATH")]
        set_file: Vec<String>,
    },
    /// Ask for one object to go.
    Delete {
        collection: String,
        name: String,
        /// Do not ask first.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Ask the API to explain something: `velstra explain instances db-1 placement`.
    Explain {
        collection: String,
        name: String,
        /// `placement`, `migration`, `recovery`, `quota`, `usage`, `capacity`.
        question: String,
    },
    /// Who this token is, and what it may do.
    Whoami,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        // One line, on stderr, and a non-zero status. A tool used in a script
        // is a tool whose failure has to be visible to `set -e` before it is
        // readable by a person.
        eprintln!("velstra: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(cli.insecure)
        .build()?;
    let cell = Cellular {
        api: cli.api.trim_end_matches('/').to_string(),
        token: cli.token.clone(),
        client,
    };
    match &cli.what {
        What::Collections => {
            let mut rows: Vec<[String; 3]> = COLLECTIONS
                .iter()
                .map(|c| {
                    [
                        c.id.to_string(),
                        match c.scope {
                            Scope::Project => "project".into(),
                            Scope::Global => "cell".into(),
                        },
                        c.blurb
                            .split(['.', '—'])
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                    ]
                })
                .collect();
            rows.sort();
            print_table(&["COLLECTION", "SCOPE", "WHAT IT IS"], &rows);
            Ok(())
        }
        What::Whoami => {
            let who = cell.get("/api/v1/sessions/current").await?;
            println!("{}", serde_json::to_string_pretty(&who)?);
            Ok(())
        }
        What::Get {
            collection,
            name,
            json,
            labels,
        } => {
            let c = find(collection)?;
            let base = base_path(c, cli.project.as_deref())?;
            match name {
                Some(name) => {
                    let object = cell.get(&format!("{base}/{name}")).await?;
                    println!("{}", serde_json::to_string_pretty(&object)?);
                }
                None => {
                    let items = cell.list(&base, labels.as_deref()).await?;
                    if *json {
                        println!("{}", serde_json::to_string_pretty(&items)?);
                    } else {
                        print_collection(c, &items);
                    }
                }
            }
            Ok(())
        }
        What::Create {
            collection,
            id,
            set,
            set_file,
            idempotency_key,
        } => {
            let c = find(collection)?;
            if !c.creatable {
                bail!(
                    "{} is not something a client creates; it is written by the platform",
                    c.id
                );
            }
            let base = base_path(c, cli.project.as_deref())?;
            let name = match c.scope {
                Scope::Project => format!(
                    "projects/{}/{}/{id}",
                    cli.project.as_deref().unwrap_or_default(),
                    c.id
                ),
                Scope::Global => format!("{}/{id}", c.id),
            };
            let body =
                serde_json::json!({ "meta": { "name": name }, "spec": spec_from(set, set_file)? });
            let answer = cell.post(&base, &body, idempotency_key.as_deref()).await?;
            // The API answers 202 with the operation to wait on, not the
            // object: it exists and has not converged. Said plainly, because a
            // script that assumed otherwise would read a field that is not
            // there yet.
            println!("{}", serde_json::to_string_pretty(&answer)?);
            Ok(())
        }
        What::Patch {
            collection,
            name,
            set,
            set_file,
        } => {
            let c = find(collection)?;
            let base = base_path(c, cli.project.as_deref())?;
            let at = format!("{base}/{name}");
            // Read first, for the revision. The API refuses a change that does
            // not say which version it was made against, which is what stops
            // two people overwriting each other without either being told.
            let current = cell.get(&at).await?;
            let revision = current["meta"]["revision"].as_str().map(str::to_string);
            let body = serde_json::json!({ "spec": spec_from(set, set_file)? });
            let answer = cell.patch(&at, &body, revision.as_deref()).await?;
            println!("{}", serde_json::to_string_pretty(&answer)?);
            Ok(())
        }
        What::Delete {
            collection,
            name,
            yes,
        } => {
            let c = find(collection)?;
            let base = base_path(c, cli.project.as_deref())?;
            if !yes {
                eprint!(
                    "Delete {}/{}? It stays visible until its finalizers let go. [y/N] ",
                    c.id, name
                );
                std::io::stderr().flush().ok();
                let mut said = String::new();
                std::io::stdin().read_line(&mut said)?;
                if !said.trim().eq_ignore_ascii_case("y") {
                    bail!("nothing was deleted");
                }
            }
            cell.delete(&format!("{base}/{name}")).await?;
            println!("{}/{name}: deletion asked for", c.id);
            Ok(())
        }
        What::Explain {
            collection,
            name,
            question,
        } => {
            let c = find(collection)?;
            let base = base_path(c, cli.project.as_deref())?;
            let verb = format!("explain{}", capitalise(question));
            let answer = cell.get(&format!("{base}/{name}:{verb}")).await?;
            println!("{}", serde_json::to_string_pretty(&answer)?);
            Ok(())
        }
    }
}

/// One cell, and how to talk to it.
struct Cellular {
    api: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl Cellular {
    fn at(&self, path: &str) -> String {
        format!("{}{path}", self.api)
    }

    fn signed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let answer = self
            .signed(self.client.get(self.at(path)))
            .send()
            .await
            .with_context(|| format!("asking {}", self.at(path)))?;
        read(answer).await
    }

    /// Every page of a collection, because a caller who asked for a list meant
    /// the list. The API answers a page and a token; following it here is what
    /// keeps a script from silently seeing the first hundred objects.
    async fn list(&self, base: &str, labels: Option<&str>) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = format!("{base}?pageSize=200");
            if let Some(labels) = labels {
                url.push_str(&format!("&labels={labels}"));
            }
            if let Some(page) = &token {
                url.push_str(&format!("&pageToken={page}"));
            }
            let answer = self.get(&url).await?;
            if let Some(items) = answer["items"].as_array() {
                out.extend(items.clone());
            }
            match answer["nextPageToken"].as_str() {
                Some(next) if !next.is_empty() => token = Some(next.to_string()),
                _ => return Ok(out),
            }
        }
    }

    async fn post(&self, path: &str, body: &Value, key: Option<&str>) -> Result<Value> {
        let mut request = self.signed(self.client.post(self.at(path)).json(body));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        read(request.send().await?).await
    }

    async fn patch(&self, path: &str, body: &Value, revision: Option<&str>) -> Result<Value> {
        let mut request = self.signed(self.client.patch(self.at(path)).json(body));
        if let Some(revision) = revision {
            request = request.header("if-match", revision);
        }
        read(request.send().await?).await
    }

    async fn delete(&self, path: &str) -> Result<Value> {
        let answer = self
            .signed(self.client.delete(self.at(path)))
            .send()
            .await?;
        read(answer).await
    }
}

/// The body, or the API's own sentence about why not.
///
/// The refusal is `{ "error": { "code", "message", "field" } }`, and the
/// message is written to be read by the person who caused it — so it is what
/// comes out, rather than a status code this tool paraphrases.
async fn read(answer: reqwest::Response) -> Result<Value> {
    let status = answer.status();
    let text = answer.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(parsed);
    }
    let message = parsed["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| text.clone());
    let field = parsed["error"]["field"].as_str().unwrap_or_default();
    if field.is_empty() {
        Err(anyhow!("{message}"))
    } else {
        Err(anyhow!("{message} ({field})"))
    }
}

fn find(id: &str) -> Result<&'static Collection> {
    COLLECTIONS.iter().find(|c| c.id == id).ok_or_else(|| {
        let near: Vec<&str> = COLLECTIONS
            .iter()
            .map(|c| c.id)
            .filter(|name| name.starts_with(id.split('-').next().unwrap_or(id)))
            .collect();
        if near.is_empty() {
            anyhow!("there is no collection called `{id}`. `velstra collections` lists them")
        } else {
            anyhow!(
                "there is no collection called `{id}`. Did you mean {}?",
                near.join(", ")
            )
        }
    })
}

fn base_path(c: &Collection, project: Option<&str>) -> Result<String> {
    match c.scope {
        Scope::Global => Ok(format!("/api/v1/{}", c.id)),
        Scope::Project => {
            let project = project.filter(|p| !p.is_empty()).ok_or_else(|| {
                anyhow!(
                    "{} lives in a project. Name one with --project or VELSTRA_PROJECT",
                    c.id
                )
            })?;
            Ok(format!("/api/v1/projects/{project}/{}", c.id))
        }
    }
}

/// `key=value` pairs into a spec.
///
/// A dotted key nests — `placementPolicy.spread=Required` — because that is
/// how the fields are named everywhere else in this platform, and a flag that
/// could not reach a nested one would send the caller to the JSON body.
fn spec_from(set: &[String], from_file: &[String]) -> Result<Value> {
    let mut spec = Map::new();
    let pairs = set
        .iter()
        .map(|p| (p, false))
        .chain(from_file.iter().map(|p| (p, true)));
    for (pair, is_file) in pairs {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            anyhow!(
                "`{pair}` is not `key={}`",
                if is_file { "path" } else { "value" }
            )
        })?;
        // A file's contents are the value, exactly as they are on disk: a
        // `#cloud-config` is a document, and anything that parsed or trimmed
        // it would change what the guest is handed.
        let value = if is_file {
            Value::String(
                std::fs::read_to_string(value)
                    .with_context(|| format!("reading {value} for `{key}`"))?,
            )
        } else {
            parse_value(value)
        };
        let mut at = &mut spec;
        let parts: Vec<&str> = key.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            at = at
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| anyhow!("`{key}` sets a field inside something that is not one"))?;
        }
        at.insert(parts[parts.len() - 1].to_string(), value);
    }
    Ok(Value::Object(spec))
}

/// What a value on the command line means.
///
/// JSON first, so a list or an object can be given as one; then the plain
/// scalars; then a string. `sizeGib=100` has to reach the API as a number, and
/// `schedulable=false` as a boolean, or every create would be refused for a
/// field of the wrong shape.
fn parse_value(raw: &str) -> Value {
    if let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && !parsed.is_string()
    {
        return parsed;
    }
    Value::String(raw.to_string())
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A collection as a table, in the columns the console shows.
fn print_collection(c: &Collection, items: &[Value]) {
    let mut head = vec!["NAME".to_string()];
    head.extend(c.columns.iter().map(|col| col.label.to_uppercase()));
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            let mut row = vec![id_of(item)];
            row.extend(
                c.columns
                    .iter()
                    .map(|col| cell_text(col.cell, at(item, col.path))),
            );
            row
        })
        .collect();
    let head: Vec<&str> = head.iter().map(String::as_str).collect();
    print_rows(&head, &rows);
    if items.is_empty() {
        eprintln!("nothing here yet");
    }
}

fn id_of(item: &Value) -> String {
    let name = item["meta"]["name"].as_str().unwrap_or_default();
    name.rsplit('/').next().unwrap_or(name).to_string()
}

fn at<'a>(item: &'a Value, path: &str) -> &'a Value {
    let mut here = item;
    for part in path.split('.') {
        here = match part.parse::<usize>() {
            Ok(index) => here.get(index).unwrap_or(&Value::Null),
            Err(_) => here.get(part).unwrap_or(&Value::Null),
        };
    }
    here
}

/// One value, in the words its column asks for.
fn cell_text(kind: Cell, value: &Value) -> String {
    match (kind, value) {
        (_, Value::Null) => "—".into(),
        (Cell::Count, Value::Array(items)) => items.len().to_string(),
        (Cell::Bytes, Value::Number(n)) => bytes(n.as_u64().unwrap_or(0)),
        (Cell::Yes { .. }, Value::Bool(b)) => if *b { "yes" } else { "no" }.into(),
        (Cell::Ago, Value::Number(n)) => ago(n.as_u64().unwrap_or(0)),
        (_, Value::String(s)) => s.clone(),
        (_, Value::Array(items)) => items.len().to_string(),
        (_, other) => other.to_string(),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn ago(millis: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seconds = now.saturating_sub(millis) / 1000;
    match seconds {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn print_table(head: &[&str], rows: &[[String; 3]]) {
    let rows: Vec<Vec<String>> = rows.iter().map(|r| r.to_vec()).collect();
    print_rows(head, &rows);
}

/// Columns wide enough for what is in them, and nothing wider.
fn print_rows(head: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, value) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(value.chars().count());
            }
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, value) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(value);
            } else {
                out.push_str(&format!("{value:<width$}  ", width = widths[i]));
            }
        }
        out.trim_end().to_string()
    };
    println!(
        "{}",
        line(&head.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    );
    for row in rows {
        println!("{}", line(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_reaches_the_api_as_the_shape_it_is() {
        // `sizeGib=100` as a string would be refused for a field of the wrong
        // shape, and every create would fail on the most ordinary flag there is.
        let spec = spec_from(
            &[
                "sizeGib=100".into(),
                "schedulable=false".into(),
                "pool=ceph".into(),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(spec["sizeGib"], serde_json::json!(100));
        assert_eq!(spec["schedulable"], serde_json::json!(false));
        assert_eq!(spec["pool"], serde_json::json!("ceph"));
    }

    #[test]
    fn a_dotted_key_nests_the_way_the_fields_are_named() {
        let spec = spec_from(&["placementPolicy.spread=Required".into()], &[]).unwrap();
        assert_eq!(
            spec["placementPolicy"]["spread"],
            serde_json::json!("Required")
        );
    }

    #[test]
    fn a_list_can_be_given_as_json() {
        let spec = spec_from(
            &[r#"networks=["projects/p1/networks/default"]"#.into()],
            &[],
        )
        .unwrap();
        assert_eq!(
            spec["networks"][0],
            serde_json::json!("projects/p1/networks/default")
        );
    }

    /// A shell mangles a multi-line value on the way past, and a
    /// `#cloud-config` that arrives as one line is a guest that boots with
    /// none of it. Verified the hard way, on a real guest.
    #[test]
    fn a_document_comes_from_a_file_exactly_as_it_is() {
        let path = std::env::temp_dir().join(format!("velstra-cli-{}.yaml", std::process::id()));
        std::fs::write(&path, "#cloud-config\nhostname: web-1\n").unwrap();
        let spec = spec_from(&[], &[format!("userData={}", path.display())]).unwrap();
        assert_eq!(
            spec["userData"].as_str().unwrap(),
            "#cloud-config\nhostname: web-1\n",
            "the document was changed on the way through"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_project_collection_says_it_needs_a_project() {
        let instances = find("instances").unwrap();
        let refused = base_path(instances, None).unwrap_err().to_string();
        assert!(refused.contains("--project"), "{refused}");
        assert_eq!(
            base_path(instances, Some("p1")).unwrap(),
            "/api/v1/projects/p1/instances"
        );
        // A cell-scoped one needs none.
        assert_eq!(
            base_path(find("nodes").unwrap(), None).unwrap(),
            "/api/v1/nodes"
        );
    }

    #[test]
    fn an_unknown_collection_suggests_the_ones_that_exist() {
        let refused = find("instance").unwrap_err().to_string();
        assert!(refused.contains("instances"), "{refused}");
    }

    #[test]
    fn every_collection_in_the_schema_is_reachable() {
        // The point of being schema-driven: a collection added to the console's
        // description is a command here without anybody writing one.
        for c in COLLECTIONS {
            let path = base_path(c, Some("p1")).unwrap();
            assert!(path.starts_with("/api/v1/"), "{}: {path}", c.id);
            assert!(path.ends_with(c.id), "{}: {path}", c.id);
        }
    }
}
