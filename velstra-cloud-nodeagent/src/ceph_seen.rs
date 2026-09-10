//! What the cluster says about itself, read off Ceph's own JSON.
//!
//! [`crate::cephadm`] is about what the platform *does* to a cluster — the
//! commands and their argv. This is about what it *sees*: `ceph status`,
//! `ceph osd df tree`, `ceph osd metadata`, `ceph df` and
//! `ceph osd pool ls detail`, each parsed into the one struct the model
//! carries for it, [`CephSeen`]. Five commands rather than one because Ceph
//! spreads the facts an operator asks for first over five answers — health and
//! placement groups in one, per-OSD fill in another, which disk an OSD is in a
//! third — and none of them is the others.
//!
//! Only the shape that is read is described here, and everything is
//! `#[serde(default)]`: Ceph omits a rate that is zero and a check list that is
//! empty, and a reader that treated a missing key as a broken answer would
//! report nothing exactly on the healthy, idle cluster.

use std::collections::BTreeMap;

use velstra_cloud_model::{
    ceph::{CephSeen, CephWarning, OsdSeen, PgState, PoolSeen},
    meta::Timestamp,
};

use crate::{
    cephadm::CephAdmin,
    host::{HostError, Result},
};

// ---- ceph status -----------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct StatusJson {
    #[serde(default)]
    health: HealthJson,
    #[serde(default)]
    pgmap: PgMapJson,
}

#[derive(serde::Deserialize, Default)]
struct HealthJson {
    #[serde(default)]
    status: String,
    #[serde(default)]
    checks: BTreeMap<String, CheckJson>,
}

#[derive(serde::Deserialize, Default)]
struct CheckJson {
    #[serde(default)]
    severity: String,
    #[serde(default)]
    summary: SummaryJson,
}

#[derive(serde::Deserialize, Default)]
struct SummaryJson {
    #[serde(default)]
    message: String,
}

#[derive(serde::Deserialize, Default)]
struct PgMapJson {
    #[serde(default)]
    pgs_by_state: Vec<PgStateJson>,
    #[serde(default)]
    num_pgs: u64,
    #[serde(default)]
    num_objects: u64,
    #[serde(default)]
    bytes_used: u64,
    #[serde(default)]
    bytes_total: u64,
    #[serde(default)]
    read_bytes_sec: u64,
    #[serde(default)]
    write_bytes_sec: u64,
    #[serde(default)]
    read_op_per_sec: u64,
    #[serde(default)]
    write_op_per_sec: u64,
}

#[derive(serde::Deserialize)]
struct PgStateJson {
    state_name: String,
    count: u64,
}

/// Health, placement groups, fill and throughput out of `ceph status -f json`.
pub fn parse_status(json: &str) -> Result<CephSeen> {
    let s: StatusJson = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`ceph status` did not answer with json: {e}")))?;
    Ok(CephSeen {
        health: s.health.status,
        warnings: s
            .health
            .checks
            .into_iter()
            .map(|(code, c)| CephWarning {
                code,
                severity: c.severity,
                message: c.summary.message,
            })
            .collect(),
        pgs: s
            .pgmap
            .pgs_by_state
            .into_iter()
            .map(|p| PgState {
                state: p.state_name,
                count: p.count,
            })
            .collect(),
        pgs_total: s.pgmap.num_pgs,
        objects: s.pgmap.num_objects,
        used_bytes: s.pgmap.bytes_used,
        total_bytes: s.pgmap.bytes_total,
        read_bps: s.pgmap.read_bytes_sec,
        write_bps: s.pgmap.write_bytes_sec,
        read_ops: s.pgmap.read_op_per_sec,
        write_ops: s.pgmap.write_op_per_sec,
        ..CephSeen::default()
    })
}

// ---- ceph osd df tree ------------------------------------------------------

#[derive(serde::Deserialize)]
struct TreeJson {
    #[serde(default)]
    nodes: Vec<TreeNodeJson>,
}

#[derive(serde::Deserialize)]
struct TreeNodeJson {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    children: Vec<i64>,
    #[serde(default)]
    kb: u64,
    #[serde(default)]
    kb_used: u64,
    #[serde(default)]
    pgs: u64,
    #[serde(default)]
    status: String,
    /// `1` in, `0` out, and anything between for a disk being drained slowly.
    #[serde(default)]
    reweight: f64,
    #[serde(default)]
    device_class: String,
}

