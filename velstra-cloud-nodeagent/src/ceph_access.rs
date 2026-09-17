//! How this node opens an RBD image: where the cluster is, and who it is.
//!
//! ## Why the hypervisor needs its own answer
//!
//! The pool agent already knows this — [`crate::ceph_pool::CephConfig`] carries
//! a configuration path and a client id, answered at setup and handed over on
//! the command line. But the pool agent only ever *creates* the volume. The
//! process that has to **open** it is QEMU, on whichever hypervisor the guest
//! landed on, and that machine may not be the one running the pool agent at
//! all — which is the whole point of Ceph.
//!
//! Until this existed the node agent knew none of it. Every RBD image was
//! handed to QEMU as a bare `rbd:pool/image`, so QEMU fell back to its own
//! defaults: `/etc/ceph/ceph.conf` and the default client. A cell whose
//! configuration lives anywhere else had a pool agent that could make a volume
//! and a hypervisor that could not open it — and the failure surfaces on the
//! guest, several minutes and one scheduling decision away from the setting
//! that caused it.
//!
//! It is also what makes the sealed appliance able to reach Ceph at all: its
//! `/etc` is a read-only dm-verity store, so `/etc/ceph/ceph.conf` is not a
//! file anybody can put there. Naming the path instead lets it live on the one
//! writable partition with the rest of the machine's state.
//!
//! ## One shape, two spellings
//!
//! QEMU takes the same four facts twice, in two syntaxes: as `-drive` options
//! when a guest starts, and as a `blockdev-add` argument when a disk is
//! plugged into a running one. Both are built here from one value, because the
//! alternative — each call site spelling it out — is how the boot path and the
//! attach path came to disagree in the first place.
//!
//! The legacy `rbd:pool/image:conf=…:id=…` filename form is deliberately not
//! used. It is a second escaping scheme (backslash, over a colon separator)
//! for facts that have perfectly good named fields, and QEMU's own schema is
//! unambiguous about what those fields are:
//!
//! ```text
//! { 'struct': 'BlockdevOptionsRbd',
//!   'data': { 'pool': 'str', 'image': 'str', '*conf': 'str', '*user': 'str', … } }
//! ```
//!
//! `pool` is not optional there. A `blockdev-add` that sent `image:
//! "pool/image"` and no `pool` — which is what this node sent before — is
//! refused by QMP for a missing parameter, so attaching a Ceph volume to a
//! running guest could never have worked.

use serde_json::{Value, json};

/// The prefix a pool puts on a place it keeps in Ceph.
pub const PREFIX: &str = "rbd:";

/// A volume's place in a Ceph cluster, as its two halves.
///
/// Borrowed rather than owned: every caller has the place in hand already, and
/// a split that allocated would be a copy per observation pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rbd<'a> {
    pub pool: &'a str,
    pub image: &'a str,
}

/// Split `rbd:<pool>/<image>` into its halves, or `None` for anything else.
///
/// `None` rather than an error, and the caller treats it as "not a Ceph place"
/// — a directory pool's place is a path, and a path is not a malformed RBD
/// name. What *is* refused is an `rbd:` prefix with nothing usable after it:
/// an empty pool or image would reach QEMU as a well-formed message naming
/// nothing, and the error would come back from the cluster instead of from
/// here.
pub fn split(place: &str) -> Option<Rbd<'_>> {
    let rest = place.strip_prefix(PREFIX)?;
    let (pool, image) = rest.split_once('/')?;
    (!pool.is_empty() && !image.is_empty()).then_some(Rbd { pool, image })
}

/// What this node needs in order to open an RBD image.
///
/// Both `None` is the ordinary answer and means "whatever QEMU would do on its
/// own" — `/etc/ceph/ceph.conf` and the default client, which is right on a
/// machine where Ceph was installed the usual way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CephAccess {
    /// `ceph.conf`, when it is not where QEMU would look.
    pub conf: Option<String>,
    /// The Ceph client id to act as — `admin`, not `client.admin`. QEMU's
    /// `user` is Ceph's `id`, and the `client.` prefix belongs to neither.
    pub user: Option<String>,
}

