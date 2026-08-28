//! The certificate this machine serves its console with.
//!
//! ## Why there is one at all
//!
//! The API listened in plaintext and the documentation said to put a reverse
//! proxy in front. That is the right answer for a cluster and the wrong one for
//! the box that *is* the whole cell: it has nothing in front of it, nobody is
//! going to install nginx to look at a dashboard, and in the meantime an
//! administrator's password crossed the wire in the clear on a port numbered
//! 8443 that looked as though it would not.
//!
//! ## Self-signed, and what that is worth
//!
//! A self-signed certificate stops somebody reading the password off the wire.
//! It does **not** tell a browser which machine it is talking to — that is what
//! a signature from somebody the browser already trusts buys, and there is
//! nobody here to be that. So the browser warns, correctly, and the operator
//! clicks past it.
//!
//! Which makes the **fingerprint** the whole point of this module. Printed once,
//! at install, on the machine's own console, it is the one chance to know what
//! the certificate should be before ever seeing it from somewhere else. A
//! self-signed certificate whose fingerprint nobody was told is a warning
//! trained into a reflex.
//!
//! ## Replacing it
//!
//! Two files and a restart. Nothing here is special: the API takes any PEM pair,
//! so a real certificate goes in the same place and the seed does not change.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where the pair lives. Beside the seed, on the writable partition, because
/// that is the directory this machine already owns.
pub fn dir(root: &Path) -> PathBuf {
    root.join("tls")
}

pub struct Certificate {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// `AB:CD:…`, the sha256 of the DER, which is the spelling every browser
    /// shows and the only form worth writing down.
    pub fingerprint: String,
    /// False when a pair was already there. Nothing is overwritten: an operator
    /// who put a real certificate in this directory must not lose it to a second
    /// run of an installer that is otherwise safe to repeat.
    pub made: bool,
}

/// Make a self-signed pair for this machine, or leave the one that is there.
///
/// The names it is issued for are what somebody will type: the hostname, its
/// short form, `localhost`, and the addresses this machine answers on. A
/// certificate for a name nobody uses produces a second warning on top of the
/// unavoidable one, which is how a warning stops being read.
pub fn ensure(root: &Path, hostname: &str, addresses: &[String]) -> Result<Certificate> {
    let dir = dir(root);
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        let pem = std::fs::read_to_string(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        return Ok(Certificate {
            fingerprint: fingerprint_of_pem(&pem)?,
            cert: cert_path,
            key: key_path,
            made: false,
        });
    }

    let mut names: Vec<String> = Vec::new();
    for name in [hostname, hostname.split('.').next().unwrap_or(hostname)] {
        let name = name.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    for extra in ["localhost", "127.0.0.1"] {
        if !names.iter().any(|n| n == extra) {
            names.push(extra.to_string());
        }
    }
    for address in addresses {
        if !address.is_empty() && !names.iter().any(|n| n == address) {
            names.push(address.clone());
        }
    }

    let issued = rcgen::generate_simple_self_signed(names)
        .context("generating a self-signed certificate")?;
    let cert_pem = issued.cert.pem();
    let key_pem = issued.key_pair.serialize_pem();

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&cert_path, &cert_pem)
        .with_context(|| format!("writing {}", cert_path.display()))?;
    write_private(&key_path, &key_pem)?;

    Ok(Certificate {
        fingerprint: fingerprint_of_der(issued.cert.der()),
        cert: cert_path,
        key: key_path,
        made: true,
    })
}

/// The key is `0600` from the moment it exists, not afterwards.
///
/// Written through `OpenOptions` with the mode set, because a `write` followed
/// by a `set_permissions` leaves a window — short, and long enough — in which a
/// private key is world-readable on a machine that may have other accounts on
/// it. The same care the node token already gets.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `AB:CD:…` — the sha256 of the DER, upper case, colon separated.
pub fn fingerprint_of_der(der: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The same, for a certificate already on disk.
///
/// The PEM's base64 decoded by hand rather than with a crate: it is one
/// certificate, the format is four lines of framing around base64, and this is
/// the only place that needs it.
fn fingerprint_of_pem(pem: &str) -> Result<String> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END CERTIFICATE-----"))
        .collect();
    let der = base64_decode(&body).context("that file does not hold a certificate")?;
    Ok(fingerprint_of_der(&der))
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pair_is_made_once_and_never_replaced() {
        // The second run of an installer that is otherwise safe to repeat must
        // not throw away a certificate somebody put there on purpose.
        let dir = std::env::temp_dir().join(format!("velstra-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = ensure(&dir, "hv-fra-03.example", &["10.0.0.5".into()]).unwrap();
        assert!(first.made);
        assert!(first.cert.exists() && first.key.exists());

        let again = ensure(&dir, "hv-fra-03.example", &[]).unwrap();
        assert!(!again.made, "a second run replaced the certificate");
        assert_eq!(
            again.fingerprint, first.fingerprint,
            "the fingerprint read back off disk is not the one that was printed"
        );

        // The key is not readable by anybody else, from the moment it exists.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first.key).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the private key is readable by others");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fingerprint_reads_the_way_a_browser_shows_one() {
        assert_eq!(
            fingerprint_of_der(b"velstra"),
            fingerprint_of_der(b"velstra"),
            "the same bytes hash differently"
        );
        let printed = fingerprint_of_der(b"velstra");
        assert!(printed.contains(':'), "{printed}");
        assert_eq!(printed.len(), 32 * 3 - 1, "{printed}");
        assert!(
            printed.chars().all(|c| c.is_ascii_hexdigit() || c == ':'),
            "{printed}"
        );
    }
}