/// Every OSD with its host, fill and state, out of `ceph osd df tree -f json`.
///
/// The host is the OSD's parent in the CRUSH tree — the tree is the only place
/// Ceph says which machine a disk is in without a second command, and the
/// platform registers hosts by the node's bare id, so the name lines up.
pub fn parse_osd_tree(json: &str) -> Result<Vec<OsdSeen>> {
    let t: TreeJson = serde_json::from_str(json).map_err(|e| {
        HostError::failed(format!("`ceph osd df tree` did not answer with json: {e}"))
    })?;
    let host_of: BTreeMap<i64, &str> = t
        .nodes
        .iter()
        .filter(|n| n.kind == "host")
        .flat_map(|h| h.children.iter().map(move |c| (*c, h.name.as_str())))
        .collect();
    Ok(t.nodes
        .iter()
        .filter(|n| n.kind == "osd" && n.id >= 0)
        .map(|n| OsdSeen {
            id: n.id as u64,
            host: host_of
                .get(&n.id)
                .map(|h| h.to_string())
                .unwrap_or_default(),
            up: n.status.eq_ignore_ascii_case("up"),
            r#in: n.reweight > 0.0,
            used_bytes: n.kb_used * 1024,
            total_bytes: n.kb * 1024,
            pgs: n.pgs,
            class: n.device_class.clone(),
            ..OsdSeen::default()
        })
        .collect())
}

// ---- ceph osd metadata -----------------------------------------------------

#[derive(serde::Deserialize)]
struct MetadataJson {
    id: u64,
    /// Kernel names, comma-separated when an OSD spans more than one disk.
    #[serde(default)]
    devices: String,
}

/// Which disk each OSD was made from, by OSD id, out of `ceph osd metadata`.
///
/// Kernel names as `/dev/sda` — the controller turns them into the path the
/// node reports the disk by, so the row lines up with `spec.osds`.
pub fn parse_osd_devices(json: &str) -> Result<BTreeMap<u64, String>> {
    let rows: Vec<MetadataJson> = serde_json::from_str(json).map_err(|e| {
        HostError::failed(format!("`ceph osd metadata` did not answer with json: {e}"))
    })?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let first = r.devices.split(',').next()?.trim();
            (!first.is_empty()).then(|| (r.id, format!("/dev/{first}")))
        })
        .collect())
}

// ---- ceph df / ceph osd pool ls detail ------------------------------------

#[derive(serde::Deserialize)]
struct DfJson {
    #[serde(default)]
    pools: Vec<DfPoolJson>,
}

#[derive(serde::Deserialize)]
struct DfPoolJson {
    name: String,
    #[serde(default)]
    stats: DfStatsJson,
}

#[derive(serde::Deserialize, Default)]
struct DfStatsJson {
    #[serde(default)]
    stored: u64,
    #[serde(default)]
    objects: u64,
    #[serde(default)]
    max_avail: u64,
}