impl CephAccess {
    /// Build from two answers that may be missing *or* blank.
    ///
    /// The blank case is not hypothetical: these arrive from the seed through
    /// `EnvironmentFile`, which has no way to say "unset". A machine that has
    /// never been asked about Ceph and one whose `VELSTRA_CEPH_CONF=` was left
    /// empty both land here, and an empty path handed to QEMU is a file it
    /// cannot open — reported as a cluster that cannot be reached, several
    /// layers from the line that caused it.
    pub fn new(conf: Option<String>, user: Option<String>) -> Self {
        Self {
            conf: non_empty(conf),
            user: non_empty(user).map(|u| strip_client(&u).to_string()),
        }
    }

    /// The `file.*` half of a `-drive` argument, for a guest being started.
    ///
    /// Written as `file.driver=rbd,file.pool=…` rather than the legacy
    /// `file=rbd:…` string so that the four facts travel as the four fields
    /// QEMU's schema names — the same ones [`Self::blockdev`] sends.
    pub fn drive_options(&self, rbd: &Rbd) -> String {
        let mut out = format!(
            "file.driver=rbd,file.pool={},file.image={}",
            escape_comma(rbd.pool),
            escape_comma(rbd.image)
        );
        if let Some(conf) = &self.conf {
            out.push_str(&format!(",file.conf={}", escape_comma(conf)));
        }
        if let Some(user) = &self.user {
            out.push_str(&format!(",file.user={}", escape_comma(user)));
        }
        out
    }

    /// The `file` member of a `blockdev-add`, for a disk being plugged in.
    pub fn blockdev(&self, rbd: &Rbd) -> Value {
        let mut file = json!({ "driver": "rbd", "pool": rbd.pool, "image": rbd.image });
        let map = file.as_object_mut().expect("an object literal");
        if let Some(conf) = &self.conf {
            map.insert("conf".into(), json!(conf));
        }
        if let Some(user) = &self.user {
            map.insert("user".into(), json!(user));
        }
        file
    }
}

/// `client.velstra` and `velstra` both mean the same client to Ceph, and both
/// `rbd --id` and QEMU's `user` want the second spelling. Accepting either is
/// not laxity: `ceph auth` prints the first, so it is what an operator copies.
///
/// Here rather than beside the pool agent that first needed it, because the
/// hypervisor needs the same normalisation off the same seed key. Two copies
/// would be two chances to normalise on one side only — and *that* failure is
/// silent on the side that forgot: the pool agent makes the volume as
/// `velstra`, the hypervisor asks the cluster for a client called
/// `client.velstra`, and the guest is refused by Ceph for credentials that are
/// perfectly correct.
pub fn strip_client(user: &str) -> &str {
    user.strip_prefix("client.").unwrap_or(user)
}

