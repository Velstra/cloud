//! Turning a guest you built by hand into an image you can stamp out.
//!
//! ## The workflow this is for
//!
//! Start a guest, install what you need, get it right, then capture it. Every
//! guest made from the result starts where that one left off. It is the thing
//! people actually do with a hypervisor, and until now this platform could only
//! boot images somebody fetched from elsewhere.
//!
//! ## Why it is a resource and not a verb
//!
//! An image's **name carries its digest** — that is what makes fetching one
//! verifiable, and the agent refuses any image whose name does not. So the
//! image cannot be named until its bytes exist, which means "capture" cannot be
//! a call that returns an image.
//!
//! It is therefore shaped like a migration: an ask, an agent that does the work
//! and reports what it produced, and a controller that makes the object which
//! follows. Nothing is ever "in progress" — a capture that has not finished is
//! an ordinary object with `digest` unset.
//!
//! ## Why a running guest is refused
//!
//! A disk copied out from under a running machine is crash-consistent at best:
//! the filesystem journal is mid-write and the database is mid-transaction.
//! That is survivable for a backup, which is read once in an emergency by
//! somebody who knows what happened. It is **not** survivable for a template,
//! which is stamped out a hundred times by people who assume it is clean — and
//! the corruption arrives a hundred times, later, with nothing pointing back
//! here.
//!
//! So: stop the guest first. The refusal says so, and says what to use instead
//! if what you wanted was a copy of a live machine.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// "Make an image out of this guest."
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureSpec {
    /// The guest whose disk is copied.
    pub instance: String,
    /// Where the bytes go — a backup target, which is simply a path an agent
    /// can reach. The image's `source_url` then points into it, so any node
    /// that can reach the same path can fetch the image.
    pub target: String,
    /// The node holding the guest, derived by the API rather than asked for.
    ///
    /// It is the assignee: only the machine with the disk can copy it. Without
    /// this the object is assigned to nobody and the access rule refuses every
    /// agent that tries to claim it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// What the resulting image should be called, before its digest is known.
    ///
    /// A person's name for it — `debian-13-golden` — which becomes part of the
    /// image's id alongside the digest. Without it every captured image would
    /// be named for a hash, and a list of hashes is not something anybody
    /// chooses from.
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
    /// The node that has claimed this capture and is doing the work.
    pub node: Option<String>,
    /// What the bytes hashed to, once they exist. `None` while the copy is
    /// still being made — which is the only "in progress" there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
}

// There is deliberately no `image` field here.
//
// It would be the obvious one — "which image did this become" — and it cannot
// exist honestly. The node owns this status and the node does not create the
// image; a controller does, and the access rule refuses a controller writing
// somebody else's status. Two writers on one object is the thing this platform
// is built to prevent, so the rule is right and the field is wrong.
//
// The answer is derived instead: [`image_id`] from the label and the digest,
// both of which are already here. A derived link cannot go stale, which is
// better than the stored one would have been — a capture whose image was
// deleted afterwards would have gone on naming it.

/// Why a guest cannot be captured.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// It is running.
    ///
    /// The refusal this exists for. A disk copied from under a running machine
    /// is crash-consistent — survivable for a backup, read once by somebody who
    /// knows what happened, and not survivable for a template that will be
    /// stamped out a hundred times by people who assume it is clean.
    #[error(
        "{instance} is running, and a disk copied from under a running machine is \
         crash-consistent — which a template stamped out a hundred times must not be. Stop it \
         first. If what you want is a copy of a live guest, take a backup: that is read once, \
         by somebody who knows what happened."
    )]
    StillRunning { instance: String },
    /// It is not placed, so there is no disk anywhere to copy.
    #[error("{instance} is not on a node, so there is no disk to copy")]
    NotPlaced { instance: String },
    /// It is being deleted.
    #[error("{instance} is being deleted")]
    Deleting { instance: String },
    /// The target cannot be written.
    #[error("{target} is not accepting, or the agent cannot reach it")]
    TargetUnusable { target: String },
}

/// One guest, as this decision sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestView {
    pub name: String,
    pub running: bool,
    pub node: Option<String>,
    pub deleting: bool,
}

