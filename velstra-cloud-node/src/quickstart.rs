//! One box, one command, from a fresh package to a cell you can sign into.
//!
//! ## Why this exists beside `setup`
//!
//! [`crate::setup`] answers "what is this machine", which is one of the three
//! things a working cell needs. The other two are objects in the cell — a Node
//! and a Pool — and a credential moved between them. Doing that by hand is six
//! steps, and every one of them was a place this platform lost somebody:
//! creating the node, copying a token that is shown exactly once, writing it
//! with the right mode, restarting the agent, creating the pool, and finding
//! out afterwards that the API had bound to loopback.
//!
//! None of that is hard. All of it is the difference between "I tried it" and
//! "I gave up", and none of it is interesting to anybody who just wants a
//! virtual machine on the laptop under their desk.
//!
//! ## What it does not do
//!
//! It does not invent a cell on a machine that is already in one, and it does
//! not touch NixOS. Units there are a declaration; a command that enabled them
//! behind the operator's back would be fighting the operating system, which is
//! the same reason `setup` prints a module snippet instead.
//!
//! ## Every step is idempotent
//!
//! Not politeness — it is what makes this usable at all. A quickstart that
//! failed at step five and could not be run again would leave a half-built cell
//! and a person with no way forward but to reinstall. So a node that exists is
//! not created twice, a token that is already on disk is not re-issued, and a
//! unit that is running is left running.

use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    roles::Role,
    setup::{self, Machine, SEED_DIR},
    wizard::{prompt, prompt_secret, validate_node_name},
};

/// How long to wait for the API to answer after enabling it.
///
/// Generous, because the first start also brings up etcd and writes the first
/// administrator, and a timeout that fires while that is happening would send
/// somebody to debug a cell that was about to work.
const API_WAIT_SECS: u64 = 90;

