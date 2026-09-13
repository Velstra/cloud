//! The console this cell serves, read from disk once and answered from memory.
//!
//! ## Why from disk at all
//!
//! The other console — the framework-free one in `velstra-cloud-console` — is
//! a single string compiled into this binary, and that is the right shape for
//! it: one file, nothing fetched, available during an incident on a network
//! that can reach the API and nothing else. This one is a Vite build: an
//! `index.html` that names two dozen hashed files under `assets/`. Compiling
//! *that* in would mean `cargo build` could not run without node and a
//! network, in a repository whose CI gate is `cargo test --workspace`.
//!
//! So it ships as files, in the Debian package, and is read here. The promise
//! the other console makes still holds: every byte comes from this cell. The
//! build fetches nothing at runtime, and nothing here reaches the network.
//!
//! ## Read once, by name
//!
//! The whole tree is read at startup into a map from request path to bytes.
//! That is 2.3 MB and 48 files today, which is nothing beside the store, and
//! it buys two things worth more than the memory: no disk in the request path,
//! and **no path traversal to reason about**. A request does not name a file;
//! it looks up a key that was put there by walking the directory. `..` in a
//! URL cannot escape a `HashMap`.
//!
//! A directory that is not there, or has no `index.html`, is not an error: the
//! cell falls back to the built-in console, which is what a machine installed
//! before this existed — or a `cargo run` in a checkout — should do.

use std::{collections::HashMap, path::Path};

/// One file, with the two headers its answer needs.
pub struct File {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub cache_control: &'static str,
}

/// A built console, by request path: `/index.html`, `/assets/index-C1qhJsR-.js`.
pub struct Console {
    files: HashMap<String, File>,
}

/// What a browser must be told a file is.
///
/// A short list on purpose. Anything a Vite build emits that is not here is a
/// file this console does not serve, which is a louder failure than guessing
/// `application/octet-stream` and letting a browser refuse a stylesheet for
/// reasons it does not explain.
fn content_type(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    Some(match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/vnd.microsoft.icon",
        "woff2" => "font/woff2",
        "map" => "application/json",
        _ => return None,
    })
}

/// How long a browser may keep it.
///
/// Everything under `assets/` carries a content hash in its own name — Vite
/// puts it there — so the file at a given name can never change, and a year is
/// the honest answer. `index.html` is the opposite: it is the only name that
/// stays put while its contents move, so it must never be cached, or an
/// upgrade leaves a browser asking for the previous build's assets, which are
/// gone.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

impl Console {
    /// Read a built console out of a directory, or `None` if there is not one
    /// there.
    ///
    /// `None` rather than an error: a machine with no console directory is an
    /// ordinary machine, and the caller falls back to the built-in page.
    pub fn read(dir: &Path) -> Option<Console> {
        if !dir.join("index.html").is_file() {
            return None;
        }
        let mut files = HashMap::new();
        collect(dir, dir, &mut files);
        files
            .contains_key("/index.html")
            .then_some(Console { files })
    }

    pub fn get(&self, path: &str) -> Option<&File> {
        self.files.get(path)
    }

    /// The page itself, which every path that is not a file answers with.
    pub fn index(&self) -> &File {
        self.files
            .get("/index.html")
            .expect("read() refuses a console without one")
    }

    /// How many files this console can answer with.
    ///
    /// `count`, not `len`: a console is not a collection, and naming it `len`
    /// asks for an `is_empty` that would mean nothing — `read` already refuses
    /// a console with no page in it.
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.files.len()
    }
}

/// Walk the tree, keying each file by the path a browser will ask for.
///
/// Anything with an extension this cell will not name a type for is skipped
/// rather than stored: it could not be answered anyway, and a map that holds
/// it would make `count()` lie about what is servable.
fn collect(root: &Path, dir: &Path, out: &mut HashMap<String, File>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(key) = relative.to_str().map(|r| format!("/{r}")) else {
            continue;
        };
        let Some(content_type) = content_type(&key) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        out.insert(
            key.clone(),
            File {
                bytes,
                content_type,
                cache_control: cache_control(&key),
            },
        );
    }
}

/// Where the Debian package puts it.
pub const SHIPPED_AT: &str = "/usr/share/velstra-cloud/console";

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("velstra-console-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The tree is keyed by the path a browser asks for, nested files included.
    #[test]
    fn a_built_console_is_read_by_the_paths_a_browser_asks_for() {
        let dir = scratch("read");
        write(&dir, "index.html", "<!doctype html>");
        write(&dir, "favicon.svg", "<svg/>");
        write(&dir, "assets/index-abc.js", "console.log(1)");
        write(&dir, "assets/index-abc.css", "body{}");

        let console = Console::read(&dir).expect("a console");
        assert_eq!(console.count(), 4);
        assert_eq!(
            console.get("/assets/index-abc.js").map(|f| f.content_type),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            console.get("/assets/index-abc.css").map(|f| f.content_type),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(
            console.get("/favicon.svg").map(|f| f.content_type),
            Some("image/svg+xml")
        );
        assert_eq!(console.index().bytes, b"<!doctype html>");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hashed asset may be kept for ever; the one name that stays put may not.
    ///
    /// The failure this prevents is the one that only shows up on the *second*
    /// deploy: a cached `index.html` naming the previous build's assets, none
    /// of which are on the cell any more, so the console is a blank page with
    /// four 404s behind it.
    #[test]
    fn only_the_hashed_files_may_be_cached() {
        let dir = scratch("cache");
        write(&dir, "index.html", "<!doctype html>");
        write(&dir, "assets/index-abc.js", "1");

        let console = Console::read(&dir).expect("a console");
        assert_eq!(console.index().cache_control, "no-cache");
        assert_eq!(
            console.get("/assets/index-abc.js").map(|f| f.cache_control),
            Some("public, max-age=31536000, immutable")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No directory, or one with no page in it, is not a console — and not an
    /// error either. The cell serves its built-in one.
    #[test]
    fn a_directory_without_a_page_is_not_a_console() {
        assert!(Console::read(Path::new("/nonexistent/velstra/console")).is_none());

        let dir = scratch("empty");
        write(&dir, "assets/index-abc.js", "1");
        assert!(Console::read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request cannot walk out of the tree, because a request does not name a
    /// file: it names a key that was put there by reading the directory.
    #[test]
    fn nothing_outside_the_tree_can_be_asked_for() {
        let dir = scratch("escape");
        write(&dir, "index.html", "<!doctype html>");
        let console = Console::read(&dir).expect("a console");

        for asked in [
            "/../../../etc/passwd",
            "/assets/../../etc/passwd",
            "/etc/passwd",
            "/assets/",
            "",
        ] {
            assert!(console.get(asked).is_none(), "{asked} was answered");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