/// Whether this guest may be captured, and to where.
///
/// Answered before anything starts, because every one of these is knowable in
/// advance — and a capture that fails after copying forty gibibytes has spent
/// real time saying something that was true before it began.
pub fn may_capture(guest: &GuestView, target_usable: bool, target: &str) -> Result<(), Refusal> {
    if guest.deleting {
        return Err(Refusal::Deleting {
            instance: guest.name.clone(),
        });
    }
    if guest.node.is_none() {
        return Err(Refusal::NotPlaced {
            instance: guest.name.clone(),
        });
    }
    if guest.running {
        return Err(Refusal::StillRunning {
            instance: guest.name.clone(),
        });
    }
    if !target_usable {
        return Err(Refusal::TargetUnusable {
            target: target.to_string(),
        });
    }
    Ok(())
}

/// The id for the image a finished capture becomes.
///
/// The label a person chose, then the digest — both, and in that order. The
/// digest is what makes fetching verifiable and the agent refuses a name
/// without one; the label is what makes a list of images something a person can
/// choose from rather than a wall of hashes.
///
/// `sha256:abc…` is written `sha256-abc…`: a resource id may not carry a colon,
/// and the agent's own reader expects that spelling.
pub fn image_id(label: &str, digest: &str) -> String {
    format!("{label}-{}", digest.replace(':', "-"))
}

/// Where the bytes of a captured image live, for an agent to fetch.
///
/// `file://` into the target, because a target is exactly "a path an agent can
/// reach" — the same property backups rely on. Any node that can reach it can
/// fetch the image; one that cannot fails the pull with a sentence naming the
/// path, which is a mount somebody can go and fix.
pub fn image_url(target_path: &str, digest: &str) -> String {
    format!(
        "file://{}/{}",
        target_path.trim_end_matches('/'),
        digest.replace(':', "-")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped() -> GuestView {
        GuestView {
            name: "projects/p1/instances/golden".into(),
            running: false,
            node: Some("node-a".into()),
            deleting: false,
        }
    }

    /// The refusal the module exists for, and the alternative it offers.
    #[test]
    fn a_running_guest_is_refused_and_told_what_to_use_instead() {
        let mut up = stopped();
        up.running = true;
        let Err(why @ Refusal::StillRunning { .. }) = may_capture(&up, true, "t") else {
            panic!("a running guest was captured into a template");
        };
        let said = why.to_string();
        assert!(said.contains("crash-consistent"), "{said}");
        // The point is not that it is refused, it is that the person is told
        // which tool does what they wanted.
        assert!(said.contains("take a backup"), "{said}");

        assert_eq!(may_capture(&stopped(), true, "t"), Ok(()));
    }

    #[test]
    fn a_guest_with_no_node_has_no_disk_to_copy() {
        let mut nowhere = stopped();
        nowhere.node = None;
        assert!(matches!(
            may_capture(&nowhere, true, "t"),
            Err(Refusal::NotPlaced { .. })
        ));
    }

    #[test]
    fn a_guest_on_its_way_out_is_not_captured() {
        let mut going = stopped();
        going.deleting = true;
        assert!(matches!(
            may_capture(&going, true, "t"),
            Err(Refusal::Deleting { .. })
        ));
    }

    #[test]
    fn a_target_that_cannot_be_written_is_refused_before_anything_is_copied() {
        assert!(matches!(
            may_capture(&stopped(), false, "backup-targets/gone"),
            Err(Refusal::TargetUnusable { .. })
        ));
    }

    /// The name carries both halves: the digest so a fetch can be verified, the
    /// label so a person can choose from a list.
    #[test]
    fn an_image_id_carries_the_digest_and_a_name_a_person_chose() {
        let id = image_id("debian-13-golden", "sha256:abc123");
        assert!(id.starts_with("debian-13-golden-"), "{id}");
        assert!(id.contains("sha256-abc123"), "{id}");
        // A colon would make it not a resource id, and the agent's own reader
        // expects the dashed spelling.
        assert!(!id.contains(':'), "{id}");
    }

    #[test]
    fn the_url_points_into_the_target_whatever_way_its_path_was_written() {
        assert_eq!(
            image_url("/srv/backups", "sha256:abc"),
            "file:///srv/backups/sha256-abc"
        );
        // A trailing slash is somebody's typing, not a different place.
        assert_eq!(
            image_url("/srv/backups/", "sha256:abc"),
            "file:///srv/backups/sha256-abc"
        );
    }
}