pub fn run(dir: Option<PathBuf>, listen: Option<String>, node: Option<String>) -> Result<()> {
    // Two directories, because they answer to different owners. The state
    // directory holds what the machine *has* — its certificate, its guests —
    // and may be a filesystem every machine in the cell mounts. The identity
    // directory holds who the machine *is*, and must not be.
    //
    // An explicit `--dir` overrides both: a test that redirects the state
    // directory and finds the seed still landing in the real `/etc` is a test
    // that writes to the machine running it.
    let state = dir.clone().unwrap_or_else(|| PathBuf::from(SEED_DIR));
    let dir = dir.unwrap_or_else(|| PathBuf::from(setup::IDENTITY_DIR));

    if std::path::Path::new("/etc/NIXOS").exists() {
        bail!(
            "this is NixOS, where units are a declaration rather than something a command \
             enables. Use the module — `velstra.cloud.controlPlane`, `node` and `pool` — and \
             `velstra-cloud-node setup` to write the seed beside it; see docs/setup-guide.md §0"
        );
    }
    if which("systemctl").is_none() {
        bail!("no systemctl on this machine: this command brings units up, and there are none");
    }

    println!("Velstra Cloud — one machine, the whole cell\n");
    println!("This box will be the control plane, a hypervisor and a storage pool at once.");
    println!(
        "It writes {}/node.env, brings the units up, and creates the two",
        dir.display()
    );
    println!("objects a cell needs before a guest can run. Everything it does can be run");
    println!("again: nothing here is created twice.\n");

    let node_id = match node {
        Some(id) => id,
        None => {
            // Its hostname, not a name this installer made up. `home-1` was a
            // laboratory name in a product that is meant to run somebody's
            // estate, and a default nobody chose is one that ends up on real
            // machines because it was there.
            let suggestion = crate::wizard::suggested_node_name(&crate::wizard::hostname());
            let question = match &suggestion {
                Some(name) => format!("A name for this machine [{name}]: "),
                None => "A name for this machine: ".to_string(),
            };
            crate::wizard::ask_valid_or(
                suggestion.as_deref().unwrap_or(""),
                &question,
                validate_node_name,
                "lowercase letters, digits and '-'",
            )?
        }
    };

    let listen = match listen {
        Some(l) => l,
        None => {
            println!("\nWho should be able to reach the console?");
            println!("  [1] only this machine — right for a laptop, and the default");
            println!("  [2] anything that can reach this machine over the network");
            println!("\nThis machine makes itself a certificate either way, so the console");
            println!("is served over TLS. Its fingerprint is printed at the end.");
            loop {
                match prompt("Reachable from [1]: ")?.trim() {
                    "" | "1" => break "127.0.0.1:8443".to_string(),
                    "2" => break "0.0.0.0:8443".to_string(),
                    other => println!("  {other:?} is not a choice — 1 or 2."),
                }
            }
        }
    };

    // Unattended when the environment carries it, asked otherwise. The same
    // convention `setup --config` already uses, and the reason is the same: a
    // password on a command line is in `ps` for every user on the machine, and
    // one that can only be typed makes this command useless to the config
    // management that would run it on fifty boxes.
    let admin_password = if let Ok(from_env) = std::env::var("VELSTRA_BOOTSTRAP_PASSWORD") {
        println!("\nTaking the administrator's password from VELSTRA_BOOTSTRAP_PASSWORD.");
        from_env
    } else {
        ask_for_a_password()?
    };

    // Before the seed, because the seed names the files. A machine that cannot
    // make one is not a machine that should serve a password in plaintext
    // instead — it is one whose operator has to be told, so the failure is
    // reported and the install stops.
    let addresses: Vec<String> = if listen.starts_with("0.0.0.0") {
        Vec::new()
    } else {
        vec![listen.rsplit(':').nth(1).unwrap_or("127.0.0.1").to_string()]
    };
    let tls = Some(crate::tls::ensure(
        &state,
        &crate::wizard::hostname(),
        &addresses,
    )?);
    // What a joining machine is told to try, in order: this machine's own
    // addresses first, then its name. The same list the certificate was just
    // made with, so every URL here is one it verifies for — which is the whole
    // reason the API is *told* this rather than left to work it out.
    let port = listen.rsplit(':').next().unwrap_or("8443");
    let advertise = crate::tls::advertise_urls(&crate::wizard::hostname(), &addresses, port);
    if let Some(cert) = &tls {
        say(if cert.made {
            "made a certificate for this machine"
        } else {
            "kept the certificate that was already here"
        });
    }

    let machine = Machine {
        // Empty: this box already runs an operating system and already has a
        // name. Only the installer, seeding a filesystem that has never
        // booted, answers this.
        hostname: String::new(),
        // The certificate is a file on this machine, named by path below.
        api_ca_pem: String::new(),
        advertise: advertise.clone(),
        bootstrap_ceph_osds: Vec::new(),
        // A Debian box already has accounts and an ssh the operator owns;
        // this command does not reach into either.
        ssh_key: String::new(),
        root_password: String::new(),
        passthrough: String::new(),
        // One machine that is the whole cell: its pool agent reaches the store
        // directly, and is deliberately given no token.
        pool_token: String::new(),
        lvm_group: String::new(),
        lvm_thin_pool: String::new(),
        ceph_conf: String::new(),
        ceph_user: String::new(),
        ceph_pool: String::new(),
        ceph_image_pool: String::new(),
        ceph_pool_id: String::new(),
        region: "eu-central".into(),
        cell: "cell-1".into(),
        roles: vec![Role::ControlPlane, Role::Hypervisor, Role::Pool],
        api_url: if tls.is_some() {
            format!(
                "https://localhost:{}",
                listen.rsplit(':').next().unwrap_or("8443")
            )
        } else {
            "http://127.0.0.1:8443".into()
        },
        api_ca: tls
            .as_ref()
            .map(|c| c.cert.display().to_string())
            .unwrap_or_default(),
        node: node_id.clone(),
        token: String::new(),
        vmm: if which("qemu-system-x86_64").is_some() {
            "qemu"
        } else {
            "fake"
        }
        .into(),
        pool: "local".into(),
        pool_backend: "directory".into(),
        store: "127.0.0.1:2379".into(),
        listen: listen.clone(),
        tls_cert: tls
            .as_ref()
            .map(|c| c.cert.display().to_string())
            .unwrap_or_default(),
        tls_key: tls
            .as_ref()
            .map(|c| c.key.display().to_string())
            .unwrap_or_default(),
        cells: Vec::new(),
        fabric: None,
        // A home cell has no fabric, so this node is the far end of every wire
        // its guests are on. Without it the guest boots, reports Running, and
        // can be reached and logged into by nobody — see `localnet`.
        local_network: true,
        admin: "admin".into(),
        admin_password,
    };

    let existing_seed = dir.join("node.env");
    if existing_seed.exists() {
        let previous = setup::parse(&std::fs::read_to_string(&existing_seed)?)?;
        if previous.node != machine.node
            || previous.cell != machine.cell
            || previous.region != machine.region
        {
            bail!(
                "this machine already belongs to {}/{}/{}; quickstart will not replace its identity",
                previous.region,
                previous.cell,
                previous.node
            );
        }
    }

    // The seed first: every unit below is conditional on a role being in it, so
    // enabling anything before it exists would enable something that skips.
    crate::setup::write_seed(&dir, &machine)?;
    say("wrote the seed");

    // The control plane, so there is something to create objects in. The node
    // and pool agents come last, once they have objects to claim.
    match crate::setup::settle_etcd() {
        Ok(true) => say("gave etcd room to keep working"),
        Ok(false) => {}
        Err(e) => say(&format!(
            "could not configure etcd ({e}); its defaults will do for now"
        )),
    }
    enable(&["etcd", "velstra-cloud-api", "velstra-cloud-controller"])?;
    say("brought up etcd and the control plane");

    let api = local_api(&listen, tls.is_some());
    if let Some(cert) = &tls {
        // For this process's own curl calls, and for nothing else.
        // The agents get the same path through the seed, as VELSTRA_API_CA.
        unsafe { std::env::set_var("VELSTRA_QUICKSTART_CA", cert.cert.display().to_string()) };
    }
    wait_for(&api)?;
    say("the API is answering");

    let token = api_token(&api, "admin", &machine.admin_password)?;
    ensure_node(&api, &token, &node_id, &dir)?;
    ensure_pool(&api, &token, "local")?;

    enable(&["velstra-cloud-nodeagent", "velstra-cloud-poolagent"])?;
    say("brought up the node and pool agents");

    println!(
        "\nDone. The console is at {}",
        browsable(&listen, tls.is_some())
    );
    if let Some(cert) = &tls {
        // Printed once, here, on the machine's own console. A browser will warn
        // about this certificate — correctly, because nobody it trusts signed it
        // — and the warning is only worth anything to somebody who can check
        // what they are agreeing to. This is that one chance.
        println!();
        println!("It serves TLS with a certificate this machine made for itself.");
        println!("Your browser will warn. Before clicking past it, check that the");
        println!("fingerprint it shows is this one:");
        println!();
        println!("  {}", cert.fingerprint);
        println!();
        println!(
            "To use a real certificate instead, put it at {} and its",
            cert.cert.display()
        );
        println!(
            "key at {}, then restart velstra-cloud-api.",
            cert.key.display()
        );
    }
    // Not "the password you just chose": on an unattended run nobody chose
    // anything here, and a closing line that describes a conversation that did
    // not happen is the kind of small untruth that makes a reader distrust the
    // rest of the output.
    println!("Sign in as `admin`.\n");
    println!("The node will appear with its capacity within a pass — that first status");
    println!("report is the registration working. Then: Images → New image, and a guest.");
    if machine.vmm == "fake" {
        println!("\nNo QEMU on this machine, so the seed says `fake`: guests will be recorded");
        println!("and not run. `apt install qemu-system-x86 qemu-utils`, set VELSTRA_VMM=qemu");
        println!(
            "in {}/node.env, and restart velstra-cloud-nodeagent.",
            dir.display()
        );
    }
    Ok(())
}

