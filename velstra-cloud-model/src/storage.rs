//! Bringing a volume into existence, and refusing to destroy data by accident.
//!
//! A volume is the first object in this platform whose owner is not a node. It
//! lives in a **pool** — an LVM group, a ZFS dataset, a Ceph pool — and the pool
//! is the party that can see whether the bytes are there. So `spec.pool` is the
//! assignment and `status.pool` is the claim, which is the same two-field
//! ownership dance as an instance and its node, reached through the same access
//! rule. Nothing new was needed to make storage work; what was missing was
//! *anybody at all* being allowed to write a volume's status.
//!
//! # The rule this file exists to enforce
//!
//! **A volume is never shrunk.** Growing one is arithmetic; shrinking one throws
//! away whatever was past the new end, and no amount of "the operator asked for
//! it" makes that recoverable. So a spec smaller than what exists is reported as
//! a refusal on the object and nothing happens — the volume keeps its size and
//! says why. An operator who really wants a smaller volume makes a smaller one
//! and copies, which is the operation they actually meant.
//!
//! Everything else here follows the same discipline as the rest of the model:
//! the functions are pure, they are handed what the pool observed rather than
//! what anybody remembers, and running them twice over a settled volume asks for
//! nothing.

use crate::{
    meta::{Condition, ConditionStatus},
    resources::Volume,
};

/// What a pool agent should do about one volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeAction {
    /// Create the backing store. `source_image` means clone it rather than
    /// leaving it blank, so a bootable volume never briefly exists empty.
    Provision {
        volume: String,
        gib: u64,
        source_image: Option<String>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeenInPool {
    pub exists: bool,
    pub gib: u64,
}

/// The whole decision, from the ask and what the pool can see.
pub fn reconcile_volume(volume: &Volume, seen: SeenInPool) -> Vec<VolumeAction> {
    let name = volume.meta.name.to_string();

    if volume.meta.is_deleting() {
        if seen.exists {
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
            source_image: volume.spec.source_image.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::{POOL_RELEASE_FINALIZER, Resource, VolumeSpec, VolumeStatus},
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
            },
            VolumeStatus::default(),
        );
        v.meta.generation = 1;
        v.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
        v
    }

    const NOTHING: SeenInPool = SeenInPool {
        exists: false,
        gib: 0,
    };

    #[test]
    fn a_volume_nobody_has_made_yet_is_provisioned_once() {
        let v = volume(100);
        assert_eq!(
            reconcile_volume(&v, NOTHING),
            vec![VolumeAction::Provision {
                volume: "projects/p1/volumes/data-1".into(),
                gib: 100,
                source_image: None,
                encryption_key: None,
            }]
        );
        // And once it is there at the asked size, a second pass asks for
        // nothing — the property that keeps a settled cell silent.
        assert!(
            reconcile_volume(
                &v,
                SeenInPool {
                    exists: true,
                    gib: 100
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn a_volume_is_grown_but_never_shrunk() {
        // The one rule this file exists for. Growing is arithmetic; shrinking
        // throws away whatever was past the new end, and nothing recovers it.
        let v = volume(200);
        assert_eq!(
            reconcile_volume(
                &v,
                SeenInPool {
                    exists: true,
                    gib: 100
                }
            ),
            vec![VolumeAction::Grow {
                volume: "projects/p1/volumes/data-1".into(),
                to_gib: 200
            }]
        );

        let smaller = volume(50);
        assert!(
            reconcile_volume(
                &smaller,
                SeenInPool {
                    exists: true,
                    gib: 100
                }
            )
            .is_empty(),
            "a volume was shrunk, which destroys data"
        );

        // …and it says so, rather than sitting there looking converged.
        let c = volume_condition(
            &smaller,
            SeenInPool {
                exists: true,
                gib: 100,
            },
        );
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "WillNotShrink");
        assert!(c.message.contains("100 GiB"), "{}", c.message);
        assert!(c.message.contains("copy"), "{}", c.message);
    }

    #[test]
    fn the_bytes_go_before_the_object_does() {
        let mut v = volume(100);
        v.meta.deleted_at = Some(Timestamp::now());
        let there = SeenInPool {
            exists: true,
            gib: 100,
        };

        assert_eq!(
            reconcile_volume(&v, there),
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
                source_image: Some("projects/p1/images/sha256-abc".into()),
                encryption_key: Some("projects/p1/keys/k1".into()),
            }]
        );
    }
}