/// What every pool holds, out of `ceph df -f json`.
pub fn parse_df(json: &str) -> Result<Vec<PoolSeen>> {
    let d: DfJson = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`ceph df` did not answer with json: {e}")))?;
    Ok(d.pools
        .into_iter()
        .map(|p| PoolSeen {
            pool: p.name,
            stored_bytes: p.stats.stored,
            objects: p.stats.objects,
            max_avail_bytes: p.stats.max_avail,
            ..PoolSeen::default()
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct PoolDetailJson {
    pool_name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    min_size: u64,
    #[serde(default)]
    pg_num: u64,
}

/// How every pool is replicated, out of `ceph osd pool ls detail -f json`.
pub fn parse_pool_detail(json: &str) -> Result<BTreeMap<String, (u64, u64, u64)>> {
    let rows: Vec<PoolDetailJson> = serde_json::from_str(json).map_err(|e| {
        HostError::failed(format!(
            "`ceph osd pool ls detail` did not answer with json: {e}"
        ))
    })?;
    Ok(rows
        .into_iter()
        .map(|p| (p.pool_name, (p.size, p.min_size, p.pg_num)))
        .collect())
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

impl CephAdmin {
    /// Everything Ceph says about the cluster, from here.
    ///
    /// `ceph status` is the one answer this cannot do without: a node that
    /// cannot get it has no admin keyring and reports nothing, which is the
    /// right answer for it. The other four are attempted and kept if they
    /// came — a cluster whose manager is down still answers `status` and stops
    /// answering `df`, and health without fill is worth more than neither.
    pub async fn seen(&self) -> Result<CephSeen> {
        let out = self.ceph(&args(&["status", "-f", "json"])).await?;
        let mut seen = parse_status(&String::from_utf8_lossy(&out))?;

        match self.ceph(&args(&["osd", "df", "tree", "-f", "json"])).await {
            Ok(out) => match parse_osd_tree(&String::from_utf8_lossy(&out)) {
                Ok(osds) => seen.osds = osds,
                Err(e) => tracing::debug!(error = %e, "osd table not read"),
            },
            Err(e) => tracing::debug!(error = %e, "osd table not read"),
        }
        if !seen.osds.is_empty() {
            match self.ceph(&args(&["osd", "metadata", "-f", "json"])).await {
                Ok(out) => match parse_osd_devices(&String::from_utf8_lossy(&out)) {
                    Ok(devices) => {
                        for osd in &mut seen.osds {
                            if let Some(d) = devices.get(&osd.id) {
                                osd.device = d.clone();
                            }
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "osd devices not read"),
                },
                Err(e) => tracing::debug!(error = %e, "osd devices not read"),
            }
        }
        match self.ceph(&args(&["df", "-f", "json"])).await {
            Ok(out) => match parse_df(&String::from_utf8_lossy(&out)) {
                Ok(pools) => seen.pool_stats = pools,
                Err(e) => tracing::debug!(error = %e, "pool fill not read"),
            },
            Err(e) => tracing::debug!(error = %e, "pool fill not read"),
        }
        if !seen.pool_stats.is_empty() {
            match self
                .ceph(&args(&["osd", "pool", "ls", "detail", "-f", "json"]))
                .await
            {
                Ok(out) => match parse_pool_detail(&String::from_utf8_lossy(&out)) {
                    Ok(detail) => {
                        for p in &mut seen.pool_stats {
                            if let Some((size, min_size, pg_num)) = detail.get(&p.pool) {
                                p.size = *size;
                                p.min_size = *min_size;
                                p.pg_num = *pg_num;
                            }
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "pool replication not read"),
                },
                Err(e) => tracing::debug!(error = %e, "pool replication not read"),
            }
        }
        seen.at = Timestamp::now();
        Ok(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real answers from a one-node lab cluster, trimmed to what is read plus
    // the keys around it — so a rename upstream fails here, not on a node.
    const STATUS: &str = r#"{"fsid":"2388447e","health":{"status":"HEALTH_WARN","checks":{"POOL_NO_REDUNDANCY":{"severity":"HEALTH_WARN","summary":{"message":"2 pool(s) have no replicas configured","count":2},"muted":false},"TOO_FEW_OSDS":{"severity":"HEALTH_WARN","summary":{"message":"OSD count 1 < osd_pool_default_size 2","count":1},"muted":false}},"mutes":[]},"quorum_names":["horst"],"osdmap":{"epoch":39,"num_osds":1,"num_up_osds":1,"num_in_osds":1},"pgmap":{"pgs_by_state":[{"state_name":"active+clean","count":64}],"num_pgs":64,"num_pools":2,"num_objects":95,"data_bytes":340234646,"bytes_used":371761152,"bytes_avail":123641225216,"bytes_total":124012986368,"read_bytes_sec":4096,"write_op_per_sec":7},"mgrmap":{"available":true}}"#;
    const TREE: &str = r#"{"nodes":[{"id":-1,"name":"default","type":"root","type_id":11,"reweight":-1,"kb":121106432,"kb_used":363048,"children":[-3]},{"id":-3,"name":"horst","type":"host","type_id":1,"reweight":-1,"kb":121106432,"kb_used":363048,"children":[0,1]},{"id":0,"device_class":"hdd","name":"osd.0","type":"osd","type_id":0,"crush_weight":0.11279296875,"depth":2,"reweight":1,"kb":121106432,"kb_used":363048,"kb_avail":120743384,"utilization":0.2997,"var":1,"pgs":64,"status":"up"},{"id":1,"device_class":"ssd","name":"osd.1","type":"osd","type_id":0,"reweight":0,"kb":1000,"kb_used":10,"pgs":0,"status":"down"}],"stray":[],"summary":{"total_kb":121106432}}"#;
    const METADATA: &str = r#"[{"id":0,"arch":"x86_64","bluestore_bdev_devices":"sda","devices":"sda","hostname":"horst","osd_objectstore":"bluestore"},{"id":1,"devices":"nvme0n1,nvme1n1","hostname":"horst"}]"#;
    const DF: &str = r#"{"stats":{"total_bytes":124012986368,"total_used_bytes":371761152,"num_osds":1},"pools":[{"name":"velstra-volumes","id":1,"stats":{"stored":2093,"objects":6,"kb_used":14,"bytes_used":14120,"percent_used":1.2e-07,"max_avail":117440577536}},{"name":"velstra-images","id":2,"stats":{"stored":338229039,"objects":89,"kb_used":330318,"bytes_used":338245263,"percent_used":0.0028,"max_avail":117440577536}}]}"#;
    const DETAIL: &str = r#"[{"pool_id":1,"pool_name":"velstra-volumes","size":1,"min_size":1,"pg_num":32,"pgp_num":32},{"pool_id":2,"pool_name":"velstra-images","size":1,"min_size":1,"pg_num":32}]"#;

    #[test]
    fn status_becomes_health_pgs_fill_and_rates() {
        let s = parse_status(STATUS).unwrap();
        assert_eq!(s.health, "HEALTH_WARN");
        assert_eq!(s.warnings.len(), 2);
        assert_eq!(s.warnings[0].code, "POOL_NO_REDUNDANCY");
        assert_eq!(
            s.warnings[0].message,
            "2 pool(s) have no replicas configured"
        );
        assert_eq!(
            s.pgs,
            [PgState {
                state: "active+clean".into(),
                count: 64
            }]
        );
        assert_eq!(s.pgs_total, 64);
        assert_eq!(s.objects, 95);
        assert_eq!((s.used_bytes, s.total_bytes), (371761152, 124012986368));
        // Ceph only writes a rate that is not zero; the others read as zero.
        assert_eq!(
            (s.read_bps, s.write_bps, s.read_ops, s.write_ops),
            (4096, 0, 0, 7)
        );
    }

    #[test]
    fn the_osd_table_has_host_fill_and_both_states() {
        let osds = parse_osd_tree(TREE).unwrap();
        assert_eq!(osds.len(), 2);
        let up = &osds[0];
        assert_eq!(
            (up.id, up.host.as_str(), up.class.as_str()),
            (0, "horst", "hdd")
        );
        assert!(up.up && up.r#in);
        assert_eq!(up.used_bytes, 363048 * 1024);
        assert_eq!(up.total_bytes, 121106432 * 1024);
        assert_eq!(up.pgs, 64);
        let down = &osds[1];
        assert!(
            !down.up && !down.r#in,
            "reweight 0 is out, status down is down"
        );
        assert_eq!(down.host, "horst");
    }

    #[test]
    fn osd_devices_are_kernel_names_made_into_paths() {
        let d = parse_osd_devices(METADATA).unwrap();
        assert_eq!(d[&0], "/dev/sda");
        // The first of several is the one the row is named after.
        assert_eq!(d[&1], "/dev/nvme0n1");
    }

    #[test]
    fn pools_get_their_fill_and_their_replication() {
        let pools = parse_df(DF).unwrap();
        assert_eq!(pools[1].pool, "velstra-images");
        assert_eq!((pools[1].stored_bytes, pools[1].objects), (338229039, 89));
        assert_eq!(pools[1].max_avail_bytes, 117440577536);
        let detail = parse_pool_detail(DETAIL).unwrap();
        assert_eq!(detail["velstra-volumes"], (1, 1, 32));
    }

    #[test]
    fn an_idle_healthy_cluster_reads_as_one() {
        // Ceph omits what is zero or empty; nothing here is an error.
        let s = parse_status(r#"{"health":{"status":"HEALTH_OK"},"pgmap":{"num_pgs":1}}"#).unwrap();
        assert_eq!(s.health, "HEALTH_OK");
        assert!(s.warnings.is_empty() && s.read_bps == 0);
        assert!(parse_status("not json").is_err());
        assert!(parse_osd_tree("not json").is_err());
    }
}
