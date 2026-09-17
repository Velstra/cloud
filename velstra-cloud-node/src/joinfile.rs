//! Finding a join token on something plugged into the machine.
//!
//! ## Why this exists
//!
//! A join token is a little over a thousand characters of base64. It solved
//! the problem it was designed for — carrying six facts from the control plane
//! without copying a certificate by hand — and it solved it for a machine that
//! could *paste*. Proxmox gets away with a blob because the thing on the other
//! end is a browser. An installer on bare metal has a console and a keyboard,
//! and a thousand characters typed by hand is not a hand-off, it is a
//! punishment with a rationale.
//!
//! So: write the ISO once, drop one text file on a second stick, and the
//! wizard finds it. Nothing about the protocol changes, nothing about the
//! security story changes — the token is exactly as secret on a stick as it is
//! in a terminal's scrollback, and it stays self-contained, so the install
//! still needs nothing to be reachable.
//!
//! ## What it looks at
//!
//! Every partition the kernel knows, mounted read-only for as long as it takes
//! to read one file, and unmounted again. The install medium itself is
//! included on purpose: a USB stick written with the ISO usually has room
//! after it, and putting the token there means one stick instead of two.
//!
//! Read-only, `nosuid`, `nodev`, `noexec`: this is a filesystem somebody else
//! wrote, handed to a program running as root, before anybody has been asked
//! anything. It is read for one small text file and nothing else.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use velstra_cloud_wire::join::JoinToken;

/// Where the wizard looks, in order. Relative to the root of each filesystem.
///
/// `velstra/join` first because it is the one that says what it is on a stick
/// somebody else will pick up in a year; the rest are what a person actually
/// types when they are in a hurry.
const NAMES: [&str; 5] = [
    "velstra/join",
    "velstra-join.txt",
    "join.txt",
    "velstra/join.txt",
    "join",
];

/// The largest file this will read. A join token is about 1 KiB; anything much
/// larger is not one, and reading it would be reading whatever somebody left
/// on a stick.
const MAX_BYTES: u64 = 64 * 1024;

/// A token found on a medium, and where it was.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The device it came off, as the kernel names it — `/dev/sdb1`.
    pub device: String,
    /// The path inside that filesystem.
    pub file: String,
    /// The token itself.
    pub token: JoinToken,
}

impl Found {
    /// One line for the wizard: which machine this token is for, and where it
    /// came from. The node id is the useful half — a stick can carry the wrong
    /// one, and "peter" is how somebody notices.
    pub fn describe(&self) -> String {
        format!(
            "{} for {} in cell {} (on {})",
            self.file, self.token.node, self.token.cell, self.device
        )
    }
}

/// Every join token on every filesystem this machine can see.
///
/// Never fails: a partition that will not mount, a filesystem the kernel has
/// no driver for, a file that is not a token — all of them are "no token
/// here". The wizard falls back to asking, which is what it did before.
pub fn find() -> Vec<Found> {
    let mut out = Vec::new();
    for device in partitions() {
        if let Some(found) = scan(&device) {
            out.push(found);
        }
    }
    out
}

/// The partitions the kernel knows, as `/dev/...` paths.
fn partitions() -> Vec<String> {
    let Ok(text) = Command::new("lsblk")
        .args(["-rno", "PATH,TYPE"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    else {
        return Vec::new();
    };
    parse_partitions(&text)
}

/// Pick the partitions out of `lsblk -rno PATH,TYPE`.
///
/// Partitions only. A whole disk with a filesystem directly on it is a thing
/// that exists, and mounting every disk to look for a text file is a good way
/// to touch the install target seconds before erasing it.
fn parse_partitions(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let path = f.next()?;
            let kind = f.next()?;
            (kind == "part").then(|| path.to_string())
        })
        .collect()
}

