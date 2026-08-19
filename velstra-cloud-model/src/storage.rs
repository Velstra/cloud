//! Bringing a volume into existence, copying one, and refusing to destroy data
//! by accident.
//!
//! A volume is the first object in this platform whose owner is not a node. It
//! lives in a **pool** — an LVM group, a ZFS dataset, a Ceph pool — and the pool
//! is the party that can see whether the bytes are there. So `spec.pool` is the
//! assignment and `status.pool` is the claim, which is the same two-field
//! ownership dance as an instance and its node, reached through the same access
//! rule. Nothing new was needed to make storage work; what was missing was
//! *anybody at all* being allowed to write a volume's status.
//!
//! A [`Snapshot`](crate::resources::Snapshot) is owned the same way and for the
//! same reason, and this file says so once rather than twice: its bytes are in a
//! pool, so the pool claims it and reports on it, and everything a snapshot says
//! about itself is something the backend can be asked right now. It is not
//! computed the way a migration's `Moved` is — that one is a judgement about
//! *another* object, which is why nobody may own it — and "the copy is there,
//! and it is this big" is nothing of the sort. It is one party's observation of
//! its own disks, so it is stored, by the one party that can see it.
//!
//! # The rules this file exists to enforce
//!
//! **A volume is never shrunk.** Growing one is arithmetic; shrinking one throws
//! away whatever was past the new end, and no amount of "the operator asked for
//! it" makes that recoverable. So a spec smaller than what exists is reported as
//! a refusal on the object and nothing happens — the volume keeps its size and
//! says why. An operator who really wants a smaller volume makes a smaller one
//! and copies, which is the operation they actually meant.
//!
//! **A snapshot is taken once and never again.** A volume that vanished from
//! its pool is created again on the next pass, because a volume is a container
//! and an empty one is what was asked for. A snapshot is a *moment*, and making
//! a new copy under the name of an old one hands an operator the wrong data
//! under the right label at exactly the moment they are restoring from it. So
//! the one place this model consults a stored status rather than the backend is
//! here: the pool's disks cannot remember what they no longer hold, and only the
//! object can say that this copy already happened.
//!
//! **A volume with snapshots is not destroyed underneath them.** On every
//! backend in sight a snapshot is a delta against its source, so the source
//! going first makes the copies unreadable — and it would happen quietly, at
//! the moment somebody deletes something they believe they have a backup of.
//! The pool refuses while it can see copies of the volume, the volume carries
//! [`SNAPSHOT_SOURCE_FINALIZER`](crate::resources::SNAPSHOT_SOURCE_FINALIZER)
//! while any snapshot object names it, and both answers come from looking
//! rather than from remembering.
//!
//! Everything else here follows the same discipline as the rest of the model:
//! the functions are pure, they are handed what the pool observed rather than
//! what anybody remembers, and running them twice over a settled volume asks for
//! nothing.

use crate::{
    meta::{Condition, ConditionStatus, ResourceName},
    resources::{Snapshot, Volume, VolumeSpec},
};

/// Where a volume's bytes come from at the moment it is created.
///
/// One value rather than two optional names, because a volume comes from
/// exactly one place and the pair could say otherwise. Cloning is part of
/// creating: a volume that exists blank for one pass is a volume a guest can be
/// started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeSource {
    Blank,
    Image(String),
    Snapshot(String),
}

impl VolumeSource {
    /// What a spec asks for.
    ///
    /// A spec naming both is refused at the API, before anything is stored, so
    /// this is the shape of an object that predates that check. It answers with
    /// the snapshot, which is the narrower of the two claims: a snapshot has a
    /// lineage of its own and the image is at the far end of it, so choosing it
    /// can only ever mean *more* recent bytes, never a silent regression to an
    /// installer image.
    pub fn of(spec: &VolumeSpec) -> Self {
        match (&spec.source_snapshot, &spec.source_image) {
            (Some(snapshot), _) if !snapshot.is_empty() => Self::Snapshot(snapshot.clone()),
            (_, Some(image)) if !image.is_empty() => Self::Image(image.clone()),
            _ => Self::Blank,
        }
    }
}

