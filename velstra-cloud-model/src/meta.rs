//! What every resource carries, and the rules that make the rest of the system
//! boring.
//!
//! Three invariants hold everywhere in this crate, and everything else follows
//! from them:
//!
//! 1. **One object, one writer.** A controller writes `spec`; the owning agent
//!    writes `status`. Never both. This is enforced by the types in
//!    [`crate::access`], not by convention.
//! 2. **No transient states.** There is no `PENDING_UPDATE`, no `BOOTING`, no
//!    `attaching`. There is what was asked for (`spec`), what is (`status`), and
//!    `generation`/`observed_generation` to say whether the second has caught up
//!    with the first. A controller that dies mid-flight leaves nothing behind to
//!    clean up, because it never wrote a state that means "in progress".
//! 3. **Level-triggered and idempotent.** Nothing is a command. Every actor asks
//!    "how should it be, how is it" and closes the gap, as often as it likes.

use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// An opaque store revision. Ordering is meaningful within one cell's store and
/// nowhere else — never persist it in a resource, never compare it across cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision(pub u64);

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Milliseconds since the epoch. Stored rather than a `SystemTime` so a
/// resource round-trips through JSON and protobuf without losing precision or
/// gaining a timezone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_millis() as u64,
        )
    }

    pub fn age(&self, now: Timestamp) -> Duration {
        Duration::from_millis(now.0.saturating_sub(self.0))
    }
}

/// Where a resource lives, in the two coordinates that can never be changed
/// afterwards.
///
/// The cell is the failure and scaling domain: a cell going down must never
/// take a region with it. It is also the one architectural decision that cannot
/// be retrofitted — a resource id that does not carry its cell cannot be routed
/// to the right store once there is more than one — so it is in the identity of
/// every object from the first commit, even while there is exactly one cell.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Placement {
    pub region: String,
    pub cell: String,
}

impl Placement {
    pub fn new(region: impl Into<String>, cell: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            cell: cell.into(),
        }
    }
}

/// An AIP-style resource name: `projects/p1/zones/z1/instances/i1`.
///
/// Kept as the parsed segments rather than a string, because every piece of the
/// system needs the parent (`projects/p1`) and the collection (`instances`) far
/// more often than it needs the whole thing spelled out.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResourceName {
    segments: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a resource name is collection/id pairs: {0:?}")]
    NotPairs(String),
    #[error("empty segment in {0:?}")]
    EmptySegment(String),
    #[error("segment {0:?} may hold only a-z, 0-9 and '-'")]
    BadCharacter(String),
}

impl ResourceName {
    /// Parse `projects/p1/instances/i1`. Rejects anything that is not an even
    /// number of non-empty, lowercase-safe segments — an id that has to be
    /// quoted is an id that will be mis-split by something downstream.
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let segments: Vec<String> = s.split('/').map(str::to_string).collect();
        if segments.len() < 2 || segments.len() % 2 != 0 {
            return Err(NameError::NotPairs(s.to_string()));
        }
        for seg in &segments {
            if seg.is_empty() {
                return Err(NameError::EmptySegment(s.to_string()));
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
            {
                return Err(NameError::BadCharacter(seg.clone()));
            }
        }
        Ok(Self { segments })
    }

    /// The collection this resource is in — `instances` for an instance.
    pub fn collection(&self) -> &str {
        &self.segments[self.segments.len() - 2]
    }

    /// The last id — `i1` for `projects/p1/instances/i1`.
    pub fn id(&self) -> &str {
        &self.segments[self.segments.len() - 1]
    }

    /// The parent name, or `None` at the top of the tree.
    pub fn parent(&self) -> Option<ResourceName> {
        if self.segments.len() <= 2 {
            return None;
        }
        Some(Self {
            segments: self.segments[..self.segments.len() - 2].to_vec(),
        })
    }

    /// The id of an ancestor collection: `project()` on
    /// `projects/p1/instances/i1` is `p1`. This is how a request is checked
    /// against the IAM tree without re-parsing strings everywhere.
    pub fn ancestor(&self, collection: &str) -> Option<&str> {
        self.segments
            .chunks(2)
            .find(|c| c[0] == collection)
            .map(|c| c[1].as_str())
    }