/// Mount one partition read-only, look for a token, unmount.
fn scan(device: &str) -> Option<Found> {
    let at = PathBuf::from("/run/velstra-join-scan");
    let _ = std::fs::create_dir_all(&at);
    // `noload` first, then without it.
    //
    // `mount -o ro` is not "do not write": on a dirty ext4 the kernel replays
    // the journal, which writes to a disk somebody else's operating system
    // owns, before this installer has asked anybody anything. `noload` says
    // do not — and is rejected by vfat, which is what the carrier usually is,
    // so the plain form is the fallback rather than the default.
    //
    // No filesystem type either way: the kernel picks, and one it cannot pick
    // is one this returns nothing for.
    let mounted = ["ro,nosuid,nodev,noexec,noload", "ro,nosuid,nodev,noexec"]
        .iter()
        .any(|opts| {
            Command::new("mount")
                .args(["-o", opts, device])
                .arg(&at)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
    if !mounted {
        return None;
    }
    let found = first_token(&at).map(|(file, token)| Found {
        device: device.to_string(),
        file,
        token,
    });
    let _ = Command::new("umount").arg(&at).output();
    found
}

/// The first of [`NAMES`] under `root` that holds a token.
fn first_token(root: &Path) -> Option<(String, JoinToken)> {
    for name in NAMES {
        let path = root.join(name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(token) = token_in(&text) {
            return Some((name.to_string(), token));
        }
    }
    None
}

/// The first join token in a text file, whatever else is in it.
///
/// Line-wise rather than whole-file, so a stick can carry a file with a
/// comment above the token — which is what somebody writing one by hand does,
/// and refusing it would be refusing the obvious thing for no reason. Wrapped
/// tokens still work: `decode` strips whitespace, so a token split over lines
/// is found by trying the rest of the file from each line that starts one.
pub fn token_in(text: &str) -> Option<JoinToken> {
    // The common case: one line, or one line among comments.
    for line in text.lines() {
        if let Ok(t) = JoinToken::decode(line) {
            return Some(t);
        }
    }
    // A token somebody's editor wrapped: take everything from the first line
    // that looks like a start, joined.
    let start = text.find("velstra1.")?;
    JoinToken::decode(&text[start..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_token() -> String {
        velstra_cloud_wire::join::JoinToken {
            v: 1,
            region: "eu-central".into(),
            cell: "cell-1".into(),
            node: "peter".into(),
            urls: vec!["https://10.10.10.8:8443".into()],
            ca: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
            token: "ab".repeat(32),
            pool: None,
        }
        .encode()
    }

    #[test]
    fn a_file_holding_only_the_token() {
        let t = token_in(&a_token()).expect("a bare token");
        assert_eq!(t.node, "peter");
    }

    /// What a person actually writes on a stick: a line saying what it is,
    /// then the token. Refusing that would be refusing the obvious thing.
    #[test]
    fn a_token_under_a_comment() {
        let text = format!("# peter, cell-1, minted 2026-09-17\n{}\n", a_token());
        assert_eq!(token_in(&text).expect("found").node, "peter");
    }

    /// An editor wrapped it. `decode` strips whitespace, so the whole tail of
    /// the file is tried once no single line parses.
    #[test]
    fn a_token_an_editor_wrapped() {
        let t = a_token();
        let wrapped: String = t
            .as_bytes()
            .chunks(72)
            .map(|c| format!("{}\n", String::from_utf8_lossy(c)))
            .collect();
        assert_eq!(token_in(&wrapped).expect("found").node, "peter");
    }

    #[test]
    fn a_file_with_no_token_in_it() {
        assert!(token_in("notes to self\nnothing here\n").is_none());
        assert!(token_in("").is_none());
    }

    /// Partitions, not whole disks: mounting the install target to look for a
    /// text file, seconds before erasing it, is not a thing to do.
    #[test]
    fn only_partitions_are_looked_at() {
        let lsblk = "/dev/sda disk\n\
                     /dev/sda1 part\n\
                     /dev/sda2 part\n\
                     /dev/nvme0n1 disk\n\
                     /dev/loop0 loop\n\
                     /dev/sr0 rom\n";
        assert_eq!(
            parse_partitions(lsblk),
            vec!["/dev/sda1".to_string(), "/dev/sda2".to_string()]
        );
    }

    /// The line the wizard shows names the machine the token is for, because a
    /// stick can carry the wrong one and that is how somebody notices.
    #[test]
    fn the_description_names_the_machine_and_the_medium() {
        let found = Found {
            device: "/dev/sdb1".into(),
            file: "velstra/join".into(),
            token: JoinToken::decode(&a_token()).unwrap(),
        };
        let line = found.describe();
        assert!(line.contains("peter"), "{line}");
        assert!(line.contains("cell-1"), "{line}");
        assert!(line.contains("/dev/sdb1"), "{line}");
    }
}
