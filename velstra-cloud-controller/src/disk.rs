//! The disks a guest asked for, made real once it has a node.
//!
//! An attachment is a join — this guest, that volume, opened by that node — and
//! it is right in the model for the reason [`crate::attachment`] gives: a field
//! on either side has two writers and cannot express "asked to let go, has not
//! yet". It is wrong in a *form*, and that is a different question. Asked for a
//! machine with a second disk, a customer had to make the volume, wait for the
//! guest to be placed somewhere, read which node that was, and only then create
//! an attachment naming all three. Three steps, one of them a wait, to say "this
//! machine has this disk".
//!
//! So `InstanceSpec.volumes` names volumes and this makes the attachments.
//!
//! **Why a controller and not the create path.** An attachment names the node
//! holding the guest — derived, so that an attachment naming the wrong node is
//! unrepresentable — and at the moment a guest is created there is no such node.
//! The API cannot answer this request; only something watching can. Which makes
//! it level-triggered by construction: a disk named before the guest is placed
//! attaches when it is placed, a disk added later attaches on the next pass, and
//! nothing here is a step in a sequence that can be half-done.
//!
//! **Why the label.** The rule is "make what `spec.volumes` lists, remove what
//! it no longer lists", and applied to every attachment that rule would tear out
//! one somebody made by hand. On a mounted filesystem that is not an
//! inconvenience, it is data. So this controller marks its own work with
//! [`MINTED_FOR`] and touches nothing else, for ever.

