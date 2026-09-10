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
//! rewrites it.

use std::path::Path;

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
