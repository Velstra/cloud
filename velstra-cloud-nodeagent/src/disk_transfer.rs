//! Explicitly configured SSH peers for cold migration of node-local root disks.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::host::{HostError, Result};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
    pub peers: BTreeMap<String, Peer>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub host: String,
    pub user: String,
    pub run_dir: PathBuf,
}
impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| HostError::failed(format!("disk transfer configuration: {e}")))?;
        for peer in config.peers.values() {
            if peer.host.is_empty()
                || peer.host.starts_with('-')
                || !peer
                    .host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c))
                || peer.user.is_empty()
                || peer.user.starts_with('-')
                || !peer
                    .user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
                || !peer.run_dir.is_absolute()
            {
                return Err(HostError::failed(
                    "disk transfer peers require a host, SSH user and absolute run directory",
                ));
            }
        }
        for file in [&config.identity_file, &config.known_hosts_file] {
            if !file.is_file() {
                return Err(HostError::failed(format!(
                    "{} is not a file",
                    file.display()
                )));
            }
        }
        Ok(config)
    }
    fn ssh(&self) -> Vec<String> {
        vec![
            "-i".into(),
            self.identity_file.display().to_string(),
            "-oBatchMode=yes".into(),
            "-oStrictHostKeyChecking=yes".into(),
            format!("-oUserKnownHostsFile={}", self.known_hosts_file.display()),
            "-oConnectTimeout=10".into(),
        ]
    }
    pub async fn copy(
        &self,
        node: &str,
        instance: &str,
        uid: &str,
        source: &Path,
        timeout: u32,
    ) -> Result<()> {
        let peer = self
            .peers
            .get(node)
            .ok_or_else(|| HostError::failed("destination has no configured disk-transfer peer"))?;
        let name = velstra_cloud_model::meta::ResourceName::parse(instance)
            .map_err(|e| HostError::failed(e.to_string()))?;
        if name.collection() != "instances"
            || !uid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(HostError::failed("invalid disk transfer identity"));
        }
        let dir = peer.run_dir.join(crate::hostfs::slug(instance));
        let partial = dir.join(format!("root.{uid}.incoming"));
        let target = dir.join("root.raw");
        let host = format!("{}@{}", peer.user, peer.host);
        let ssh = self.ssh();
        let work = async {
            run(
                "ssh",
                ssh.iter()
                    .cloned()
                    .chain([
                        host.clone(),
                        format!("mkdir -p -- {}", quote(&dir.display().to_string())),
                    ])
                    .collect(),
            )
            .await?;
            let remote_shell = std::iter::once("ssh".to_string())
                .chain(ssh.iter().map(|s| quote(s)))
                .collect::<Vec<_>>()
                .join(" ");
            // No --inplace: a partial transfer must never replace the bootable disk.
            run(
                "rsync",
                vec![
                    "--sparse".into(),
                    "--checksum".into(),
                    "--protect-args".into(),
                    "--timeout=30".into(),
                    "-e".into(),
                    remote_shell,
                    "--".into(),
                    source.display().to_string(),
                    format!("{host}:{}", partial.display()),
                ],
            )
            .await?;
            run(
                "ssh",
                ssh.iter()
                    .cloned()
                    .chain([
                        host,
                        format!(
                            "sync -f {0} && mv -f -- {0} {1} && sync -f {2}",
                            quote(&partial.display().to_string()),
                            quote(&target.display().to_string()),
                            quote(&dir.display().to_string())
                        ),
                    ])
                    .collect(),
            )
            .await
        };
        tokio::time::timeout(Duration::from_secs(u64::from(timeout.max(1))), work)
            .await
            .map_err(|_| {
                HostError::failed("local disk transfer timed out; the source retains ownership")
            })?
    }
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
async fn run(program: &str, args: Vec<String>) -> Result<()> {
    let result = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await?;
    if result.status.success() {
        Ok(())
    } else {
        Err(HostError::failed(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )))
    }
}
