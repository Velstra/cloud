//! The door where nothing is typed but an address.
//!
//! The other three doors all end with a secret in somebody's hands: a token
//! pasted, a token found on a stick, or a cell that has none yet. This one
//! turns the direction round — the machine asks, an operator answers.
//!
//! 1. It generates a keypair, here, once, in memory.
//! 2. It announces itself to the cell address, over TLS it cannot verify
//!    because it has no CA yet — and records the fingerprint of the
//!    certificate it was served.
//! 3. It prints **two** fingerprints on the screen: its own key's, and that
//!    certificate's.
//! 4. It waits, asking every few seconds whether somebody has approved it.
//! 5. When somebody has, it is handed the same join token door 2 pastes, and
//!    the rest of the install is identical.
//!
//! ## Why two fingerprints and not one
//!
//! One answers "is this the machine I am standing in front of" — the operator
//! sees the same value on the console's pending row, and only the holder of
//! the private key can produce it.
//!
//! The other answers "is this the cell I meant". The machine has no CA, so it
//! cannot verify what it connected to; it shows what it saw, and the control
//! plane's own banner prints that same value for itself. Somebody in the
//! middle has to present a certificate they hold the key for, which is a
//! different number, on this machine's screen. Both are computed by
//! [`crate::tls::fingerprint_of_pem`] — one function, so the two numbers an
//! operator compares cannot drift apart.
//!
//! ## Why `curl`
//!
//! The same reason `quickstart` shells out to it: an installer that carried an
//! HTTP stack, a TLS stack and an async runtime to make four requests would be
//! an installer nobody could audit for the sake of four requests. `--insecure`
//! is deliberate and is the whole point — the certificate is *unverifiable* at
//! this moment, and pretending otherwise is what the fingerprint on the screen
//! replaces.

use std::process::Command;

use anyhow::{Context, Result, bail};
use velstra_cloud_wire::join::JoinToken;

use crate::wizard::prompt;

/// How long to keep asking, and how often.
///
/// Twenty minutes is long enough to walk to a desk, compare a fingerprint and
/// click; it is also inside the hour an announcement stands, so a machine that
/// gives up here has not yet lost its row. Five seconds because an operator
/// clicking approve should see the machine move on while they are still
/// looking at it.
const WAIT_SECS: u64 = 20 * 60;
const EVERY_SECS: u64 = 5;

/// What the machine tells the cell about itself.
///
/// Every field is this machine's own claim and decides nothing. It is here so
/// that a person looking at three pending rows can tell which one is the box
/// in front of them — which, in a rack of identical machines, is the serial.
///
/// `hostname` is the one the wizard was given, never the live medium's own. The
/// installer runs on an ISO whose hostname is `velstra-node-installer`, and
/// that is what the first machine to try this announced itself as: a node
/// called after the medium rather than after itself, on the one field an
/// operator reads to know which box it is.
fn reported(hostname: &str) -> serde_json::Value {
    let disks: Vec<String> = crate::disks::discover_disks()
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.name)
        .collect();
    serde_json::json!({
        "hostname": hostname,
        "addresses": crate::cell::own_addresses(),
        "vcpus": num_cpus(),
        "memoryMib": memory_mib(),
        "disks": disks,
        "serial": dmi("product_serial"),
    })
}

fn num_cpus() -> u32 {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|t| t.lines().filter(|l| l.starts_with("processor")).count() as u32)
        .unwrap_or(0)
}

fn memory_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemTotal:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .map(|kib| kib / 1024)
        .unwrap_or(0)
}

/// A value the firmware wrote, when it wrote one.
///
/// Absent often enough that nothing may depend on it — a great many boards
/// report `To be filled by O.E.M.` — so an unhelpful answer is treated as no
/// answer rather than shown as if it meant something.
fn dmi(what: &str) -> String {
    dmi_from(&std::fs::read_to_string(format!("/sys/class/dmi/id/{what}")).unwrap_or_default())
}