use tracing::info;
use velstra_cloud_model::{
    Resource,
    access::Writer,
    meta::{Meta, ResourceName},
    resources::{
        Attachment, AttachmentSpec, AttachmentStatus, Instance, InstanceSpec, InstanceStatus,
        MINTED_FOR,
    },
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "disk";

pub struct DiskController {
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    /// Read to answer one question: is the guest this attachment was made for
    /// still there? See [`DiskController::collect_the_abandoned`].
    instances: TypedStore<InstanceSpec, InstanceStatus>,
}

impl DiskController {
    pub fn new(
        attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
    ) -> Self {
        Self {
            attachments,
            instances,
        }
    }
}

/// The attachment a given instance/volume pair gets.
///
/// Derived rather than minted, and that is what makes this idempotent: two
/// passes racing produce the same name, so the loser's create is an
/// `AlreadyExists` — which is the right answer — instead of a second attachment
/// for one disk. The evacuation controller derives a migration's name for the
/// same reason.
pub fn attachment_name(instance: &str, volume: &str) -> Option<String> {
    let i = ResourceName::parse(instance).ok()?;
    let v = ResourceName::parse(volume).ok()?;
    Some(format!("{}/attachments/{}-{}", i.parent()?, i.id(), v.id()))
}

impl DiskController {
    /// The attachments this controller made for one guest.
    async fn mine(&self, instance: &str) -> Result<Vec<Attachment>> {
        Ok(self
            .attachments
            .list()
            .await?
            .into_iter()
            .filter(|a| a.meta.labels.get(MINTED_FOR).is_some_and(|f| f == instance))
            .collect())
    }

    /// Attachments this controller made for guests that are no longer there.
    ///
    /// Keyed on the attachments rather than on the instances, because that is
    /// the only direction that finds them: a resync enqueues the objects that
    /// exist, so an instance deleted while this controller was not running is an
    /// instance nothing will ever hand it again — and its attachment sits there
    /// for ever, holding a volume `InUse` that nobody can then delete.
    ///
    /// Cheap on a settled cell: one collection read per pass, no writes.
    async fn collect_the_abandoned(&self) -> Result<()> {
        let mut gone: Vec<String> = Vec::new();
        for attachment in self.attachments.list().await? {
            let Some(guest) = attachment.meta.labels.get(MINTED_FOR) else {
                continue;
            };
            if attachment.meta.is_deleting() || gone.contains(guest) {
                continue;
            }
            if self.instances.get(guest).await?.is_none() {
                gone.push(guest.clone());
            }
        }
        for guest in gone {
            self.detach_all(&guest).await?;
        }
        Ok(())
    }

    /// Every disk this controller attached for a guest that no longer exists.
    ///
    /// Only its own, as everywhere else here: an attachment somebody made by
    /// hand is theirs even after the guest is gone, and removing it would be a
    /// detach nobody asked for.
    async fn detach_all(&self, instance: &str) -> Result<()> {
        for existing in self.mine(instance).await? {
            if existing.meta.is_deleting() {
                continue;
            }
            self.attachments
                .delete(
                    &existing.meta.name.to_string(),
                    existing.meta.revision,
                    &Writer::controller(WHO),
                )
                .await?;
            info!(
                instance,
                volume = %existing.spec.volume,
                "the guest is gone; letting its disk go"
            );
        }
        Ok(())
    }
}

impl Reconciler for DiskController {
    type Spec = InstanceSpec;
    type Status = InstanceStatus;

    fn name(&self) -> &'static str {
        WHO
    }

    async fn reconcile(&self, name: &str, object: Option<&Instance>) -> Result<()> {
        let Some(instance) = object else {
            // The guest is gone. Its disks go with it — nothing is writing to
            // them any more, and an attachment naming an instance that does not
            // exist is an object no agent will ever act on, holding its volume
            // `InUse` so that nobody can delete it either. Found live: a machine
            // deleted, its attachment left behind, its volume undeletable.
            //
            // This is the fast path, not the guarantee. A delete this controller
            // was not running to witness — it crashed, it had not been written
            // yet — leaves an attachment nothing will ever enqueue again, since
            // a resync walks the objects that *exist*. The sweep below is what
            // catches those.
            return self.detach_all(name).await;
        };
        // A guest on its way out takes its disks with it, now rather than once
        // the object is gone.
        //
        // The first version waited, reasoning that detaching under a running
        // teardown pulls a filesystem from beneath a process still writing to
        // it. That is right about detaching a disk from a *living* guest and
        // wrong here: this guest is being destroyed entirely, and waiting
        // deadlocks. The node stops the VM; `open_volumes` then reports nothing;
        // the attachment reads as `attached: false` while not deleting — and
        // `reconcile_attachment` answers that with `OpenVolume`, so the node
        // would spend for ever plugging a disk into a guest that no longer
        // exists.
        //
        // Deleting the attachment instead puts it in the branch that closes the
        // volume and releases its finalizer, which is the sequence this whole
        // pair of controllers is built around.
        if instance.meta.is_deleting() {
            return self.detach_all(name).await;
        }

        // Where it *is*, not where a scheduler wants it. During a migration
        // those differ for as long as the transfer takes, and an attachment
        // opened on the destination early is the two-nodes-one-image case that
        // eats data.
        let Some(node) = instance.status.node.clone() else {
            return Ok(());
        };

        self.collect_the_abandoned().await?;

        let mine = self.mine(name).await?;

        for volume in &instance.spec.volumes {
            let Some(attachment) = attachment_name(name, volume) else {
                continue;
            };
            if mine.iter().any(|a| a.meta.name.to_string() == attachment) {
                continue;
            }
            let Ok(parsed) = ResourceName::parse(&attachment) else {
                continue;
            };
            let mut meta = Meta::new(parsed, instance.meta.placement.clone());
            meta.labels.insert(MINTED_FOR.to_string(), name.to_string());
            let asked = Resource::new(
                meta,
                AttachmentSpec {
                    volume: volume.clone(),
                    instance: name.to_string(),
                    node: node.clone(),
                    // Filled in by the attachment controller once the pool has
                    // said where it put the bytes. Empty here on purpose: at
                    // this moment nobody knows, and guessing is the defect.
                    at: String::new(),
                    read_only: false,
                },
                AttachmentStatus::default(),
            );
            match self
                .attachments
                .create(&asked, &Writer::controller(WHO))
                .await
            {
                Ok(_) => info!(
                    instance = name,
                    volume = %volume,
                    node = %node,
                    "attaching a disk the guest asked for"
                ),
                // Another pass got there first. The derived name is what makes
                // that harmless rather than a duplicate.
                Err(e) if is_taken(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // Taken off the list: detach. Only ours — an attachment somebody made by
        // hand is theirs, and removing it would be a detach nobody asked for.
        for existing in &mine {
            if instance.spec.volumes.contains(&existing.spec.volume) {
                continue;
            }
            if existing.meta.is_deleting() {
                continue;
            }
            self.attachments
                .delete(
                    &existing.meta.name.to_string(),
                    existing.meta.revision,
                    &Writer::controller(WHO),
                )
                .await?;
            info!(
                instance = name,
                volume = %existing.spec.volume,
                "detaching a disk the guest no longer asks for"
            );
        }

        Ok(())
    }
}

fn is_taken(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Exists { .. }
        )
    )
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::meta::{Meta, Placement, ResourceName};
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    pub const GUEST: &str = "projects/p1/instances/db";

    pub fn by_hand(name: &str) -> Meta {
        meta(name)
    }

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    pub async fn cell() -> (
        DiskController,
        TypedStore<AttachmentSpec, AttachmentStatus>,
        TypedStore<InstanceSpec, InstanceStatus>,
    ) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let attachments = TypedStore::new(store.clone(), "cell-1", "attachments");
        let instances = TypedStore::new(store.clone(), "cell-1", "instances");
        (
            DiskController::new(attachments.clone(), instances.clone()),
            attachments,
            instances,
        )
    }

    /// A guest with disks, placed on a node.
    pub async fn guest(
        instances: &TypedStore<InstanceSpec, InstanceStatus>,
        volumes: &[&str],
        node: Option<&str>,
    ) -> Instance {
        guest_named(instances, GUEST, volumes, node).await
    }

    pub async fn guest_named(
        instances: &TypedStore<InstanceSpec, InstanceStatus>,
        name: &str,
        volumes: &[&str],
        node: Option<&str>,
    ) -> Instance {
        let object = Resource::new(
            meta(name),
            InstanceSpec {
                volumes: volumes.iter().map(|v| v.to_string()).collect(),
                ..InstanceSpec::default()
            },
            InstanceStatus {
                node: node.map(str::to_string),
                ..InstanceStatus::default()
            },
        );
        instances
            .create(&object, &Writer::controller("test"))
            .await
            .unwrap();
        object
    }

    #[tokio::test]
    async fn a_disk_a_guest_asked_for_is_attached_once_it_has_a_node() {
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], Some("nodes/n1")).await;

        disk.reconcile(GUEST, Some(&object)).await.unwrap();

        let made = attachments.list().await.unwrap();
        assert_eq!(made.len(), 1, "the disk was not attached");
        assert_eq!(made[0].spec.volume, "projects/p1/volumes/data");
        assert_eq!(made[0].spec.instance, GUEST);
        assert_eq!(
            made[0].spec.node, "nodes/n1",
            "an attachment is opened by the node holding the guest"
        );
        assert_eq!(
            made[0].meta.name.to_string(),
            "projects/p1/attachments/db-data",
            "the name is derived, so two passes racing cannot make two attachments"
        );
    }

    #[tokio::test]
    async fn a_second_pass_makes_nothing_new() {
        // The derived name is the whole reason this is safe to run on every
        // event: a repeat is an AlreadyExists, not a duplicate disk.
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], Some("nodes/n1")).await;
        for _ in 0..3 {
            disk.reconcile(GUEST, Some(&object)).await.unwrap();
        }
        assert_eq!(attachments.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unplaced_guest_waits_instead_of_making_a_meaningless_object() {
        // An attachment names the node holding the guest. Made before there is
        // one, it would name nothing, and no agent would ever pick it up.
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], None).await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();
        assert!(attachments.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn taking_a_disk_off_the_list_detaches_it() {
        let (disk, attachments, instances) = cell().await;
        let object = guest(
            &instances,
            &["projects/p1/volumes/data", "projects/p1/volumes/logs"],
            Some("nodes/n1"),
        )
        .await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();
        assert_eq!(attachments.list().await.unwrap().len(), 2);

        let mut fewer = object.clone();
        fewer.spec.volumes = vec!["projects/p1/volumes/data".to_string()];
        disk.reconcile(GUEST, Some(&fewer)).await.unwrap();

        let left = attachments.list().await.unwrap();
        let live: Vec<_> = left.iter().filter(|a| !a.meta.is_deleting()).collect();
        assert_eq!(live.len(), 1, "the removed disk was not detached");
        assert_eq!(live[0].spec.volume, "projects/p1/volumes/data");
    }

    #[tokio::test]
    async fn an_attachment_somebody_made_by_hand_is_never_torn_out() {
        // The rule is "make what the list names, remove what it does not", and
        // applied to every attachment that rule is a detach nobody asked for —
        // on a mounted filesystem, a destructive one. Unmarked work is theirs.
        let (disk, attachments, instances) = cell().await;
        attachments
            .create(
                &Resource::new(
                    meta("projects/p1/attachments/von-hand"),
                    AttachmentSpec {
                        volume: "projects/p1/volumes/wichtig".into(),
                        instance: GUEST.into(),
                        node: "nodes/n1".into(),
                        at: String::new(),
                        read_only: false,
                    },
                    AttachmentStatus::default(),
                ),
                &Writer::controller("test"),
            )
            .await
            .unwrap();

        // A guest that asks for nothing at all: the strongest form of "remove
        // what the list does not name".
        let object = guest(&instances, &[], Some("nodes/n1")).await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();

        let left = attachments.list().await.unwrap();
        assert_eq!(left.len(), 1, "somebody's own attachment was removed");
        assert!(!left[0].meta.is_deleting());
    }

    #[tokio::test]
    async fn a_guest_on_its_way_out_takes_its_disks_with_it() {
        // This asserted the opposite once, on the reasoning that detaching under
        // a running teardown pulls a filesystem from beneath a process still
        // writing to it. True of a living guest, false here — and the cost of
        // being wrong was a deadlock found live: the machine sat at "deleting"
        // for as long as anybody watched, because the attachment was waiting for
        // the instance and the instance was waiting for nothing at all.
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], Some("nodes/n1")).await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();

        let mut going = object.clone();
        going.meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp::now());
        disk.reconcile(GUEST, Some(&going)).await.unwrap();

        let left = attachments.list().await.unwrap();
        let live: Vec<_> = left.iter().filter(|a| !a.meta.is_deleting()).collect();
        assert!(
            live.is_empty(),
            "a guest being torn down kept its disks, which is the deadlock: {:?}",
            live.iter()
                .map(|a| a.meta.name.to_string())
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod when_the_guest_is_gone {
    use super::{tests::*, *};

    #[tokio::test]
    async fn its_disks_go_with_it() {
        // Found live: a machine deleted, its attachment left behind naming an
        // instance that no longer existed — an object no agent would ever act
        // on, holding its volume `InUse` so nobody could delete that either.
        //
        // The teardown path deliberately keeps the disks *while* the guest is
        // being torn down, because detaching under a running teardown pulls a
        // filesystem from beneath a process still writing to it. This is the
        // other end of that: once the guest is gone, so are they.
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], Some("nodes/n1")).await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();
        assert_eq!(attachments.list().await.unwrap().len(), 1);

        // `None` is what a reconcile is handed for an object that is no longer
        // in the store.
        disk.reconcile(GUEST, None).await.unwrap();

        let left = attachments.list().await.unwrap();
        let live: Vec<_> = left.iter().filter(|a| !a.meta.is_deleting()).collect();
        assert!(
            live.is_empty(),
            "a deleted guest left its disks attached: {:?}",
            live.iter()
                .map(|a| a.meta.name.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn somebody_elses_attachment_outlives_the_guest_too() {
        // Unmarked work is theirs, before the guest is gone and after.
        let (disk, attachments, _instances) = cell().await;
        attachments
            .create(
                &Resource::new(
                    by_hand("projects/p1/attachments/von-hand"),
                    AttachmentSpec {
                        volume: "projects/p1/volumes/wichtig".into(),
                        instance: GUEST.into(),
                        node: "nodes/n1".into(),
                        at: String::new(),
                        read_only: false,
                    },
                    AttachmentStatus::default(),
                ),
                &Writer::controller("test"),
            )
            .await
            .unwrap();

        disk.reconcile(GUEST, None).await.unwrap();

        let left = attachments.list().await.unwrap();
        assert_eq!(left.len(), 1);
        assert!(!left[0].meta.is_deleting());
    }
}

#[cfg(test)]
mod the_ones_nobody_witnessed {
    use super::{tests::*, *};

    #[tokio::test]
    async fn an_attachment_whose_guest_vanished_unnoticed_is_still_collected() {
        // The case the delete event does not cover, and the one that was found
        // live: three attachments naming instances that no longer existed, each
        // holding its volume `InUse` so nobody could delete those either.
        //
        // A resync enqueues the objects that *exist*. An instance deleted while
        // this controller was not running — it crashed, it had not been written
        // yet — is one nothing will ever hand it again, so the delete branch
        // never fires. Keyed on the attachments, it does.
        let (disk, attachments, instances) = cell().await;
        let object = guest(&instances, &["projects/p1/volumes/data"], Some("nodes/n1")).await;
        disk.reconcile(GUEST, Some(&object)).await.unwrap();
        assert_eq!(attachments.list().await.unwrap().len(), 1);

        // The guest goes without anybody telling this controller.
        let stored = instances.get(GUEST).await.unwrap().unwrap();
        instances
            .delete(GUEST, stored.meta.revision, &Writer::controller("test"))
            .await
            .unwrap();

        // Any pass at all, about any other guest.
        let other = "projects/p1/instances/andere";
        let elsewhere = guest_named(&instances, other, &[], Some("nodes/n1")).await;
        disk.reconcile(other, Some(&elsewhere)).await.unwrap();

        let left = attachments.list().await.unwrap();
        let live: Vec<_> = left.iter().filter(|a| !a.meta.is_deleting()).collect();
        assert!(
            live.is_empty(),
            "an abandoned attachment survived a sweep: {:?}",
            live.iter()
                .map(|a| a.meta.name.to_string())
                .collect::<Vec<_>>()
        );
    }
}