/// What a pool agent should do about one volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeAction {
    /// Create the backing store, cloning `source` if there is one.
    Provision {
        volume: String,
        gib: u64,
        source: VolumeSource,
        encryption_key: Option<String>,
    },
    /// Make it bigger. Never smaller — see the module doc.
    Grow { volume: String, to_gib: u64 },
    /// Destroy the backing store, on the way out.
    Destroy { volume: String },
}

// There is deliberately no `ReleaseFinalizer` here. A pool may not write `meta`,
// so it cannot drop its own finalizer; it reports that it has let go and a
// controller acts on that. An action the agent could only answer with a no-op
// would read like something happening.

/// What the pool sees of one volume on itself, right now.
///
/// Handed in rather than read from the object, because a pool that trusted the
/// object's own `status.provisioned` would believe a stale report over its own
/// disks — which is how a volume that was deleted underneath the platform stays
/// "provisioned" forever.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeenInPool {
    pub exists: bool,
    pub gib: u64,
    /// How many copies of *this* volume the pool holds.
    ///
    /// Counted on the backend rather than taken from the store, because it is
    /// what makes destroying safe or unsafe, and the backend is the only place
    /// that knows about a snapshot somebody took with a shell. A number rather
    /// than a flag so the volume can say how many are in the way.
    pub snapshots: u32,
}

/// The whole decision, from the ask and what the pool can see.
pub fn reconcile_volume(volume: &Volume, seen: SeenInPool) -> Vec<VolumeAction> {
    let name = volume.meta.name.to_string();

    if volume.meta.is_deleting() {
        // Copies of it are read through it. Destroying now would take them with
        // it, silently, at the moment somebody deletes a volume they believe
        // they have backups of — so nothing happens until the copies are gone,
        // and `volume_condition` says what is in the way.
        if seen.exists && seen.snapshots == 0 {
            return vec![VolumeAction::Destroy { volume: name }];
        }
        // Nothing left to do on the backend. Whether the finalizer may go is
        // said in the status, not done here — see the note on `VolumeAction`.
        return Vec::new();
    }

    if !seen.exists {
        return vec![VolumeAction::Provision {
            volume: name,
            gib: volume.spec.size_gib,
            source: VolumeSource::of(&volume.spec),
            encryption_key: volume.spec.encryption_key.clone(),
        }];
    }
    if seen.gib < volume.spec.size_gib {
        return vec![VolumeAction::Grow {
            volume: name,
            to_gib: volume.spec.size_gib,
        }];
    }
    // Larger than asked for: nothing. Not an error to repair, because the repair
    // would be destroying data. `volume_condition` says so on the object.
    Vec::new()
}

/// What the volume should say about itself.
pub fn volume_condition(volume: &Volume, seen: SeenInPool) -> Condition {
    let at = volume.meta.generation;
    if volume.meta.is_deleting() && seen.exists && seen.snapshots > 0 {
        return Condition::new(
            "Ready",
            ConditionStatus::False,
            "SnapshotsDependOnIt",
            &format!(
                "{} snapshots were taken from it and are read through it, so nothing was \
                 destroyed — delete them first, or keep the volume",
                seen.snapshots
            ),
            at,
        );
    }
    if !seen.exists {
        return Condition::new(
            "Ready",
            ConditionStatus::Unknown,
            "Provisioning",
            "the pool has not created the backing store yet",
            at,
        );
    }
    if seen.gib > volume.spec.size_gib {
        return Condition::new(
            "Ready",
            ConditionStatus::False,
            "WillNotShrink",
            &format!(
                "it is {} GiB and was asked to be {} GiB; shrinking would destroy \
                 whatever is past the new end, so it was left alone — make a \
                 smaller volume and copy what you need",
                seen.gib, volume.spec.size_gib
            ),
            at,
        );
    }
    if seen.gib < volume.spec.size_gib {
        return Condition::new(
            "Ready",
            ConditionStatus::Unknown,
            "Growing",
            &format!("{} GiB of {} GiB", seen.gib, volume.spec.size_gib),
            at,
        );
    }
    Condition::new(
        "Ready",
        ConditionStatus::True,
        "Ready",
        &format!("{} GiB in {}", seen.gib, volume.spec.pool),
        at,
    )
}

// ---- snapshots -----------------------------------------------------------