/// The judgement half, pure and tested: a great many boards report a
/// placeholder, and showing one on an operator's screen as if it identified
/// the machine is worse than showing nothing.
fn dmi_from(raw: &str) -> String {
    let raw = raw.trim();
    let useless = [
        "to be filled by o.e.m.",
        "system serial number",
        "default string",
        "none",
        "0",
    ];
    if raw.is_empty() || useless.contains(&raw.to_lowercase().as_str()) {
        return String::new();
    }
    raw.to_string()
}

/// Ask the cell, show the fingerprints, and wait to be let in.
///
/// `hostname` is the name the wizard collected — it is asked before the door
/// is chosen, so it is known here and is what the machine announces itself as.
pub fn run(hostname: &str) -> Result<Option<JoinToken>> {
    println!("\nThis machine will ask the cell to let it in, and you approve it there.");
    println!("Nothing secret is typed here or in the console — you compare two numbers.");
    let url = loop {
        let got = prompt("Cell address (host or host:port): ")?;
        match cell_url(got.trim()) {
            Ok(url) => break url,
            Err(e) => println!("  {e}"),
        }
    };

    // Reachable at all, before a keypair and an announcement. "I cannot reach
    // that address" is the answer nine times out of ten, and it is worth
    // saying on its own rather than inside a sentence about an enrolment.
    if let Err(e) = reachable(&url) {
        println!("\n  {e}");
        println!("  Nothing was written, and this machine is unchanged.");
        return Ok(None);
    }

    let (pair, public) = keypair()?;
    let (answer, served) = announce(&url, &public, hostname)?;
    let id = match answer["id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            // Whatever the cell said, said back. An error body carries a
            // message written for exactly this moment; replacing it with a
            // sentence about a missing field is how a fixable problem becomes
            // an evening.
            let said = answer["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .trim();
            if said.is_empty() {
                bail!(
                    "the cell answered something this installer cannot read:\n  {}",
                    answer
                );
            }
            bail!("the cell refused the announcement: {said}");
        }
    };
    let mine = answer["fingerprint"].as_str().unwrap_or_default();

    println!("\n  This machine:  {mine}");
    println!("  The cell:      {served}");
    println!();
    println!("In the console, under Pending machines, there is a row whose fingerprint is");
    println!("the first number. Check it, give the machine a name, say what it is for, and");
    println!("approve it. The second number is this cell's own certificate — the banner on");
    println!("its console prints the same one, and if it does not, something is in between.");
    println!("\nWaiting for approval. Ctrl-C stops and changes nothing.");

    for tick in 0..(WAIT_SECS / EVERY_SECS) {
        std::thread::sleep(std::time::Duration::from_secs(EVERY_SECS));
        match claim(&url, &id, &pair) {
            Claimed::Token(token) => {
                println!("  approved — installing as {}", token.node);
                return Ok(Some(token));
            }
            Claimed::Waiting => {
                // Said once a minute rather than every five seconds: a screen
                // that scrolls is a screen nobody reads the fingerprints off.
                if tick % 12 == 11 {
                    println!("  still waiting…");
                }
            }
            Claimed::No(why) => {
                println!("\n  {why}");
                return Ok(None);
            }
        }
    }
    println!("\n  Nobody approved this machine within twenty minutes.");
    println!("  Its row stands for an hour from when it announced, so approving it now still");
    println!("  works — run the installer again and it will find the same row.");
    Ok(None)
}

/// `10.10.10.8` and `10.10.10.8:8443` and `https://10.10.10.8:8443` all mean
/// the same thing, because all three are what somebody types.
fn cell_url(typed: &str) -> Result<String> {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty() {
        bail!("the address of a machine that is already a control plane in the cell");
    }
    if typed.contains(' ') {
        bail!("that has a space in it");
    }
    if let Some(rest) = typed.strip_prefix("http://") {
        bail!("plain HTTP, and the cell serves TLS — try {rest} on its own");
    }
    let authority = typed.strip_prefix("https://").unwrap_or(typed);
    // A port only if one was typed. Bare IPv6 is the one shape where guessing
    // wrong is silent, so it is required to be bracketed if a port follows,
    // which is what a person copying an address from the banner already does.
    let has_port = match authority.rfind(':') {
        None => false,
        Some(at) => {
            let tail = &authority[at + 1..];
            !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit())
        }
    };
    let with_port = if has_port {
        authority.to_string()
    } else {
        format!("{authority}:8443")
    };
    Ok(format!("https://{with_port}"))
}