    pub fn project(&self) -> Option<&str> {
        self.ancestor("projects")
    }

    /// Whether this name sits under `prefix` — the containment test IAM
    /// inheritance and quota both need.
    pub fn is_under(&self, prefix: &ResourceName) -> bool {
        self.segments.len() >= prefix.segments.len()
            && self.segments[..prefix.segments.len()] == prefix.segments[..]
    }
}

impl fmt::Display for ResourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.segments.join("/"))
    }
}

impl std::str::FromStr for ResourceName {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The bookkeeping every resource carries.
///
/// `generation` counts changes an operator asked for; `status.observed_generation`
/// counts the ones an agent has seen and acted on. Their difference — and
/// nothing else — is what "still converging" means. A resource that reports
/// `observed_generation == generation` with a healthy condition is done, and one
/// that does not is drifting, with the reason on the object rather than in a log
/// somewhere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: ResourceName,
    /// Stable for the lifetime of this object even if the name is reused.
    pub uid: String,
    pub placement: Placement,
    /// Bumped by the API on every accepted change to `spec`. Never by an agent.
    pub generation: u64,
    pub created_at: Timestamp,
    /// Set once, when deletion is requested. Its presence is what makes
    /// finalizers run; the object stays visible until they are all gone, so a
    /// delete is never half-done behind an operator's back.
    pub deleted_at: Option<Timestamp>,
    /// Names of the parties that must release the object before it disappears.
    /// A volume is not gone until the node that had it open says so.
    pub finalizers: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// The store revision this copy was read at, used for compare-and-swap.
    /// Not part of the object's identity or its equality.
    #[serde(default)]
    pub revision: Revision,
}

impl Meta {
    pub fn new(name: ResourceName, placement: Placement) -> Self {
        Self {
            name,
            uid: uuid::Uuid::new_v4().to_string(),
            placement,
            generation: 1,
            created_at: Timestamp::now(),
            deleted_at: None,
            finalizers: Vec::new(),
            labels: BTreeMap::new(),
            revision: Revision::default(),
        }
    }

    pub fn is_deleting(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn has_finalizer(&self, who: &str) -> bool {
        self.finalizers.iter().any(|f| f == who)
    }

    pub fn add_finalizer(&mut self, who: &str) {
        if !self.has_finalizer(who) {
            self.finalizers.push(who.to_string());
        }
    }

    pub fn remove_finalizer(&mut self, who: &str) {
        self.finalizers.retain(|f| f != who);
    }
}

/// Whether a named aspect of a resource is true, false, or not yet known.
///
/// `Unknown` is deliberate and load-bearing: it is what a condition says
/// between the spec changing and the agent reporting, and it is why there is no
/// `PENDING` state anywhere. Nothing has to be written to mean "in flight".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

/// One reported fact about a resource, in the shape that makes it useful in an
/// interface: what it is about, whether it holds, a stable machine reason, and a
/// sentence for the person reading it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// `Ready`, `Scheduled`, `Attached`, `NetworkProgrammed`, …
    pub kind: String,
    pub status: ConditionStatus,
    /// A short, stable token an interface can branch on: `NoCapacity`,
    /// `ImageMissing`, `NodeUnreachable`.
    pub reason: String,
    /// One sentence for a person. Errors belong here, on the object, not only in
    /// a log file on whichever machine happened to run the controller.
    pub message: String,
    /// The generation this was observed at, so a stale condition is visibly
    /// stale rather than quietly wrong.
    pub observed_generation: u64,
    pub last_transition: Timestamp,
}

impl Condition {
    pub fn new(
        kind: &str,
        status: ConditionStatus,
        reason: &str,
        message: &str,
        at_generation: u64,
    ) -> Self {
        Self {
            kind: kind.to_string(),
            status,
            reason: reason.to_string(),
            message: message.to_string(),
            observed_generation: at_generation,
            last_transition: Timestamp::now(),
        }
    }

    pub fn ready(at_generation: u64) -> Self {
        Self::new("Ready", ConditionStatus::True, "Ready", "", at_generation)
    }
}