/// An unset variable and one set to nothing are the same answer.
///
/// `EnvironmentFile` has no way to say "not set": a seed that has never been
/// asked about Ceph and one whose `VELSTRA_CEPH_CONF=` was left blank both
/// arrive here, and an empty path handed to QEMU is a file it cannot open.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// A literal comma in a QEMU option string is written twice.
///
/// Pool and image names cannot contain one, but a configuration path set by an
/// operator can, and the failure it would otherwise cause is the worst kind:
/// QEMU reads the tail as a further option, rejects an argument nobody wrote,
/// and names a key that appears nowhere in this cell's configuration.
fn escape_comma(value: &str) -> String {
    value.replace(',', ",,")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves come apart, and only for a place that is actually Ceph.
    #[test]
    fn a_ceph_place_splits_into_a_pool_and_an_image() {
        assert_eq!(
            split("rbd:velstra-volumes/root-1"),
            Some(Rbd {
                pool: "velstra-volumes",
                image: "root-1"
            })
        );
        for not_ceph in [
            "/var/lib/velstra/volumes/v1.qcow2",
            "/dev/vg0/lv1",
            "rbd:",
            "rbd:velstra-volumes",
            "rbd:/root-1",
            "rbd:velstra-volumes/",
        ] {
            assert!(split(not_ceph).is_none(), "{not_ceph} was split");
        }
    }

    /// An image name may itself carry slashes — a namespace, a nested name —
    /// and only the first one separates it from the pool.
    #[test]
    fn only_the_first_slash_separates_the_pool() {
        assert_eq!(
            split("rbd:pool/a/b"),
            Some(Rbd {
                pool: "pool",
                image: "a/b"
            })
        );
    }

    /// With nothing configured, QEMU is told the two facts it cannot guess and
    /// left to its own defaults for the two it can.
    #[test]
    fn an_unconfigured_node_names_only_the_pool_and_the_image() {
        let rbd = split("rbd:velstra-volumes/root-1").expect("a place");
        let access = CephAccess::default();
        assert_eq!(
            access.drive_options(&rbd),
            "file.driver=rbd,file.pool=velstra-volumes,file.image=root-1"
        );
        assert_eq!(
            access.blockdev(&rbd),
            json!({ "driver": "rbd", "pool": "velstra-volumes", "image": "root-1" })
        );
    }

    /// Both spellings carry the same four facts. This is the test that would
    /// have caught the two halves drifting apart.
    #[test]
    fn both_spellings_carry_what_the_node_was_configured_with() {
        let rbd = split("rbd:velstra-volumes/root-1").expect("a place");
        let access = CephAccess {
            conf: Some("/var/lib/velstra/ceph/ceph.conf".into()),
            user: Some("velstra".into()),
        };
        assert_eq!(
            access.drive_options(&rbd),
            "file.driver=rbd,file.pool=velstra-volumes,file.image=root-1,\
             file.conf=/var/lib/velstra/ceph/ceph.conf,file.user=velstra"
        );
        assert_eq!(
            access.blockdev(&rbd),
            json!({
                "driver": "rbd",
                "pool": "velstra-volumes",
                "image": "root-1",
                "conf": "/var/lib/velstra/ceph/ceph.conf",
                "user": "velstra",
            })
        );
    }

    /// `pool` is not optional in QEMU's schema, and sending the pool inside
    /// `image` is what made attaching a Ceph volume impossible.
    #[test]
    fn a_blockdev_always_names_the_pool_on_its_own() {
        let rbd = split("rbd:velstra-volumes/root-1").expect("a place");
        let sent = CephAccess::default().blockdev(&rbd);
        assert_eq!(sent["pool"], json!("velstra-volumes"));
        assert_eq!(sent["image"], json!("root-1"));
        assert!(
            !sent["image"].as_str().expect("a string").contains('/'),
            "the pool is still inside the image name: {sent}"
        );
    }

    /// A blank answer is not an answer. `EnvironmentFile` cannot express
    /// "unset", so an empty value has to mean the same as a missing one.
    #[test]
    fn a_blank_setting_is_the_same_as_none() {
        assert_eq!(CephAccess::new(None, None), CephAccess::default());
        assert_eq!(
            CephAccess::new(Some(String::new()), Some("   ".into())),
            CephAccess::default()
        );
        assert_eq!(
            CephAccess::new(Some("/etc/ceph/ceph.conf".into()), Some("velstra".into())),
            CephAccess {
                conf: Some("/etc/ceph/ceph.conf".into()),
                user: Some("velstra".into()),
            }
        );
    }

    /// The pool agent and the hypervisor read the *same* seed key, and the one
    /// spelling an operator copies out of `ceph auth` is the one neither tool
    /// takes. Normalising in only one of them is the worst outcome: the volume
    /// is created and the guest that would use it is refused.
    #[test]
    fn a_client_prefix_is_stripped_the_way_the_pool_agent_strips_it() {
        assert_eq!(strip_client("client.velstra"), "velstra");
        assert_eq!(strip_client("velstra"), "velstra");
        assert_eq!(strip_client("client.admin"), "admin");
        for spelling in ["client.velstra", "velstra"] {
            assert_eq!(
                CephAccess::new(None, Some(spelling.into())).user.as_deref(),
                Some("velstra"),
                "{spelling}"
            );
        }
    }

    /// A comma in a path must not become an option separator.
    #[test]
    fn a_comma_in_a_path_stays_part_of_the_path() {
        let rbd = split("rbd:p/i").expect("a place");
        let access = CephAccess {
            conf: Some("/etc/ceph/odd,name.conf".into()),
            user: None,
        };
        assert!(
            access
                .drive_options(&rbd)
                .ends_with("file.conf=/etc/ceph/odd,,name.conf"),
            "{}",
            access.drive_options(&rbd)
        );
        // The JSON form needs no escaping at all, and must not acquire any.
        assert_eq!(
            access.blockdev(&rbd)["conf"],
            json!("/etc/ceph/odd,name.conf")
        );
    }
}