pub(crate) fn say(what: &str) {
    println!("  · {what}");
}

/// The address to *talk* to, which is not the address it binds.
///
/// `0.0.0.0` is a bind, never a destination: connecting to it works on Linux by
/// accident and is wrong to print at somebody.
pub(crate) fn local_api(listen: &str, tls: bool) -> String {
    let port = listen.rsplit(':').next().unwrap_or("8443");
    let scheme = if tls { "https" } else { "http" };
    // `localhost` and not `127.0.0.1`, because the certificate names hostnames
    // and the bare address only as a subject-alternative — and curl matches
    // what was typed. Both are in the certificate; the name is the safer bet on
    // a machine whose resolver is untouched.
    format!("{scheme}://localhost:{port}/api/v1")
}

fn browsable(listen: &str, tls: bool) -> String {
    let port = listen.rsplit(':').next().unwrap_or("8443");
    let scheme = if tls { "https" } else { "http" };
    if listen.starts_with("0.0.0.0") {
        format!("{scheme}://<this machine>:{port}/")
    } else {
        format!("{scheme}://127.0.0.1:{port}/")
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
    })
}

fn enable(units: &[&str]) -> Result<()> {
    for unit in units {
        // `enable --now` on a unit that is already running is a no-op, which is
        // what makes re-running this whole command safe.
        let out = Command::new("systemctl")
            .args(["enable", "--now", unit])
            .output()
            .with_context(|| format!("running systemctl enable {unit}"))?;
        if !out.status.success() {
            bail!(
                "could not enable {unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// `curl`, for the same reason every other outside tool here is a command: this
/// binary is the installer, and giving it an HTTP stack would give the installer
/// a TLS stack, a certificate store, and their upgrades.
pub(crate) fn curl(args: &[&str]) -> Result<String> {
    let mut base: Vec<String> = vec!["-sS".into(), "--max-time".into(), "20".into()];
    // Against the cell's own certificate, when there is one. `-k` would also
    // work and would also teach every reader of this script that verification
    // is optional; the CA file is right there and pinning it costs one flag.
    if let Ok(ca) = std::env::var("VELSTRA_QUICKSTART_CA") {
        if !ca.is_empty() {
            base.extend(["--cacert".into(), ca]);
        }
    }
    use std::{io::Write, process::Stdio};
    let (public, config) = curl_input(args)?;
    let mut child = Command::new("curl")
        .args(&base)
        .args(&public)
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running curl — the package depends on it")?;
    child
        .stdin
        .take()
        .context("curl has no stdin")?
        .write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub(crate) fn wait_for(api: &str) -> Result<()> {
    for _ in 0..API_WAIT_SECS {
        if curl(&["-o", "/dev/null", "-w", "%{http_code}", api])
            .is_ok_and(|c| c.starts_with('2') || c.starts_with('4'))
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    bail!(
        "the API did not answer within {API_WAIT_SECS}s. `journalctl -u velstra-cloud-api` \
         says why; the usual answer is that etcd is not up"
    )
}

/// Sensitive headers and request bodies go through stdin, never argv.
fn curl_input(args: &[&str]) -> Result<(Vec<String>, String)> {
    let mut public = Vec::new();
    let mut config = String::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let option = match *arg {
            "-H" | "--header" => Some("header"),
            "-d" | "--data" | "--data-raw" => Some("data-raw"),
            _ => None,
        };
        if let Some(option) = option {
            let value = args.next().context("missing curl option value")?;
            config.push_str(&format!("{option} = {}\n", serde_json::to_string(value)?));
        } else {
            public.push((*arg).to_string());
        }
    }
    Ok((public, config))
}

pub(crate) fn field(body: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn api_token(api: &str, user: &str, password: &str) -> Result<String> {
    let body = curl(&[
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        &serde_json::json!({"username": user, "password": password}).to_string(),
        &format!("{api}/sessions"),
    ])?;
    field(&body, "token").ok_or_else(|| {
        anyhow::anyhow!(
            "signing in as {user} did not work: {}. If this cell already had an administrator, \
             the password in the seed is not used — a bootstrap never resets a live one",
            body.trim()
        )
    })
}

pub(crate) fn ensure_node(api: &str, token: &str, id: &str, dir: &std::path::Path) -> Result<()> {
    let token_file = dir.join("node-token");
    if token_file.exists() {
        say("the node already has its token");
        return Ok(());
    }
    let body = curl(&[
        "-X",
        "POST",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-H",
        "Content-Type: application/json",
        "-d",
        &format!("{{\"id\":\"{id}\",\"spec\":{{\"schedulable\":true}}}}"),
        &format!("{api}/nodes"),
    ])?;
    let node_token = match field(&body, "nodeToken") {
        Some(token) => token,
        None => {
            let response: serde_json::Value = serde_json::from_str(&body)?;
            if response["error"]["code"] != "ALREADY_EXISTS" {
                bail!("creating node {id} failed: {}", body.trim());
            }
            // The seed written by this quickstart names this exact node. A
            // previous run may have created it before losing its response.
            let issued = curl(&[
                "-f",
                "-X",
                "POST",
                "-H",
                &format!("Authorization: Bearer {token}"),
                "-H",
                "Content-Type: application/json",
                "-d",
                "{\"purpose\":\"quickstart recovery\"}",
                &format!("{api}/nodes/{id}:issueCredential"),
            ])?;
            field(&issued, "nodeToken").context("credential recovery returned no nodeToken")?
        }
    };
    crate::setup::write_secret(&token_file, &node_token)?;
    say("created the node and wrote its one-time token");
    Ok(())
}

pub(crate) fn ensure_pool(api: &str, token: &str, id: &str) -> Result<()> {
    let body = curl(&[
        "-X",
        "POST",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-H",
        "Content-Type: application/json",
        "-d",
        &format!("{{\"id\":\"{id}\",\"spec\":{{\"accepting\":true}}}}"),
        &format!("{api}/pools"),
    ])?;
    // A pool that is already there is the answer this wants, not an error: the
    // whole command has to survive being run twice.
    if body.contains("ALREADY_EXISTS") {
        say("the pool is already there");
    } else {
        say("created the storage pool");
    }
    Ok(())
}

/// The password, typed twice, when nobody handed one in.
fn ask_for_a_password() -> Result<String> {
    println!("\nThe administrator you will sign in as. There is no default password:");
    println!("a platform that ships one ships a way in.");
    loop {
        let first = prompt_secret("Password: ")?;
        if first.trim().len() < 12 {
            println!("  at least 12 characters — this one credential is the way into everything");
            continue;
        }
        if prompt_secret("Repeat it: ")? != first {
            println!("  they do not match");
            continue;
        }
        break Ok(first);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn curl_secrets_never_appear_in_the_argument_list() {
        let password = "a\"b\\c\nline";
        let body = serde_json::json!({"username": "admin", "password": password}).to_string();
        let (args, config) = curl_input(&[
            "-X",
            "POST",
            "-H",
            "Authorization: Bearer secret",
            "-d",
            &body,
            "https://localhost",
        ])
        .unwrap();
        assert!(!args.join(" ").contains("secret"));
        assert!(!args.join(" ").contains("password"));
        let data = config
            .lines()
            .find_map(|line| line.strip_prefix("data-raw = "))
            .unwrap();
        let decoded: String = serde_json::from_str(data).unwrap();
        assert_eq!(field(&decoded, "password").as_deref(), Some(password));
    }
    #[test]
    fn quickstart_authenticates_escaped_passwords_and_recovers_a_lost_node_token() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let password = "password-with-\"quotes\\and-slashes";
        let server = std::thread::spawn(move || {
            for (path, response) in [
                ("/api/v1/sessions", r#"{"token":"session-secret"}"#),
                ("/api/v1/nodes", r#"{"error":{"code":"ALREADY_EXISTS"}}"#),
                (
                    "/api/v1/nodes/recover:issueCredential",
                    r#"{"nodeToken":"recovered-secret"}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(
                    headers.starts_with(&format!("POST {path} HTTP/1.1")),
                    "{headers}"
                );
                let len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_string)
                    })
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                if path.ends_with("sessions") {
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["password"],
                        password
                    );
                } else {
                    assert!(headers.contains("Authorization: Bearer session-secret"));
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
        });
        let dir =
            std::env::temp_dir().join(format!("velstra-quickstart-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let token = api_token(&api, "admin", password).unwrap();
        ensure_node(&api, &token, "recover", &dir).unwrap();
        server.join().unwrap();
        ensure_node(&api, &token, "recover", &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("node-token"))
                .unwrap()
                .trim(),
            "recovered-secret"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("node-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