/// The volume a snapshot was taken from: its parent, because the source is in a
/// snapshot's identity rather than in a field.
///
/// `None` for a name that is not under a volume, which the API refuses at
/// create — so a stored snapshot always has one, and everything that reads this
/// still has to say what it does when it does not.
pub fn source_volume(snapshot: &Snapshot) -> Option<ResourceName> {
    snapshot
        .meta
        .name
        .parent()
        .filter(|parent| parent.collection() == "volumes")
}

/// What the pool sees of one snapshot on itself, right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeenSnapshot {
    pub exists: bool,
    /// The logical size of the copy — how large the volume was when it was
    /// made, and so the smallest volume that can be made from it.
    pub gib: u64,
}

/// What a pool agent should do about one snapshot.
///
/// There is no `Grow` and no `Retake`. A snapshot has no size of its own to
/// change and no second moment to be taken at; the only two things that can
/// happen to one are that it comes into existence and that it stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotAction {
    /// Copy `volume` as it stands, under this name.
    Take { snapshot: String, volume: String },
    /// Destroy the copy. The volume it was taken from is untouched.
    Destroy { snapshot: String },
}

/// The whole decision, from the ask and what the pool can see.
///
/// The one asymmetry with [`reconcile_volume`] is deliberate and is the reason
/// this function exists at all: a volume that disappeared from its pool is made
/// again, and a snapshot that disappeared is **not**. For a volume the repair is
/// harmless — an empty container is what was asked for — and refusing to repair
/// leaves a guest opening a device that is not there. For a snapshot the repair
/// is the harm: a copy taken now is a copy of a different moment, and it would
/// wear the name of the one somebody is about to restore from. So a snapshot the
/// pool has never held is taken, and a snapshot the pool held and lost is
/// reported as lost.
pub fn reconcile_snapshot(snapshot: &Snapshot, seen: SeenSnapshot) -> Vec<SnapshotAction> {
    let name = snapshot.meta.name.to_string();

    if snapshot.meta.is_deleting() {
        if seen.exists {
            return vec![SnapshotAction::Destroy { snapshot: name }];
        }
        // As with a volume: whether the finalizer may go is said in the status
        // and acted on by a controller, because a pool may not write `meta`.
        return Vec::new();
    }
    if seen.exists {
        return Vec::new();
    }
    // Reported once and gone: this is the only place in the model where a
    // stored status decides an action, and it is because the backend cannot
    // remember what it no longer holds. See the module doc.
    if snapshot.status.taken {
        return Vec::new();
    }
    let Some(volume) = source_volume(snapshot) else {
        // A snapshot of nothing. Nothing to copy, and `snapshot_condition` says
        // so; acting on a guess about which volume was meant is how the wrong
        // data ends up under a name somebody trusts.
        return Vec::new();
    };
    vec![SnapshotAction::Take {
        snapshot: name,
        volume: volume.to_string(),
    }]
}

/// What the snapshot should say about itself.
pub fn snapshot_condition(snapshot: &Snapshot, seen: SeenSnapshot) -> Condition {
    let at = snapshot.meta.generation;
    if seen.exists {
        return Condition::new(
            "Ready",
            ConditionStatus::True,
            "Ready",
            &format!("{} GiB in {}", seen.gib, snapshot.spec.pool),
            at,
        );
    }
    if snapshot.status.taken {
        return Condition::new(
            "Ready",
            ConditionStatus::False,
            "Vanished",
            "the pool no longer holds this copy, and it will not be taken again — a copy made \
             now would be of a different moment under the same name; delete it and take a new \
             one if you still want a copy",
            at,
        );
    }
    if source_volume(snapshot).is_none() {
        return Condition::new(
            "Ready",
            ConditionStatus::False,
            "NoSource",
            "a snapshot is a copy of the volume it lives under, and this one is not under a \
             volume",
            at,
        );
    }
    Condition::new(
        "Ready",
        ConditionStatus::Unknown,
        "Taking",
        "the pool has not made the copy yet",
        at,
    )
}

