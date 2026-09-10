//! The guard on a backup, so deleting the record takes the bytes with it.
//!
//! Modelled on [`crate::snapshot`], and for the same reason: the object and the
//! thing it stands for live in two different places, and a delete that removes
//! only the first leaves the second behind with nothing pointing at it.
//!
//! Before this, nothing held a backup at all. `keep: 7` on a schedule expired
//! the *record* and left the file, so a nightly copy of a 500 GiB volume added
//! half a terabyte of unreferenced data to the target every day, for ever —
//! uncounted, unreachable, and invisible once the record was gone.
//!
//! This controller writes exactly one object, the backup, and only its `meta`.
//! The bytes are the pool agent's to remove; what happens here is the waiting.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    backup::{BackupSpec, BackupStatus},
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{Backup, POOL_RELEASE_FINALIZER},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "backup";

pub struct BackupController {
    backups: TypedStore<BackupSpec, BackupStatus>,
}

impl BackupController {
    pub fn new(backups: TypedStore<BackupSpec, BackupStatus>) -> Self {
        Self { backups }
    }
}

/// Whether the agent has said the target holds nothing of this copy any more.
fn agent_has_let_go(backup: &Backup) -> bool {
    condition(&backup.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
}

impl Reconciler for BackupController {
    type Spec = BackupSpec;
    type Status = BackupStatus;

    fn name(&self) -> &'static str {
        "backup"
    }

    async fn reconcile(&self, name: &str, object: Option<&Backup>) -> Result<()> {
        let Some(backup) = object else {
            return Ok(());
        };
        match finalizer_step(&backup.meta, POOL_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                // Before the agent can be asked to make the copy. Added
                // afterwards, there would be a window in which a delete takes
                // the record and leaves the bytes — which is the whole failure
                // this controller exists to close.
                let mut next = backup.clone();
                next.meta.add_finalizer(POOL_RELEASE_FINALIZER);
                self.backups.update(&next, &Writer::controller(WHO)).await?;
                Ok(())
            }
            FinalizerStep::Wait => {
                if !backup.meta.is_deleting() || !agent_has_let_go(backup) {
                    return Ok(());
                }
                let mut next = backup.clone();
                next.meta.remove_finalizer(POOL_RELEASE_FINALIZER);
                self.backups.update(&next, &Writer::controller(WHO)).await?;
                info!(backup = name, "the target let go; the guard is off");
                Ok(())
            }
            FinalizerStep::Delete => {
                // Conditional on the revision, so a backup that gained a
                // finalizer between the read and now survives instead of being
                // torn out from under whoever added it.
                self.backups
                    .delete(name, backup.meta.revision, &Writer::controller(WHO))
                    .await?;
                info!(backup = name, "gone");
                Ok(())
            }
        }
    }
}
