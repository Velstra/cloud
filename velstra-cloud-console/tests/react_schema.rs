//! The React console's `schema.json` must be the generated one.
//!
//! Both consoles are drawn from [`velstra_cloud_console::COLLECTIONS`]:
//! the page embeds it at build time, and the React console reads a copy checked
//! in beside its source. Only the first of those follows the schema on its own.
//! The copy was written once, by hand, from a promise in a commit message — and
//! a promise is not a mechanism. Without this, a field added to the model
//! reaches one console and quietly misses the other, which is the exact failure
//! the shared schema exists to prevent.
//!
//!     VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema
//!
//! rewrites it — and, with it, `schema.d.ts`, which is the same promise one
//! layer up. TypeScript only reads the keys its own type declares, so a type
//! written by hand beside a generated document is a second copy that drifts
//! silently: eleven keys the schema carried — every `check`, every `unit`,
//! every `min`/`max`, the words a boolean column reads by — were serialised,
//! shipped, and then unreachable from the code, because naming one was a
//! compile error. The declarations are derived from the generated JSON rather
//! than from the Rust types, so what TypeScript believes is exactly what is in
//! the file.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde_json::Value;

/// The TypeScript shape of one JSON value.
///
/// Widest wins: a key that is a string in one entry and null in another is
/// `string | null`, because the console has to handle both. A value that is
/// never present on a variant is absent from that variant, which is what makes
/// the union worth having — `f.check` is reachable on a text field and a
/// compile error on a number one.
fn shape_of(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
        Value::Array(items) => {
            let mut inner: BTreeSet<String> = items.iter().map(shape_of).collect();
            inner.remove("null");
            match inner.len() {
                0 => "unknown[]".into(),
                1 => format!("{}[]", inner.into_iter().next().unwrap_or_default()),
                _ => format!("({})[]", inner.into_iter().collect::<Vec<_>>().join(" | ")),
            }
        }
        Value::Object(fields) => format!(
            "{{ {} }}",
            fields
                .iter()
                .map(|(k, v)| format!("{k}: {}", shape_of(v)))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

/// One variant of a tagged union: the keys every entry carrying this tag has,
/// optional where some entry does not.
fn variant(tag: &str, name: &str, entries: &[&Value]) -> String {
    let mut keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for entry in entries {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        for (key, value) in fields {
            if key == tag {
                continue;
            }
            keys.entry(key.clone()).or_default().insert(shape_of(value));
            *seen.entry(key.clone()).or_default() += 1;
        }
    }
    let mut out = format!("  | {{ {tag}: \"{name}\"");
    for (key, shapes) in &keys {
        // Literal shapes are unioned; a key missing from some entry is
        // optional, which is how "`also` only on the fields that have one"
        // reaches TypeScript.
        let optional = seen.get(key).copied().unwrap_or(0) < entries.len();
        let mut kinds: Vec<&str> = shapes.iter().map(String::as_str).collect();
        kinds.sort_unstable();
        out.push_str(&format!(
            "; {key}{}: {}",
            if optional { "?" } else { "" },
            kinds.join(" | ")
        ));
    }
    out.push_str(" }");
    out
}

/// Every value of a tagged union found in the document, in the order the tags
/// first appear, as a TypeScript discriminated union.
fn union(name: &str, tag: &str, found: &[&Value]) -> String {
    let mut tags: Vec<String> = Vec::new();
    for entry in found {
        if let Some(t) = entry.get(tag).and_then(Value::as_str) {
            if !tags.iter().any(|k| k == t) {
                tags.push(t.to_string());
            }
        }
    }
    let mut out = format!("export type {name} =\n");
    for t in &tags {
        let entries: Vec<&Value> = found
            .iter()
            .copied()
            .filter(|e| e.get(tag).and_then(Value::as_str) == Some(t.as_str()))
            .collect();
        out.push_str(&variant(tag, t, &entries));
        out.push('\n');
    }
    out.push_str(";\n");
    out
}

/// The declarations the React console compiles against, derived from the
/// document it will read at runtime.
fn declarations(schema: &Value) -> String {
    let collections = schema.as_array().cloned().unwrap_or_default();
    let fields: Vec<&Value> = collections
        .iter()
        .filter_map(|c| c.get("fields")?.as_array())
        .flatten()
        .collect();
    let columns: Vec<&Value> = collections
        .iter()
        .filter_map(|c| c.get("columns")?.as_array())
        .flatten()
        .collect();
    let mut out = String::new();
    for line in [
        "// Generated from schema.json. Do not edit:",
        "// VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema",
        "//",
        "// Derived from the document itself rather than from the Rust types, so a",
        "// key the schema carries is a key TypeScript knows about — and a key it",
        "// does not carry for this kind of field is a compile error rather than a",
        "// value that is quietly `undefined`.",
        "",
    ] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&union("Field", "kind", &fields));
    out.push('\n');
    out.push_str(&union("Column", "cell", &columns));
    out
}

#[test]
fn the_typescript_declarations_are_the_generated_ones() {
    let schema: Value =
        serde_json::from_str(&velstra_cloud_console::as_pretty_json()).expect("the schema is JSON");
    let generated = declarations(&schema);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../velstra-cloud-console-react/src/schema.d.ts");
    if std::env::var_os("VELSTRA_WRITE_SCHEMA").is_some() {
        std::fs::write(&path, &generated).expect("writing schema.d.ts");
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        stored == generated,
        "velstra-cloud-console-react/src/schema.d.ts is not what this schema \
         describes, so the React console is compiled against a different set of \
         fields than it reads. Regenerate it: \
         VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema"
    );
}

#[test]
fn the_checked_in_schema_is_the_generated_one() {
    let generated = velstra_cloud_console::as_pretty_json();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../velstra-cloud-console-react/src/schema.json");
    if std::env::var_os("VELSTRA_WRITE_SCHEMA").is_some() {
        std::fs::write(&path, &generated).expect("writing schema.json");
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {}: {e}. Generate it with \
             VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema",
            path.display()
        )
    });
    assert!(
        stored == generated,
        "velstra-cloud-console-react/src/schema.json is not the schema this crate \
         generates, so the two consoles are drawn from different descriptions of \
         the same objects. Regenerate it: \
         VELSTRA_WRITE_SCHEMA=1 cargo test -p velstra-cloud-console --test react_schema"
    );
}