/// Why a copy cannot be made, or made from.
///
/// Every one is knowable before anything is written — the same discipline as
/// [`crate::migration::may_migrate`], and for the same reason: finding out
/// afterwards means finding out from a backend error on an object that already
/// exists, which is a worse sentence delivered later.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{volume} has not been provisioned yet, so there is nothing to copy")]
    SourceNotProvisioned { volume: String },
    #[error("{volume} is being deleted; a copy taken now would keep it from ever going")]
    SourceGoing { volume: String },
    #[error("{snapshot} has not been taken yet, so there is nothing to make a volume from")]
    NotTakenYet { snapshot: String },
    /// A volume smaller than the copy it is made from is a volume the clone
    /// does not fit in, and the pool cannot shrink what it writes.
    #[error(
        "{snapshot} is {snapshot_gib} GiB and this volume asks for {wanted_gib} GiB; a volume made from a snapshot is at least as big as the snapshot"
    )]
    SmallerThanItsSnapshot {
        snapshot: String,
        snapshot_gib: u64,
        wanted_gib: u64,
    },
    /// No backend clones between pools without reading and writing every
    /// block, and none of them do it behind one `lvcreate`.
    #[error(
        "{snapshot} is in {snapshot_pool} and this volume would be in {pool}; a volume is cloned from a snapshot by the pool that holds it"
    )]
    AnotherPool {
        snapshot: String,
        snapshot_pool: String,
        pool: String,
    },
    #[error("a volume is created from an image or from a snapshot, not from both")]
    TwoSources,
}

/// Whether this volume may be copied.
pub fn may_snapshot(volume: &Volume) -> Result<(), Refusal> {
    let name = volume.meta.name.to_string();
    // On its way out and would be pinned by the copy: the finalizer a snapshot
    // puts on its source would be added to an object nobody would ever release
    // it from.
    if volume.meta.is_deleting() {
        return Err(Refusal::SourceGoing { volume: name });
    }
    // The pool's own report, not the ask. A volume whose spec says 100 GiB and
    // whose pool has not made it yet has nothing in it to copy, and the copy
    // would fail on the backend one pass later.
    if !volume.status.provisioned {
        return Err(Refusal::SourceNotProvisioned { volume: name });
    }
    Ok(())
}