/// Set `c` on `conditions`, keeping `last_transition` when nothing about the
/// condition actually changed.
///
/// The timestamp has to mean "since when has it been like this" — an alert on
/// "drifting for more than five minutes" is worthless if every reconcile pass
/// refreshes it.
pub fn set_condition(conditions: &mut Vec<Condition>, c: Condition) {
    if let Some(existing) = conditions.iter_mut().find(|e| e.kind == c.kind) {
        let unchanged = existing.status == c.status
            && existing.reason == c.reason
            && existing.message == c.message;
        let transition = if unchanged {
            existing.last_transition
        } else {
            c.last_transition
        };
        *existing = Condition {
            last_transition: transition,
            ..c
        };
        return;
    }
    conditions.push(c);
}

pub fn condition<'a>(conditions: &'a [Condition], kind: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_collection_and_id_pairs() {
        let n = ResourceName::parse("projects/p1/zones/z1/instances/i1").unwrap();
        assert_eq!(n.collection(), "instances");
        assert_eq!(n.id(), "i1");
        assert_eq!(n.project(), Some("p1"));
        assert_eq!(n.ancestor("zones"), Some("z1"));
        assert_eq!(n.parent().unwrap().to_string(), "projects/p1/zones/z1");
    }

    #[test]
    fn a_name_that_would_be_mis_split_is_refused() {
        // An odd number of segments means somebody appended an id to a
        // collection, or a collection to an id — either way the parent is a lie.
        assert!(ResourceName::parse("projects/p1/instances").is_err());
        assert!(ResourceName::parse("projects//instances/i1").is_err());
        // Upper case and slashes inside an id are how a name stops round-tripping
        // through a URL, a store key and a log line unscathed.
        assert!(ResourceName::parse("projects/P1/instances/i1").is_err());
    }

    #[test]
    fn containment_is_what_iam_and_quota_both_ask() {
        let project = ResourceName::parse("projects/p1").unwrap();
        let instance = ResourceName::parse("projects/p1/instances/i1").unwrap();
        let other = ResourceName::parse("projects/p2/instances/i1").unwrap();
        assert!(instance.is_under(&project));
        assert!(!other.is_under(&project));
        // A prefix that merely shares a *string* prefix is not containment:
        // `projects/p1` must not swallow `projects/p10`.
        let similar = ResourceName::parse("projects/p10/instances/i1").unwrap();
        assert!(!similar.is_under(&project));
    }

    #[test]
    fn a_condition_that_did_not_change_keeps_the_time_it_started() {
        let mut cs = Vec::new();
        set_condition(
            &mut cs,
            Condition::new("Ready", ConditionStatus::False, "NoCapacity", "no host", 1),
        );
        let first = cs[0].last_transition;
        std::thread::sleep(std::time::Duration::from_millis(5));
        set_condition(
            &mut cs,
            Condition::new("Ready", ConditionStatus::False, "NoCapacity", "no host", 1),
        );
        assert_eq!(cs.len(), 1);
        // Otherwise "drifting for more than five minutes" can never fire: every
        // pass would reset the clock it is measured against.
        assert_eq!(
            cs[0].last_transition, first,
            "an unchanged condition was re-stamped"
        );

        std::thread::sleep(std::time::Duration::from_millis(5));
        set_condition(
            &mut cs,
            Condition::new("Ready", ConditionStatus::True, "Ready", "", 2),
        );
        assert!(
            cs[0].last_transition > first,
            "a real change kept a stale timestamp"
        );
    }

    #[test]
    fn a_finalizer_is_added_once() {
        let mut m = Meta::new(
            ResourceName::parse("projects/p1/volumes/v1").unwrap(),
            Placement::new("eu-central", "cell-1"),
        );
        m.add_finalizer("node-agent");
        m.add_finalizer("node-agent");
        assert_eq!(m.finalizers.len(), 1);
        m.remove_finalizer("node-agent");
        assert!(m.finalizers.is_empty());
    }
}

