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
    setup::{Machine, SEED_DIR},
    wizard::{prompt, prompt_secret, validate_node_name},
};

/// How long to wait for the API to answer after enabling it.
///
/// Generous, because the first start also brings up etcd and writes the first
/// administrator, and a timeout that fires while that is happening would send
/// somebody to debug a cell that was about to work.
const API_WAIT_SECS: u64 = 90;

pub fn run(dir: Option<PathBuf>, listen: Option<String>, node: Option<String>) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from(SEED_DIR));

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
        &dir,
        &crate::wizard::hostname(),
        &addresses,
    )?);
    if let Some(cert) = &tls {
        say(if cert.made {
            "made a certificate for this machine"
        } else {
            "kept the certificate that was already here"
        });
    }

    let machine = Machine {
        lvm_group: String::new(),
        lvm_thin_pool: String::new(),
        ceph_conf: String::new(),
        ceph_user: String::new(),
        ceph_pool: String::new(),
        ceph_image_pool: String::new(),
        region: "eu-central".into(),
        cell: "cell-1".into(),
        roles: vec![Role::ControlPlane, Role::Hypervisor, Role::Pool],
        api_url: if tls.is_some() {
            format!("https://localhost:{}", listen.rsplit(':').next().unwrap_or("8443"))
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

    // The seed first: every unit below is conditional on a role being in it, so
    // enabling anything before it exists would enable something that skips.
    crate::setup::write_seed(&dir, &machine)?;
    say("wrote the seed");

    // The control plane, so there is something to create objects in. The node
    // and pool agents come last, once they have objects to claim.
    match crate::setup::settle_etcd() {
        Ok(true) => say("gave etcd room to keep working"),
        Ok(false) => {}
        Err(e) => say(&format!("could not configure etcd ({e}); its defaults will do for now")),
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

    println!("\nDone. The console is at {}", browsable(&listen, tls.is_some()));
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
        println!("To use a real certificate instead, put it at {} and its", cert.cert.display());
        println!("key at {}, then restart velstra-cloud-api.", cert.key.display());
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

fn say(what: &str) {
    println!("  · {what}");
}

/// The address to *talk* to, which is not the address it binds.
///
/// `0.0.0.0` is a bind, never a destination: connecting to it works on Linux by
/// accident and is wrong to print at somebody.
fn local_api(listen: &str, tls: bool) -> String {
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
fn curl(args: &[&str]) -> Result<String> {
    let mut base: Vec<String> = vec!["-sS".into(), "--max-time".into(), "20".into()];
    // Against the cell's own certificate, when there is one. `-k` would also
    // work and would also teach every reader of this script that verification
    // is optional; the CA file is right there and pinning it costs one flag.
    if let Ok(ca) = std::env::var("VELSTRA_QUICKSTART_CA") {
        if !ca.is_empty() {
            base.extend(["--cacert".into(), ca]);
        }
    }
    let out = Command::new("curl")
        .args(&base)
        .args(args)
        .output()
        .context("running curl — the package depends on it")?;
    if !out.status.success() {
        bail!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn wait_for(api: &str) -> Result<()> {
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

/// One field out of a JSON object, without a JSON parser.
///
/// Deliberately crude and deliberately narrow: these are two responses this
/// same codebase produces, the fields are flat strings, and a dependency added
/// for six characters of parsing is a dependency in every future audit.
fn field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn api_token(api: &str, user: &str, password: &str) -> Result<String> {
    let body = curl(&[
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-d",
        &format!("{{\"username\":\"{user}\",\"password\":\"{password}\"}}"),
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

fn ensure_node(api: &str, token: &str, id: &str, dir: &std::path::Path) -> Result<()> {
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
    let Some(node_token) = field(&body, "nodeToken") else {
        bail!(
            "creating the node {id} did not hand back a token: {}. It is shown exactly once, so \
             if the node already exists, delete it and run this again",
            body.trim()
        );
    };
    crate::setup::write_secret(&token_file, &node_token)?;
    say("created the node and wrote its one-time token");
    Ok(())
}

fn ensure_pool(api: &str, token: &str, id: &str) -> Result<()> {
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