/// Whether this volume may be created as asked, given the snapshot it names.
///
/// `from` is the snapshot `spec.source_snapshot` points at, already read.
/// Passing it in keeps this pure — and lets the API answer at the moment
/// somebody clicks rather than after the object exists.
pub fn may_create_volume(spec: &VolumeSpec, from: Option<&Snapshot>) -> Result<(), Refusal> {
    let has_image = spec.source_image.as_deref().is_some_and(|i| !i.is_empty());
    let has_snapshot = spec
        .source_snapshot
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if has_image && has_snapshot {
        return Err(Refusal::TwoSources);
    }
    let Some(snapshot) = from else {
        return Ok(());
    };
    let name = snapshot.meta.name.to_string();
    if !snapshot.status.taken {
        return Err(Refusal::NotTakenYet { snapshot: name });
    }
    if snapshot.spec.pool != spec.pool {
        return Err(Refusal::AnotherPool {
            snapshot: name,
            snapshot_pool: snapshot.spec.pool.clone(),
            pool: spec.pool.clone(),
        });
    }
    // Bigger is ordinary — a volume is grown, and growing it at the moment it
    // is made costs nothing. Smaller is the clone not fitting.
    if spec.size_gib < snapshot.status.size_gib {
        return Err(Refusal::SmallerThanItsSnapshot {
            snapshot: name,
            snapshot_gib: snapshot.status.size_gib,
            wanted_gib: spec.size_gib,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::{
            POOL_RELEASE_FINALIZER, Resource, SnapshotSpec, SnapshotStatus, VolumeSpec,
            VolumeStatus,
        },
    };

    fn volume(size_gib: u64) -> Volume {
        let mut v = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/volumes/data-1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            VolumeSpec {
                size_gib,
                pool: "pool-a".into(),
                encryption_key: None,
                source_image: None,
                source_snapshot: None,
            },
            VolumeStatus::default(),
        );
        v.meta.generation = 1;
        v.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
        v
    }

    /// A snapshot of `data-1`, under it, as every stored one is.
    fn snapshot(id: &str) -> Snapshot {
        let mut s = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("projects/p1/volumes/data-1/snapshots/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            SnapshotSpec {
                pool: "pool-a".into(),
            },
            SnapshotStatus::default(),
        );
        s.meta.generation = 1;
        s.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
        s
    }

    const NOTHING: SeenInPool = SeenInPool {
        exists: false,
        gib: 0,
        snapshots: 0,
    };

    fn there(gib: u64) -> SeenInPool {
        SeenInPool {
            exists: true,
            gib,
            snapshots: 0,
        }
    }

    #[test]
    fn a_volume_nobody_has_made_yet_is_provisioned_once() {
        let v = volume(100);
        assert_eq!(
            reconcile_volume(&v, NOTHING),
            vec![VolumeAction::Provision {
                volume: "projects/p1/volumes/data-1".into(),
                gib: 100,
                source: VolumeSource::Blank,
                encryption_key: None,
            }]
        );
        // And once it is there at the asked size, a second pass asks for
        // nothing — the property that keeps a settled cell silent.
        assert!(reconcile_volume(&v, there(100)).is_empty());
    }

    #[test]
    fn a_volume_is_grown_but_never_shrunk() {
        // The one rule this file exists for. Growing is arithmetic; shrinking
        // throws away whatever was past the new end, and nothing recovers it.
        let v = volume(200);
        assert_eq!(
            reconcile_volume(&v, there(100)),
            vec![VolumeAction::Grow {
                volume: "projects/p1/volumes/data-1".into(),
                to_gib: 200
            }]
        );

        let smaller = volume(50);
        assert!(
            reconcile_volume(&smaller, there(100)).is_empty(),
            "a volume was shrunk, which destroys data"
        );

        // …and it says so, rather than sitting there looking converged.
        let c = volume_condition(&smaller, there(100));
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "WillNotShrink");
        assert!(c.message.contains("100 GiB"), "{}", c.message);
        assert!(c.message.contains("copy"), "{}", c.message);
    }

    #[test]
    fn the_bytes_go_before_the_object_does() {
        let mut v = volume(100);
        v.meta.deleted_at = Some(Timestamp::now());

        assert_eq!(
            reconcile_volume(&v, there(100)),
            vec![VolumeAction::Destroy {
                volume: "projects/p1/volumes/data-1".into()
            }],
            "the finalizer went before the storage did"
        );
        // Once the pool can no longer see it there is nothing left to do on the
        // backend at all — the finalizer is a controller's business, and a
        // resync over a destroyed volume costs nothing.
        assert!(reconcile_volume(&v, NOTHING).is_empty());
    }

    #[test]
    fn what_the_pool_sees_beats_what_the_object_claims() {
        // A volume whose backing store was deleted underneath the platform.
        // Believing `status.provisioned` here is how it stays "ready" forever
        // while every guest that opens it fails.
        let mut v = volume(100);
        v.status.provisioned = true;
        v.status.actual_size_gib = 100;
        assert!(
            matches!(
                reconcile_volume(&v, NOTHING).as_slice(),
                [VolumeAction::Provision { .. }]
            ),
            "a volume that is gone from the pool was reported as still there"
        );
        assert_eq!(volume_condition(&v, NOTHING).reason, "Provisioning");
    }

    #[test]
    fn a_bootable_volume_is_never_briefly_empty() {
        // Cloning the image is part of creating it, not a step afterwards: a
        // volume that exists blank for one pass is a volume a guest can be
        // started from.
        let mut v = volume(20);
        v.spec.source_image = Some("projects/p1/images/sha256-abc".into());
        v.spec.encryption_key = Some("projects/p1/keys/k1".into());
        assert_eq!(
            reconcile_volume(&v, NOTHING),
            vec![VolumeAction::Provision {
                volume: "projects/p1/volumes/data-1".into(),
                gib: 20,
                source: VolumeSource::Image("projects/p1/images/sha256-abc".into()),
                encryption_key: Some("projects/p1/keys/k1".into()),
            }]
        );
    }

    #[test]
    fn a_volume_made_from_a_snapshot_is_cloned_from_it_at_creation() {
        // The same rule as an image and for the same reason: the clone is part
        // of creating the volume, so there is no pass in which a volume made
        // from a snapshot is a blank one somebody could boot or attach.
        let mut v = volume(20);
        v.spec.source_snapshot = Some("projects/p1/volumes/data-1/snapshots/nightly".into());
        assert_eq!(
            reconcile_volume(&v, NOTHING),
            vec![VolumeAction::Provision {
                volume: "projects/p1/volumes/data-1".into(),
                gib: 20,
                source: VolumeSource::Snapshot(
                    "projects/p1/volumes/data-1/snapshots/nightly".into()
                ),
                encryption_key: None,
            }]
        );
    }

    // ---- snapshots -------------------------------------------------------

    #[test]
    fn a_volume_with_copies_is_not_destroyed_underneath_them() {
        // The failure: an operator deletes a volume they believe they have
        // backups of, and the backups go with it — quietly, because a delta
        // snapshot is read through its source.
        let mut v = volume(100);
        v.meta.deleted_at = Some(Timestamp::now());
        let held = SeenInPool {
            exists: true,
            gib: 100,
            snapshots: 2,
        };
        assert!(
            reconcile_volume(&v, held).is_empty(),
            "a volume was destroyed with copies still hanging off it"
        );

        let c = volume_condition(&v, held);
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "SnapshotsDependOnIt");
        assert!(c.message.contains('2'), "{}", c.message);

        // …and once they are gone, the delete continues on its own. Nothing
        // had to be asked for twice.
        assert_eq!(
            reconcile_volume(&v, there(100)),
            vec![VolumeAction::Destroy {
                volume: "projects/p1/volumes/data-1".into()
            }]
        );
    }

    #[test]
    fn a_snapshot_nobody_has_taken_yet_is_taken_once() {
        let s = snapshot("nightly");
        assert_eq!(
            reconcile_snapshot(&s, SeenSnapshot::default()),
            vec![SnapshotAction::Take {
                snapshot: "projects/p1/volumes/data-1/snapshots/nightly".into(),
                volume: "projects/p1/volumes/data-1".into(),
            }],
            "the source is read from the name, because that is where it lives"
        );
        // Settled: the second pass over a copy that is there asks for nothing.
        assert!(
            reconcile_snapshot(
                &s,
                SeenSnapshot {
                    exists: true,
                    gib: 100
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn a_snapshot_that_vanished_is_never_taken_again() {
        // The whole reason this is not `reconcile_volume` with a different
        // noun. A volume that disappeared is made again; a copy that
        // disappeared must not be, because a copy made now is a copy of a
        // different moment — and it would be restored under the name of the
        // one somebody wanted.
        let mut s = snapshot("nightly");
        s.status.taken = true;
        s.status.size_gib = 100;
        assert!(
            reconcile_snapshot(&s, SeenSnapshot::default()).is_empty(),
            "yesterday's snapshot was re-taken with today's data"
        );

        let c = snapshot_condition(&s, SeenSnapshot::default());
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "Vanished");
        assert!(c.message.contains("different moment"), "{}", c.message);
    }

    #[test]
    fn the_copy_goes_before_the_object_does_and_the_volume_is_untouched() {
        let mut s = snapshot("nightly");
        s.status.taken = true;
        s.meta.deleted_at = Some(Timestamp::now());
        let held = SeenSnapshot {
            exists: true,
            gib: 100,
        };
        assert_eq!(
            reconcile_snapshot(&s, held),
            vec![SnapshotAction::Destroy {
                snapshot: "projects/p1/volumes/data-1/snapshots/nightly".into()
            }]
        );
        // Nothing about the source is in that action: deleting a copy is not a
        // thing that happens to the volume.
        assert!(reconcile_snapshot(&s, SeenSnapshot::default()).is_empty());
    }

    #[test]
    fn a_snapshot_that_is_not_under_a_volume_copies_nothing_and_says_why() {
        // The API refuses this name at create. If one is ever in the store —
        // written by hand, or by a version of the API that did not check —
        // guessing which volume was meant is how the wrong bytes end up under
        // a trusted name.
        let mut orphan = snapshot("nightly");
        orphan.meta.name = ResourceName::parse("projects/p1/snapshots/nightly").unwrap();
        assert!(reconcile_snapshot(&orphan, SeenSnapshot::default()).is_empty());
        assert_eq!(
            snapshot_condition(&orphan, SeenSnapshot::default()).reason,
            "NoSource"
        );
        assert!(source_volume(&orphan).is_none());
    }

    #[test]
    fn what_a_snapshot_says_while_it_is_being_made_is_an_observation() {
        let s = snapshot("nightly");
        assert_eq!(
            snapshot_condition(&s, SeenSnapshot::default()).reason,
            "Taking"
        );
        let c = snapshot_condition(
            &s,
            SeenSnapshot {
                exists: true,
                gib: 100,
            },
        );
        assert_eq!(c.status, ConditionStatus::True);
        assert!(c.message.contains("100 GiB"), "{}", c.message);
        assert!(c.message.contains("pool-a"), "{}", c.message);
    }

    #[test]
    fn a_copy_of_something_that_is_not_there_yet_is_refused() {
        // Both of these are answerable before anything is written, and both
        // are backend errors on an existing object if they are not asked here.
        let mut v = volume(100);
        assert_eq!(
            may_snapshot(&v),
            Err(Refusal::SourceNotProvisioned {
                volume: "projects/p1/volumes/data-1".into()
            })
        );

        v.status.provisioned = true;
        assert!(may_snapshot(&v).is_ok());

        v.meta.deleted_at = Some(Timestamp::now());
        assert_eq!(
            may_snapshot(&v),
            Err(Refusal::SourceGoing {
                volume: "projects/p1/volumes/data-1".into()
            }),
            "a copy was taken of a volume on its way out, which would pin it forever"
        );
    }

    #[test]
    fn a_volume_made_from_a_snapshot_is_refused_before_it_exists() {
        let mut s = snapshot("nightly");
        let mut spec = VolumeSpec {
            size_gib: 100,
            pool: "pool-a".into(),
            source_snapshot: Some(s.meta.name.to_string()),
            ..Default::default()
        };

        // Not taken yet: there is nothing to clone, and the pool would fail on
        // it one pass later with a sentence of its own.
        assert!(matches!(
            may_create_volume(&spec, Some(&s)),
            Err(Refusal::NotTakenYet { .. })
        ));

        s.status.taken = true;
        s.status.size_gib = 100;
        assert!(may_create_volume(&spec, Some(&s)).is_ok());

        // Bigger is ordinary — a volume is grown.
        spec.size_gib = 200;
        assert!(may_create_volume(&spec, Some(&s)).is_ok());

        // Smaller is the clone not fitting in what it is written into.
        spec.size_gib = 50;
        assert_eq!(
            may_create_volume(&spec, Some(&s)),
            Err(Refusal::SmallerThanItsSnapshot {
                snapshot: "projects/p1/volumes/data-1/snapshots/nightly".into(),
                snapshot_gib: 100,
                wanted_gib: 50,
            })
        );

        spec.size_gib = 100;
        spec.pool = "pool-b".into();
        assert!(
            matches!(
                may_create_volume(&spec, Some(&s)),
                Err(Refusal::AnotherPool { .. })
            ),
            "a pool was asked to clone a snapshot it does not hold"
        );

        spec.pool = "pool-a".into();
        spec.source_image = Some("projects/p1/images/sha256-abc".into());
        assert_eq!(
            may_create_volume(&spec, Some(&s)),
            Err(Refusal::TwoSources),
            "a volume was asked to come from two places at once"
        );
    }

    #[test]
    fn a_spec_that_somehow_names_two_sources_clones_the_newer_one() {
        // Unreachable through the API, which refuses it. What matters is that
        // the fallback cannot regress a volume to an installer image that the
        // snapshot's own lineage has long since moved past.
        let spec = VolumeSpec {
            source_image: Some("projects/p1/images/sha256-abc".into()),
            source_snapshot: Some("projects/p1/volumes/data-1/snapshots/nightly".into()),
            ..Default::default()
        };
        assert_eq!(
            VolumeSource::of(&spec),
            VolumeSource::Snapshot("projects/p1/volumes/data-1/snapshots/nightly".into())
        );
        // An empty string is an unset reference everywhere else in this API,
        // and a create that filled a spec from its defaults sends exactly that.
        let blank = VolumeSpec {
            source_image: Some(String::new()),
            source_snapshot: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(VolumeSource::of(&blank), VolumeSource::Blank);
    }
}