/// Whether the cell answers at all.
///
/// `/healthz` needs no token and no certificate anybody trusts, which makes it
/// exactly the right question to ask first: it separates "the network is not
/// up", "that is the wrong address" and "the API is not running" from anything
/// about enrolment.
fn reachable(url: &str) -> Result<()> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "--insecure",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("{url}/healthz"),
        ])
        .output()
        .context("running curl — the installer medium carries it")?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let said = said.trim();
    bail!(
        "{url} did not answer: {}\n  Check that this machine has an address (ip -brief addr), \
         that the cell is that one, and that its console opens in a browser from somewhere \
         else.",
        if said.is_empty() {
            "no reason given".to_string()
        } else {
            said.to_string()
        }
    )
}

fn keypair() -> Result<(ring::signature::Ed25519KeyPair, String)> {
    use ring::signature::KeyPair;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("this machine has no usable source of randomness"))?;
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow::anyhow!("could not make a keypair"))?;
    let public = base64(pair.public_key().as_ref());
    Ok((pair, public))
}

/// Standard base64, the one spelling the API canonicalises to anyway.
fn base64(raw: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let index = (n >> (18 - 6 * i)) & 0x3f;
                out.push(ALPHABET[index as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Say hello, and learn what certificate answered.
fn announce(url: &str, public: &str, hostname: &str) -> Result<(serde_json::Value, String)> {
    let body = serde_json::json!({
        "publicKey": public,
        "reported": reported(hostname),
    });
    // The fingerprint of what answered has to be recorded *before* it is
    // reported, so it is computed here and sent in a second call. One request
    // would need the certificate the request is being made over, which is
    // knowledge the request does not have yet.
    let (answer, served) = post(url, "enrollments:announce", &body)?;
    let told = serde_json::json!({
        "publicKey": public,
        "seenCertificate": served,
        "reported": reported(hostname),
    });
    let (answer, _) = post(url, "enrollments:announce", &told).unwrap_or((answer, served.clone()));
    Ok((answer, served))
}

enum Claimed {
    Token(JoinToken),
    Waiting,
    No(String),
}

fn claim(url: &str, id: &str, pair: &ring::signature::Ed25519KeyPair) -> Claimed {
    let message = velstra_cloud_wire::join::claim_message(id);
    let signature = base64(pair.sign(&message).as_ref());
    let body = serde_json::json!({ "id": id, "signature": signature });
    let Ok((answer, _)) = post(url, "enrollments:claim", &body) else {
        // A request that did not go through at all: the cell may be
        // restarting. Keep asking — this is a loop, and a machine that gave up
        // on one refused connection would be a machine somebody has to walk
        // back to.
        return Claimed::Waiting;
    };
    if let Some(token) = answer["joinToken"].as_str() {
        return match JoinToken::decode(token) {
            Ok(token) => Claimed::Token(token),
            Err(e) => Claimed::No(format!(
                "the cell sent a join token this build cannot read: {e}"
            )),
        };
    }
    let code = answer["error"]["code"].as_str().unwrap_or_default();
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    match code {
        // Not yet. This is the ordinary case and the reason there is a loop.
        "FAILED_PRECONDITION" if message.contains("approved this machine yet") => Claimed::Waiting,
        "" => Claimed::Waiting,
        // Everything else is an answer: turned away, expired, approved without
        // being named. Each says what to do about it, so it is printed as it
        // came rather than translated.
        _ => Claimed::No(message.to_string()),
    }
}

/// One POST, and the certificate that answered it.
fn post(url: &str, verb: &str, body: &serde_json::Value) -> Result<(serde_json::Value, String)> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "20",
            // The certificate cannot be verified: this machine has no CA and
            // is asking for one. What replaces verification is the fingerprint
            // printed on this screen, which is the whole design.
            "--insecure",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-w",
            "\n%{certs}",
            "-d",
            &body.to_string(),
            &format!("{url}/api/v1/{verb}"),
        ])
        .output()
        .context("running curl — the installer medium carries it")?;
    // curl's own complaint, said out loud.
    //
    // This was thrown away, and it cost somebody an evening: a request that
    // never reached the cell leaves stdout empty, which parsed as no JSON,
    // which read as an answer with no enrolment id — so "I cannot reach
    // 10.10.10.8" was reported as "the cell's answer names no enrolment id".
    // The machine had said exactly what was wrong on stderr and this function
    // dropped it. Whatever else is true of a door nobody has walked through
    // yet, it has to be able to say why it did not open.
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.trim();
        bail!(
            "could not reach {url}: {}",
            if said.is_empty() {
                format!("curl gave up with status {:?}", out.status.code())
            } else {
                said.to_string()
            }
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // The body is everything before the certificate block curl appends.
    let (body_text, certs) = match text.find("\nSubject:") {
        Some(at) => (&text[..at], &text[at..]),
        None => (text.as_ref(), ""),
    };
    let answer: serde_json::Value = serde_json::from_str(body_text.trim()).unwrap_or_else(
        |_| serde_json::json!({ "error": { "code": "", "message": body_text.trim() } }),
    );
    let served = crate::tls::fingerprint_of_pem(certs).unwrap_or_default();
    Ok((answer, served))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a person types, and what it has to mean.
    #[test]
    fn an_address_may_be_typed_the_way_people_type_one() {
        assert_eq!(cell_url("10.10.10.8").unwrap(), "https://10.10.10.8:8443");
        assert_eq!(
            cell_url("10.10.10.8:9443").unwrap(),
            "https://10.10.10.8:9443"
        );
        assert_eq!(
            cell_url("https://cell-1:8443/").unwrap(),
            "https://cell-1:8443"
        );
        assert_eq!(cell_url("cell-1").unwrap(), "https://cell-1:8443");
        // A bracketed v6 address with a port, which is what somebody copying
        // from the banner has in front of them.
        assert_eq!(
            cell_url("[fd00::8]:8443").unwrap(),
            "https://[fd00::8]:8443"
        );
    }

    /// Plain HTTP is said to be wrong rather than tried and refused by the
    /// cell, because the reason is on this side.
    #[test]
    fn plain_http_is_refused_with_the_answer() {
        let e = cell_url("http://10.10.10.8:8443").unwrap_err().to_string();
        assert!(e.contains("10.10.10.8:8443"), "{e}");
        assert!(cell_url("").is_err());
        assert!(cell_url("10.10.10.8 8443").is_err());
    }

    /// The claim message is the API's, byte for byte. Two spellings of it
    /// would be a machine that signs something the cell does not check.
    #[test]
    fn the_claim_message_is_the_wire_crate_s() {
        // Not spelled here at all: the installer carries the wire crate, which
        // is where the format lives, and `velstra-cloud-model` pins its own
        // copy against the same function.
        assert_eq!(
            velstra_cloud_wire::join::claim_message("m-1a2b3c4d5e6f"),
            b"velstra-enrollment-claim:v1:m-1a2b3c4d5e6f".to_vec()
        );
    }

    /// The base64 here is standard base64, which is what the API
    /// canonicalises to — checked against known values rather than against
    /// itself.
    #[test]
    fn the_base64_is_the_ordinary_one() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(&[0xff, 0xef, 0xfe]), "/+/+");
    }

    /// Firmware that says nothing useful is treated as saying nothing, rather
    /// than putting "To be filled by O.E.M." on an operator's screen as if it
    /// identified the machine.
    #[test]
    fn a_useless_serial_is_no_serial() {
        for raw in ["To be filled by O.E.M.", "Default string", "None", "0"] {
            assert!(
                dmi_from(raw).is_empty(),
                "{raw} should not be shown as a serial"
            );
        }
        assert_eq!(dmi_from("PT-0042"), "PT-0042");
    }
}