/// One term of a label selector: a key, and optionally the value it must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelTerm {
    pub key: String,
    /// `None` means "has this label at all, whatever it says". Both forms are
    /// worth having: `env=prod` is the common one, and `deprecated` — present
    /// with any value, or none — is how somebody marks a thing without
    /// deciding on a vocabulary first.
    pub value: Option<String>,
}

/// Read a selector as a person types it: `env=prod,tier=web`.
///
/// Commas separate terms and every term must match — an "or" would need
/// precedence rules, and a filter box whose meaning depends on precedence is
/// one people get wrong silently. Somebody who needs "or" runs two searches
/// and can see both answers.
///
/// Whitespace around a term is somebody's typing, not part of the key. An
/// empty term is skipped rather than refused, so a trailing comma does not
/// turn a working filter into an error message.
pub fn parse_selector(text: &str) -> Vec<LabelTerm> {
    text.split(',')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| match term.split_once('=') {
            Some((key, value)) => LabelTerm {
                key: key.trim().to_string(),
                value: Some(value.trim().to_string()),
            },
            None => LabelTerm {
                key: term.to_string(),
                value: None,
            },
        })
        .collect()
}

/// Whether these labels satisfy every term.
///
/// An empty selector matches everything, which is what "no filter" has to
/// mean — the alternative is a filter box that empties the list when cleared.
pub fn labels_match(labels: &BTreeMap<String, String>, terms: &[LabelTerm]) -> bool {
    terms.iter().all(|term| match &term.value {
        Some(want) => labels.get(&term.key).is_some_and(|have| have == want),
        None => labels.contains_key(&term.key),
    })
}

#[cfg(test)]
mod selector_tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn every_term_must_match_rather_than_any() {
        let it = labels(&[("env", "prod"), ("tier", "web")]);
        assert!(labels_match(&it, &parse_selector("env=prod")));
        assert!(labels_match(&it, &parse_selector("env=prod,tier=web")));
        // An "or" would need precedence rules, and a filter box whose meaning
        // depends on precedence is one people get wrong silently.
        assert!(!labels_match(&it, &parse_selector("env=prod,tier=db")));
    }

    /// A bare key asks whether the label is there at all.
    #[test]
    fn a_bare_key_matches_whatever_the_value_says() {
        let marked = labels(&[("deprecated", "")]);
        assert!(labels_match(&marked, &parse_selector("deprecated")));
        assert!(!labels_match(&labels(&[("env", "prod")]), &parse_selector("deprecated")));
    }

    /// An empty selector matches everything.
    ///
    /// The alternative is a filter box that empties the list when it is
    /// cleared, which reads as "nothing here" rather than "no filter".
    #[test]
    fn no_selector_matches_everything() {
        assert!(labels_match(&labels(&[]), &parse_selector("")));
        assert!(labels_match(&labels(&[("env", "prod")]), &parse_selector("  ")));
        assert!(labels_match(&labels(&[("env", "prod")]), &parse_selector(",,")));
    }

    /// Typing is forgiven where forgiving it cannot change the meaning.
    #[test]
    fn spaces_and_a_trailing_comma_do_not_break_a_working_filter() {
        let it = labels(&[("env", "prod"), ("tier", "web")]);
        assert!(labels_match(&it, &parse_selector(" env = prod , tier=web ,")));
    }

    /// A value that itself contains `=` survives.
    #[test]
    fn only_the_first_equals_separates_a_term() {
        let it = labels(&[("note", "a=b")]);
        assert_eq!(
            parse_selector("note=a=b"),
            vec![LabelTerm {
                key: "note".into(),
                value: Some("a=b".into())
            }]
        );
        assert!(labels_match(&it, &parse_selector("note=a=b")));
    }

    /// An empty value is a value, and is not the same as asking for presence.
    #[test]
    fn an_empty_value_is_different_from_no_value() {
        assert!(labels_match(&labels(&[("env", "")]), &parse_selector("env=")));
        assert!(!labels_match(&labels(&[("env", "prod")]), &parse_selector("env=")));
        // Whereas the bare key matches either.
        assert!(labels_match(&labels(&[("env", "prod")]), &parse_selector("env")));
    }
}
