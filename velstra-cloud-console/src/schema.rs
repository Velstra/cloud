//! What the console knows about each collection, in one place.
//!
//! The page renders nothing per-type. Every list, every form, every detail is
//! produced by the same script reading this table, which is the console's half
//! of the promise the model makes: one shape, one rendering, no special cases
//! about what "in progress" means. A collection added to the platform is a
//! `Collection` added here, not a new screen.
//!
//! It lives in Rust rather than in the script for two reasons. It is checked at
//! build time — a field kind that points at a collection that does not exist is
//! a failing test, not a select that renders empty at three in the morning. And
//! it is the one part of the console that has to agree with
//! `docs/rest-contract.md`, so it is the one part worth holding still.

use serde::Serialize;

/// Where a collection is addressed from.
///
/// Everything chargeable hangs under a project; a hypervisor does not belong to
/// one. The contract writes the path as the resource name under `/api/v1/`, and
/// that name is what decides this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    /// `/api/v1/projects/{project}/{collection}`
    Project,
    /// `/api/v1/{collection}` — a node is infrastructure, not tenancy.
    Global,
}

/// How one value is asked for.
///
/// The kinds are deliberately few and they are all *chosen* except two. A value
/// that is constrained gets a control that can only produce a legal answer; a
/// text box is what is left when the value is genuinely free, and there are
/// exactly two of those shapes here for that reason.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Kind {
    /// Free text, with a `check` naming the inline validator that guards it.
    Text {
        placeholder: &'static str,
        check: Check,
    },
    /// Free text over several lines — cloud-init, a public key blob.
    Lines { placeholder: &'static str },
    /// A stepper carrying its unit. Never a text box: a size typed into one is
    /// a size that can be typed wrong.
    Number {
        unit: &'static str,
        min: u64,
        max: u64,
        step: u64,
        /// Render MiB and GiB with a second reading in the larger unit, so
        /// "8192" is never ambiguous about which one it is.
        scale: Scale,
    },
    /// A boolean is a switch. It is never a checkbox with a sentence beside it.
    Switch,
    /// A moment in time, in the operator's own timezone.
    ///
    /// Never a number box. What is stored is milliseconds since the epoch, and
    /// a person asked to type one of those either pastes the wrong number or
    /// works one out by hand — both of which end with a machine going out of
    /// service at the wrong hour. The control offers a calendar and a clock and
    /// does the arithmetic itself.
    Moment {
        /// Offered when the field is empty, as a nudge rather than a default:
        /// "in an hour" is the answer most maintenance windows want, and one
        /// somebody would otherwise compute.
        #[serde(rename = "defaultInMinutes")]
        default_in_minutes: u64,
    },
    /// One of a fixed set, shown as a segmented control when short.
    Choice { options: &'static [Choice] },
    /// One of the objects that exist, fetched live. `filter_by` narrows the
    /// options to those whose named spec field matches this form's value for
    /// the same field — a subnet picker showing only subnets of the chosen
    /// network, which is the mapping being right rather than a label
    /// apologising for it being wrong.
    Ref {
        collection: &'static str,
        #[serde(rename = "filterBy")]
        filter_by: Option<&'static str>,
        spelling: Spelling,
    },
    /// An ordered list of picked objects.
    RefList {
        collection: &'static str,
        /// A second collection whose objects are a *more precise* answer to the
        /// same question, offered beneath the first one's.
        ///
        /// One field, two spellings — the same shape the image picker has, one
        /// control over: `families/debian-13` and a concrete build are both
        /// answers to "which image", and the concrete one is for pinning. Here a
        /// network and one of its subnets are both answers to "where does this
        /// guest go", and the subnet is for the case where the network has more
        /// than one and no longer says which range the address comes from.
        ///
        /// `None` for every field whose question has one kind of answer.
        #[serde(skip_serializing_if = "Option::is_none")]
        also: Option<&'static str>,
        spelling: Spelling,
    },
    /// A list of free strings, each one checked as it is typed.
    TextList {
        placeholder: &'static str,
        check: Check,
    },
    /// A list of security-group rules. The one field here that is not a scalar,
    /// and it is not one because a rule is not one: direction, protocol, an
    /// optional port range and a remote that is either a prefix or another
    /// group. Spelling it as four separate list fields would let somebody build
    /// a rule out of parts that do not line up.
    ///
    /// `remote_collection` is the collection a group-shaped remote picks from,
    /// so the picker offers what exists rather than asking for a name to be
    /// typed correctly.
    RuleList {
        #[serde(rename = "remoteCollection")]
        remote_collection: &'static str,
    },
    /// What a role grants: a list of `{verb, collections}`.
    ///
    /// Modelled on [`Kind::RuleList`] and for the same reason — a grant is not a
    /// scalar. A verb means nothing without the collections it applies to, and
    /// two list fields side by side would let somebody assemble "write" from one
    /// row and "networks" from another, producing a permission neither of them
    /// meant. One control produces the pair or it produces nothing.
    ///
    /// There is deliberately no wildcard among the collections offered: a role
    /// that could mean *everything* would be a second spelling of `admin` with
    /// no way to tell them apart in a list of who may do what.
    GrantList,
    /// The disks handed to Ceph: a list of `{node, device}` pairs.
    ///
    /// Modelled on [`Kind::RuleList`] and for the same reason — an OSD is not a
    /// scalar. A device path means nothing without the machine it is plugged
    /// into, and two list fields side by side would let somebody assemble the
    /// third disk of one node out of the second row of the node list and the
    /// third row of the path list. One control produces the pair or it produces
    /// nothing.
    ///
    /// It is not a [`Kind::RefList`] either, because a disk is not an object the
    /// API serves. It exists only inside `status.devices` on the node that can
    /// see it, so this control reads the collection named here and offers what
    /// each node reports about its own hardware.
    ///
    /// **The refusals are the feature.** Handing a disk to Ceph erases it, so a
    /// device is offered only when it is provably empty and every other one is
    /// shown *with the reason, in words*. Greying a row out answers "why can I
    /// not select this disk" with silence, and silence is what sends somebody to
    /// a terminal to find out something the platform already knew.
    ///
    /// The sentences are carried here rather than written in the script so there
    /// is one copy of each, and
    /// `velstra-cloud-api/tests/console_covers_the_model.rs` pins every one of
    /// them against `ceph::may_consume`. The console does not link the model —
    /// that is deliberate, it speaks REST — so nothing else is in a position to
    /// stop the two wordings drifting apart, and a refusal that no longer
    /// matches what the API would do is worse than no refusal at all.
    DiskList {
        /// The collection whose `status.devices` are offered.
        collection: &'static str,
        /// One sentence per state a device can be in that is not free, keyed by
        /// the tag that state carries on the wire. `{field}` in the sentence is
        /// replaced by whatever the state carries under that key.
        refusals: &'static [Refusal],
        /// Below this a disk is refused on size alone, whatever is on it.
        #[serde(rename = "minGib")]
        min_gib: u64,
        /// That refusal, with `{sizeGib}` and `{minGib}` in it.
        #[serde(rename = "tooSmall")]
        too_small: &'static str,
        /// What a device in a state this console has never heard of gets.
        ///
        /// Refused, never offered, and `{kind}` says which state it was. A node
        /// running a newer agent can report something this table has no sentence
        /// for, and the safe direction when the answer is unknown is the
        /// conservative one: a disk whose state cannot be read is not provably
        /// empty, and offering it would erase whatever the newer agent was
        /// trying to warn about.
        unknown: &'static str,
        /// Said on the control itself, loudly. Not a hint: this is the one
        /// action in the platform that nothing undoes.
        warning: &'static str,
    },
    /// The ports a load balancer answers on: protocol, the port on the VIP,
    /// and the port the members answer on.
    ///
    /// Rendered together like a security-group rule, and for the same reason:
    /// the three only mean anything together, and three parallel list fields
    /// would let somebody assemble a listener out of rows that do not line up.
    /// The protocols on offer are TCP and UDP and nothing else, because the
    /// fabric's datapath balances no others — a wider choice would be a
    /// control that produces an error.
    ListenerList,
    /// The pools to create once the cluster is up: a name and its two
    /// replication numbers.
    ///
    /// A third bespoke list, and the alternative was genuinely considered: a
    /// [`Kind::TextList`] of pool names would fit the existing machinery exactly
    /// and let the model's serde defaults fill in the rest. It is rejected
    /// because those two numbers are the pool's whole risk profile — `size` is
    /// how many copies exist, `min_size` is how few may be written to — and a
    /// control that cannot show them is a control that hides the difference
    /// between a pool that survives a node reboot and one that stops taking
    /// writes during it. They also constrain each other, and a cross-check needs
    /// both halves in one row.
    ///
    /// The defaults are carried here so a blank row starts where the model
    /// starts. Duplicated numbers, pinned in
    /// `velstra-cloud-api/tests/console_covers_the_model.rs` against what
    /// `CephPoolSpec` deserialises from a bare name — a console that offered 2/1
    /// while the API meant 3/2 would be a form quietly proposing a weaker pool
    /// than the one it was copying.
    PoolList {
        #[serde(rename = "defaultSize")]
        default_size: u32,
        #[serde(rename = "defaultMinSize")]
        default_min_size: u32,
    },
}

/// One reason a disk is not offered, in the words the model would use.
///
/// `kind` is the tag the state carries on the wire — `Filesystem`, `Mounted`,
/// `Osd` — and `text` is the sentence, with `{field}` wherever a value that
/// state carries belongs. The substitution is what keeps the sentence specific:
/// "it holds an ext4 filesystem" is an answer, "it is in use" is a shrug.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Refusal {
    pub kind: &'static str,
    pub text: &'static str,
}

const fn refusal(kind: &'static str, text: &'static str) -> Refusal {
    Refusal { kind, text }
}

/// Every state a block device can be in that is not free, and what to say about
/// it.
///
/// Word for word what `velstra_cloud_model::ceph::may_consume` returns, and
/// held to it by a test in the API crate — the one place both halves are
/// linkable. Nothing here is paraphrase: an operator who reads this sentence and
/// then reads the API's refusal of the same disk has to see the same words, or
/// the console has taught them something the platform does not believe.
const DEVICE_REFUSALS: &[Refusal] = &[
    refusal(
        "Partitioned",
        "it has a partition table with {partitions} partition(s) on it. Something laid this disk \
         out deliberately; wipe it outside the platform if it really is spare.",
    ),
    refusal(
        "Filesystem",
        "it holds a {fstype} filesystem. Handing it to Ceph erases that, so it is not offered \
         until the filesystem is gone.",
    ),
    refusal(
        "Mounted",
        "it is mounted at {at} right now. Whatever is using it is using it.",
    ),
    refusal(
        "System",
        "it holds swap or the root filesystem — consuming it takes this node down.",
    ),
    refusal("Osd", "it is already OSD {id}."),
    refusal(
        "Volume",
        "it is a member of {of}. Take it out of that first, if that is really what you want.",
    ),
    // The agent already wrote the sentence for this one — it is the state it
    // reaches for when it cannot classify a device, and it carries its own
    // reason. Repeating it verbatim is the whole template.
    refusal("Unsuitable", "{why}"),
];

/// The smallest disk worth making an OSD of, and the sentence that refuses a
/// smaller one.
///
/// `velstra_cloud_model::ceph::MIN_OSD_GIB`, copied for the same reason the
/// sentences above are, and pinned by the same test.
const MIN_OSD_GIB: u64 = 20;
const TOO_SMALL: &str = "it is {sizeGib} GiB, and an OSD wants at least {minGib}. Below that the \
                         OSD's own bookkeeping is a meaningful fraction of the disk.";

/// How a reference is written on the wire.
///
/// The platform spells them two ways and both are deliberate. A node is a bare
/// id, because that is what the scheduler writes into `spec.node`
/// (`node.meta.name.id()`), what an agent calls itself, and what ownership is
/// decided by — the store compares `spec.node` to the agent's own name by string
/// equality, so a full name there assigns an object to a node that does not
/// answer to it and nothing ever starts, with no error anywhere. Everything else
/// is a full resource name, because something has to follow it and a bare id
/// under an unstated parent finds nothing.
///
/// The API refuses the wrong spelling at the door rather than normalising it, so
/// this is stated per field rather than guessed from the collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Spelling {
    /// `projects/p1/images/debian-13`
    Name,
    /// `node-a`
    Id,
}

/// The inline validators. Named rather than written as a regex here so the
/// script owns one implementation of each and the schema cannot invent a
/// seventh dialect of "is this an address".
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Check {
    /// Anything.
    None,
    /// A resource id: the `ResourceName` rules, so it survives a URL, a store
    /// key and a log line unscathed.
    Id,
    Cidr,
    Address,
    Mac,
    /// `sha256:…`
    Digest,
    Url,
    /// An AIP resource name, `projects/p1/…`.
    Name,
}

/// A second reading beside a number, where the unit is one people mis-key.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Scale {
    None,
    /// Also show GiB.
    Mib,
    /// Also show a human size.
    Bytes,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
}

const fn choice(value: &'static str, label: &'static str) -> Choice {
    Choice { value, label }
}

/// One settable thing on a spec.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    /// The JSON key on `spec`, as the contract spells it.
    pub key: &'static str,
    pub label: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
    /// Refused by the form when empty.
    pub required: bool,
    /// Behind "More settings". The common path is what an operator sees first;
    /// everything that has a defensible default is one level deeper.
    pub advanced: bool,
    /// What to say when there is nothing to choose from.
    ///
    /// A picker over an empty collection says "none exist yet", which is true
    /// and useless: the person reading it is usually meeting the platform for
    /// the first time, and what they need is the order to do things in. Empty
    /// where the absence speaks for itself.
    pub when_empty: &'static str,
    /// Why this exists, in a sentence, shown under the control. Empty when the
    /// label already says everything — a hint that repeats the label is noise.
    pub help: &'static str,
    /// Set by the platform, not by an operator: shown, never editable.
    pub derived: bool,
    /// Answered when the object is created and fixed from then on.
    ///
    /// Not [`derived`] — an operator does decide it, once — and not an ordinary
    /// field either, because the API refuses to change it afterwards. Without
    /// this the edit form offers a control whose only possible outcome is a
    /// refusal, or worse: `spec.pool` on a volume was offered for editing, the
    /// API accepted the change, and nothing moved a byte. The old pool's agent
    /// stopped matching its filter and let go; the new one saw a volume another
    /// pool still had claimed and would not touch it. The volume simply stopped
    /// converging, with nothing anywhere saying why.
    ///
    /// [`derived`]: Field::derived
    pub at_creation: bool,
}

/// A column in a list.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    /// A dotted path into the resource: `status.state`, `spec.sizeGib`.
    pub path: &'static str,
    pub label: &'static str,
    #[serde(flatten)]
    pub cell: Cell,
    /// Fixed, sized to the data it holds. A list of forty rows is a departure
    /// board or it is nothing.
    pub width: u32,
}

/// How a cell reads.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "cell", rename_all = "camelCase")]
pub enum Cell {
    /// Language: the UI face.
    Text,
    /// A machine value: tabular mono, so digits line up down the column.
    Mono,
    Number {
        unit: &'static str,
    },
    Bytes,
    /// A count of the list at this path.
    Count,
    /// True/False as words, neutral — being attached is not a virtue.
    Yes {
        yes: &'static str,
        no: &'static str,
    },
    /// A timestamp, read as an age.
    Ago,
}

/// A pair the console puts side by side: what was asked, and what is.
///
/// This is the whole product in one row. Neither half means anything alone —
/// `status.state = Stopped` is a fault or exactly right depending on what
/// `spec.desiredState` says — so they are never shown apart.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Agreement {
    pub label: &'static str,
    /// Path into `spec`.
    pub asked: &'static str,
    /// Path into `status`.
    pub is: &'static str,
    /// What a disagreement means, for the operator reading one. Not "values
    /// differ" — why they would, and what closes the gap.
    pub note: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    /// The path segment and the id in the script: `instances`.
    pub id: &'static str,
    pub title: &'static str,
    pub singular: &'static str,
    /// How often to read this collection again, in seconds; 0 to only listen.
    ///
    /// The watch is enough for anything whose status is *written*: a change
    /// means a write, a write means an event. It is not enough for a condition
    /// the API computes at read time out of the passing of time — a migration
    /// that has run past its timeout is decided by the clock, and there is no
    /// write for the store to announce. A screen that only listens shows that
    /// migration as transferring forever, so this one is asked again.
    pub recheck: u32,
    /// The condition the console reads a verdict out of.
    ///
    /// Almost everything answers `Ready`, and the one that does not is the one
    /// that would be a lie if it did: a migration is not "ready", it has moved
    /// or it has not, and the model writes that as `Moved`. Naming it here keeps
    /// the console's promise that there is exactly one function deciding whether
    /// something is settled — it reads a different condition, not a second
    /// vocabulary.
    ///
    /// **Empty means nothing reports on these objects at all**, and the verdict
    /// for them is "settled" rather than "not reported". That is not a
    /// convenience: an audit record, a usage reading, a user account are
    /// *records* — there is no agent that will ever write a condition on one,
    /// so `observedGeneration` stays at zero for ever, and a console that read
    /// that as "waiting" put every one of them on the screen an operator uses
    /// to find what is wrong.
    ///
    /// Measured on a real cell before this existed: a hundred and nine objects
    /// on the attention list, of which three were actually wrong. The three
    /// were unfindable.
    pub condition: &'static str,
    /// The rail groups these. The groups are an operator's vocabulary, not the
    /// source layout.
    pub group: &'static str,
    pub scope: Scope,
    /// One sentence at the top of the list saying what this collection is for.
    /// It is there because a console is also where somebody learns the system.
    pub blurb: &'static str,
    pub fields: &'static [Field],
    pub columns: &'static [Column],
    pub agreements: &'static [Agreement],
    pub creatable: bool,
    pub editable: bool,
    pub deletable: bool,
    /// Answers `:explainPlacement`.
    pub explainable: bool,
}

// ---- the collections -------------------------------------------------------

const USER_FIELDS: &[Field] = &[
    Field {
        key: "service",
        label: "Service account",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "A program rather than a person: it signs in with no password and \
               carries a token instead, minted by an operator and shown once. It \
               is named in a project's bindings like anybody else and gets the \
               same four roles.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "displayName",
        label: "Display name",
        kind: Kind::Text {
            placeholder: "Ada Lovelace",
            check: Check::None,
        },
        required: false,
        advanced: false,
        // Says what it is *not* for, because the obvious assumption is wrong and
        // the consequence of acting on it is a permission that does not apply.
        help: "Shown on screen. Never used to decide anything — the account's id \
               is its identity.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "email",
        label: "Email",
        kind: Kind::Text {
            placeholder: "ada@example.org",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "disabled",
        label: "Disabled",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        // The two things an operator needs to know before flipping it: it takes
        // effect now, and it is the reversible half of deleting.
        help: "Ends every session this account holds immediately, and refuses \
               new sign-ins. Its roles are kept, so turning it back on restores \
               exactly what it had.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "cellAdmin",
        label: "Cell administrator",
        kind: Kind::Switch,
        required: false,
        // Behind the disclosure on purpose. It is the most consequential switch
        // on the screen and the one least often wanted, and putting it beside
        // the display name invites the mis-click.
        advanced: true,
        help: "May do anything anywhere in this cell, including inside every \
               project. Grant it to operate the platform, not to use it.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const CEPH_FIELDS: &[Field] = &[
    Field {
        key: "publicNetwork",
        label: "Network",
        kind: Kind::Text {
            placeholder: "10.0.0.0/24",
            check: Check::Cidr,
        },
        required: true,
        advanced: false,
        // Says the consequence of getting it wrong, not what the field is. "The
        // public network" tells an operator nothing they could act on.
        help: "Where the daemons talk to each other and to clients. A node with \
               several interfaces has several answers, and the wrong one puts \
               replication traffic on the tenant network.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "clusterNetwork",
        label: "Replication network",
        kind: Kind::Text {
            placeholder: "10.1.0.0/24",
            check: Check::Cidr,
        },
        required: false,
        advanced: true,
        help: "A separate network for replication. Empty means the one above \
               carries both, which is the ordinary answer for a small cluster.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "monitors",
        label: "Monitors",
        kind: Kind::RefList {
            collection: "nodes",
            also: None,
            // A bare id, like every other reference to a node in this platform.
            // The convention exists because a node is a cell-scoped root object
            // — `hv-1`, not `nodes/hv-1` — and one spelling everywhere is what
            // keeps a reference from having to be re-expanded somewhere.
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        // The one number worth being loud about is two, and the reason is not
        // obvious — it looks redundant and is not.
        help: "Three nodes, or one for a lab. Never two: a quorum of two \
               survives no failures and looks like it would.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "osds",
        label: "Disks",
        kind: Kind::DiskList {
            collection: "nodes",
            refusals: DEVICE_REFUSALS,
            min_gib: MIN_OSD_GIB,
            too_small: TOO_SMALL,
            unknown: "this node reports it as {kind}, which this console has no answer for. Not \
                      offered: a disk whose state cannot be read is not a disk anything here can \
                      call empty.",
            // The one action in this platform that nothing undoes, said on the
            // control where the click is, in the colour of a thing that cannot
            // be taken back. The collection's blurb says it too, at the top of
            // the dialog — but by the time somebody is reading a list of disks
            // they are past the blurb, and this is the sentence that has to be
            // in the way.
            warning: "Adding a disk here erases it. Everything on that device goes, and nothing \
                      in this platform brings it back.",
        },
        required: false,
        advanced: false,
        // Says what the refusals are for, because the first thing an operator
        // does on this screen is look for a disk that is not in the list.
        help: "One disk, one OSD. Anything that cannot be chosen says why beside \
               it rather than being greyed out — only a device the node can see \
               is empty is offered, so a disk you really do want to give up has \
               to be wiped outside the platform first.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "pools",
        label: "Pools",
        kind: Kind::PoolList {
            default_size: 3,
            default_min_size: 2,
        },
        required: false,
        // One level deeper because three copies and a floor of two is the answer
        // almost everybody wants and the model already fills in. An operator who
        // needs another comes looking; nobody should have to answer it to get a
        // cluster.
        advanced: true,
        // The consequence of each number, not what each number is called. "Size
        // is the replica count" is a definition; "min size is where writes stop"
        // is the thing that happens at three in the morning.
        help: "Where volumes are stored. Copies is how many of every object \
               exist; below the floor the pool refuses writes rather than \
               holding data it cannot protect — so a floor equal to the copies \
               means one node rebooting stops writing.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "paused",
        label: "Paused",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "Stops the deployment where it stands. Nothing is torn down, and \
               turning it off carries on from there.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const ROLE_FIELDS: &[Field] = &[
    Field {
        key: "displayName",
        label: "Name",
        kind: Kind::Text {
            placeholder: "Database operator",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "description",
        label: "What it is for",
        kind: Kind::Text {
            placeholder: "Restart the database machines, nothing else",
            check: Check::None,
        },
        required: false,
        advanced: false,
        // Not decoration. A list of role names tells nobody what they mean, and
        // the person reading it is usually deciding whether to grant one.
        help: "Somebody granting this will read this line and nothing else.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "grants",
        label: "Grants",
        kind: Kind::GrantList,
        required: true,
        advanced: false,
        help: "A verb, and the collections it applies to. Being able to act on \
               something carries being able to see it — anything else is a \
               button that works above a screen that shows nothing.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const FOLDER_FIELDS: &[Field] = &[
    Field {
        key: "displayName",
        label: "Name",
        kind: Kind::Text {
            placeholder: "Engineering",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "parent",
        label: "Inside",
        kind: Kind::Ref {
            collection: "folders",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: false,
        // No control for the bindings here, and that is deliberate: they are a
        // *set*, and a form field holding the whole set replaces all of it on
        // every save — somebody adding one person while a colleague adds another
        // loses the colleague's change and never learns it happened. The Access
        // panel on the sheet edits them a row at a time, against the revision it
        // was drawn from.
        help: "Left empty this folder sits at the top. Roles granted above it \
               reach in here too.",
        when_empty: "There are no folders yet. The first one goes at the top.",
        derived: false,
        at_creation: false,
    },
];

const PROJECT_FIELDS: &[Field] = &[
    // ---- what the cell allows this project ---------------------------------
    //
    // A quota says how much; these say what kind. They are the cell operator's
    // to set and the tenant's to work within — a project admin sees them and
    // cannot change them, which is the point: the provider decides what a
    // customer may reach for.
    Field {
        key: "policy.hostBridges",
        label: "Host bridges this project may use",
        kind: Kind::Lines {
            placeholder: "br0\nvmbr1",
        },
        required: false,
        advanced: true,
        help: "One bridge per line, as they are named on the nodes. A network \
               put on one takes its guests off this platform's networks and \
               onto whatever the machine is on — no address from us, no \
               gateway, no security group. Empty means this project gets \
               logical networks only, which is what a new customer should \
               have. Named rather than a yes/no, because what anybody means by \
               a host bridge is a particular wire.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "policy.devicePassthrough",
        label: "May pass hardware through",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "Whether guests here may be given a GPU or a NIC of their own. A \
               passed-through device is a physical thing one guest holds and no \
               other guest can have, so a project that may ask for them can \
               empty a node of the hardware everybody else was scheduled \
               against.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "policy.floatingIps",
        label: "May hold public addresses",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "Whether this project may claim addresses the world can reach. \
               They come out of address space the cell was given by whoever is \
               above it, so a project that could mint them could exhaust the \
               pool every other project is waiting on.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "policy.customSizes",
        label: "May size guests by hand",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "Whether guests here may be sized by typed numbers instead of a \
               flavor. Only asked once the cell has flavors at all: with a menu \
               defined, the closed answer is the sold one, and this is the \
               deliberate exception for the customer whose shapes are their own.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "displayName",
        label: "Display name",
        kind: Kind::Text {
            placeholder: "Platform team",
            check: Check::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "parent",
        label: "Folder",
        kind: Kind::Ref {
            collection: "folders",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: true,
        // The help used to say "policies and quota are inherited from here",
        // which was never true of anything: nothing walked this field at all. It
        // is roles that inherit, and only roles.
        help: "Roles granted on the folder reach every project under it. Quota \
               and policy are this project's own.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "cell",
        label: "Cell",
        kind: Kind::Text {
            placeholder: "cell-1",
            check: Check::Id,
        },
        required: false,
        // Behind the disclosure, because a project is named far more often than
        // it is placed: an installation with one cell never sets this, and one
        // with several sets it once, when the project is created.
        advanced: true,
        // Says what it decides rather than what it is. "The home cell" tells an
        // operator nothing they could act on; "where this project's resources
        // live" tells them the consequence of getting it wrong.
        help: "Which cell this project's resources live in, and where requests \
               for them are routed. Leave empty to use whichever cell answers — \
               which is what an installation with one cell wants.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.instances",
        label: "Instances",
        kind: Kind::Number {
            unit: "",
            min: 0,
            max: 100_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        // The four together are one decision — how large this project may
        // get — and it is a decision an operator makes about a project that
        // exists, not while naming one. Creating a project is naming it.
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.vcpus",
        label: "vCPUs",
        kind: Kind::Number {
            unit: "",
            min: 0,
            max: 1_000_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.memoryMib",
        label: "Memory",
        kind: Kind::Number {
            unit: "MiB",
            min: 0,
            max: 100_000_000,
            step: 1024,
            scale: Scale::Mib,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.volumeGib",
        label: "Volume space",
        kind: Kind::Number {
            unit: "GiB",
            min: 0,
            max: 10_000_000,
            step: 10,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    // The four the API has always enforced and this screen could not set. A
    // dimension a cell operator cannot cap is a dimension that is capped at
    // whatever somebody typed into the API by hand, or not at all.
    Field {
        key: "quota.volumes",
        label: "Volumes",
        kind: Kind::Number {
            unit: "volumes",
            min: 0,
            max: 100_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "A count, separate from the space: per-volume overhead is a \
               different worry from capacity, and the two are capped apart.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.floatingIps",
        label: "Floating IPs",
        kind: Kind::Number {
            unit: "addresses",
            min: 0,
            max: 100_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "An address that outlives the machine answering on it is scarce \
               and externally routable.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.loadBalancers",
        label: "Load balancers",
        kind: Kind::Number {
            unit: "balancers",
            min: 0,
            max: 100_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Each one takes an address out of a subnet and datapath entries \
               on every ingress host.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "quota.devices",
        label: "Passed-through devices",
        kind: Kind::Number {
            unit: "devices",
            min: 0,
            max: 10_000,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Each one is a piece of hardware that exists once and cannot be \
               oversubscribed, so without a cap one project can take every \
               accelerator in the cell.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const FAMILY_FIELDS: &[Field] = &[
    Field {
        key: "family",
        label: "Family",
        kind: Kind::Text {
            placeholder: "",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "version",
        label: "Newest version",
        kind: Kind::Text {
            placeholder: "",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "image",
        label: "Resolves to",
        kind: Kind::Ref {
            collection: "images",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        advanced: false,
        help: "The bytes a machine made right now would get.",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "public",
        label: "Everybody may boot it",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        // Placement is what has always decided this; it was simply never said
        // out loud, so an operator publishing a template had no way to check
        // which of the two they had made.
        help: "Published to the cell rather than kept in one project.",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
];

const FLAVOR_FIELDS: &[Field] = &[
    Field {
        key: "vcpus",
        label: "vCPUs",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 256,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "memoryMib",
        label: "Memory",
        kind: Kind::Number {
            unit: "MiB",
            min: 256,
            max: 4_194_304,
            step: 256,
            scale: Scale::Mib,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "rootDiskGib",
        label: "Root disk",
        kind: Kind::Number {
            unit: "GiB",
            min: 1,
            max: 65_536,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "description",
        label: "Description",
        kind: Kind::Text {
            placeholder: "burstable, for dev boxes",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "One sentence for the picker. The numbers already say most of it.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const BGP_PEER_FIELDS: &[Field] = &[
    Field {
        key: "peer",
        label: "Peer address",
        kind: Kind::Text {
            placeholder: "10.10.10.1",
            check: Check::None,
        },
        required: true,
        advanced: false,
        help: "The router or firewall in front of the cell.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "peerAs",
        label: "Peer AS",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 4_294_967_295,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "localAs",
        label: "Local AS",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 4_294_967_295,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "node",
        label: "Speaks from",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        help: "The machine that holds the session — a gateway, with FRR \
               installed. One session is one speaker.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "password",
        label: "TCP-MD5 password",
        kind: Kind::Text {
            placeholder: "",
            check: Check::None,
        },
        required: false,
        advanced: true,
        help: "The same string the router has for this session (RFC 2385). Most \
               firewalls will not bring a session up without one.",
        when_empty: "none — the session is unauthenticated",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "multihop",
        label: "Hops to the peer",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 255,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Only when the router is not on the same wire: eBGP refuses a \
               neighbour more than one hop away unless told the distance.",
        when_empty: "directly connected",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "description",
        label: "Description",
        kind: Kind::Text {
            placeholder: "edge firewall, rack 3",
            check: Check::None,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const INSTANCE_FIELDS: &[Field] = &[
    Field {
        key: "flavor",
        label: "Flavor",
        kind: Kind::Ref {
            collection: "flavors",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: false,
        help: "A named size from the cell's menu. Picking one sets the vCPUs,                memory and root disk below; typing sizes instead needs the                project's leave.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "image",
        // The catalogue, not the bytes. Picking `debian-13` gets the newest of
        // that family at the moment the machine is made and pins the guest to it
        // for life; picking one image pins it to whichever build somebody
        // happened to be looking at. The field still takes either — it should
        // just not be a digest that greets somebody choosing an OS.
        label: "Image",
        kind: Kind::Ref {
            collection: "families",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "The newest of the family, resolved once when the machine is made. \
               An existing machine keeps the bytes it was built from.",
        when_empty: "",
        derived: false,
        // Decided when the machine is made: the API refuses a change, so the
        // form does not offer one. Until it was locked, an edit showed the
        // family picker with nothing chosen and would not save without a pick.
        at_creation: true,
    },
    Field {
        key: "vcpus",
        label: "vCPUs",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 256,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "memoryMib",
        label: "Memory",
        kind: Kind::Number {
            unit: "MiB",
            // A grid that contains the sizes people ask for. `min: 256` with a step of
            // 512 put 2048, 4096 and 8192 all *between* two valid values, so the
            // form opened with its own default marked invalid and every round
            // number a person typed was refused by the control.
            min: 256,
            max: 4_194_304,
            step: 256,
            scale: Scale::Mib,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "rootDiskGib",
        label: "Root disk",
        kind: Kind::Number {
            unit: "GiB",
            min: 1,
            max: 65_536,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        // One disclosure deeper since the flavor arrived: a size picked by
        // name sets this anyway, and the control keeps its default for the
        // hand-sized path. The form still submits it either way.
        advanced: true,
        help: "Set by the flavor when one is picked.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "desiredState",
        label: "Power",
        kind: Kind::Choice {
            options: &[choice("Running", "Running"), choice("Stopped", "Stopped")],
        },
        required: false,
        // A guest somebody asked to exist runs. Saying so on the way in is
        // a choice almost nobody makes, and it is one switch away.
        advanced: true,
        help: "What it should be doing. Asking twice is the same as asking once.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "ports",
        label: "Ports",
        kind: Kind::RefList {
            collection: "ports",
            also: None,
            spelling: Spelling::Name,
        },
        required: false,
        // In the common path, not behind "More settings".
        //
        // The old reasoning was that a guest with no NIC is unusual and not
        // what most people are deciding while they name a machine. Watching
        // somebody meet this form for the first time settled it the other way:
        // the field being hidden is exactly *why* they end up with a machine
        // that has no NIC, and a guest with no network cannot be reached, cannot
        // be configured, and cannot reach its own metadata service. It is not a
        // detail; it is the difference between a usable guest and a puzzle.
        // One level deeper, and the form says so instead.
        //
        // Putting it in the common path was tried and traded away: the path is
        // capped at four on purpose, and displacing the root disk to make room
        // moved the invisible problem rather than removing it. What somebody
        // meeting this form needs is not the control in front of them — it is
        // to be told, before they press Create, that the guest they are about
        // to make will have no network at all. `consequences` in form.js says
        // it, beside the button.
        advanced: true,
        help: "For a NIC that already exists — one holding an address you want \
               this guest to keep. To simply put a machine on a network, name the \
               network above and the port is made for you.",
        when_empty: "This project has no ports yet, and does not need any: name a \
                     network above and one is made. A port of your own is for the \
                     case where the address has to outlive the machine.",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "volumes",
        label: "Extra disks",
        kind: Kind::RefList {
            collection: "volumes",
            also: None,
            spelling: Spelling::Name,
        },
        required: false,
        // Beside the root disk, which is a size rather than an object: a guest
        // always has one and nobody picks it from a list.
        //
        // The same move as `networks`, one layer over. An attachment is a join —
        // this guest, that volume, opened by that node — and making one by hand
        // meant creating the volume, waiting for the guest to be placed, reading
        // which node that was, and only then attaching. Three steps, one of them
        // a wait, to say "this machine has this disk".
        //
        // Kept rather than consumed, unlike `networks`: taking a disk off this
        // list is how you detach, and a field that emptied itself could not say
        // that.
        advanced: true,
        help: "Attached once the guest is on a node. Take one off the list to \
               detach it.",
        when_empty: "This project has no volumes yet. Make one under Volumes and \
                     it can be attached here — the attachment itself is made for \
                     you.",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "networks",
        label: "Networks",
        kind: Kind::RefList {
            collection: "networks",
            also: Some("subnets"),
            spelling: Spelling::Name,
        },
        required: false,
        // Advanced, and that is the change the default network paid for.
        //
        // A port is a join — this guest, that network, this address — right in
        // the model and wrong in a form. The old form asked for one and the
        // empty-state taught the ritual: a Network, then a Subnet on it, then a
        // Port on that subnet, in that order, before the machine. Three objects
        // and a dependency order, to answer "put it on my network".
        //
        // The port field was in the common path because hiding it was *why*
        // people ended up with a guest that had no NIC. That reason is gone:
        // empty now means the project's default network, so the honest thing is
        // to say which network they are getting rather than ask. The line beside
        // the Create button says it.
        advanced: true,
        help: "Left empty, this guest joins your project's default network — made \
               the first time somebody needs it, so two machines in a project can \
               talk without anybody configuring anything.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "sshKeys",
        label: "SSH keys",
        kind: Kind::TextList {
            placeholder: "ssh-ed25519 AAAA…",
            check: Check::None,
        },
        required: false,
        // Almost always cloud-init's business rather than a field typed here.
        advanced: true,
        help: "Read by cloud-init on the guest's **first** boot and never again, \
               so adding one to a machine that has already started does nothing \
               to that machine. Without a key and without a password set in \
               user-data, the console is the only way in.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "userData",
        label: "User data",
        kind: Kind::Lines {
            placeholder: "#cloud-config",
        },
        required: false,
        advanced: true,
        help: "Handed to the guest on first boot.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "node",
        label: "Pinned to node",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: true,
        help: "Left empty the scheduler chooses. Moving it afterwards is a \
               migration, not an edit.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.antiAffinityGroup",
        label: "Anti-affinity group",
        kind: Kind::Text {
            placeholder: "web-tier",
            check: Check::Id,
        },
        required: false,
        advanced: true,
        help: "Instances in one group are kept off the same node — what keeps \
               a service alive when a machine dies.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.spread",
        label: "Keeping them apart is",
        kind: Kind::Choice {
            options: &[
                Choice {
                    value: "Required",
                    label: "a rule",
                },
                Choice {
                    value: "Preferred",
                    label: "a wish",
                },
            ],
        },
        required: false,
        advanced: true,
        help: "A rule refuses a node that already runs a member — three \
               replicas of a database must not share a machine even if that \
               means one stays down. A wish takes a crowded node over not \
               running at all, which is what twelve web servers want.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.affinityGroup",
        label: "Affinity group",
        kind: Kind::Text {
            placeholder: "checkout",
            check: Check::Id,
        },
        required: false,
        advanced: true,
        help: "The opposite ask: instances in one group are placed together. \
               For a pair that talks constantly — an application and the cache \
               it reads on every request — where a hop between machines is the \
               whole latency budget.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.affinity",
        label: "Keeping them together is",
        kind: Kind::Choice {
            options: &[
                Choice {
                    value: "Required",
                    label: "a rule",
                },
                Choice {
                    value: "Preferred",
                    label: "a wish",
                },
            ],
        },
        required: false,
        advanced: true,
        help: "A rule refuses every node but the one the group is already on, \
               and says which that is. A wish places elsewhere rather than not \
               at all.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.requiredLabels",
        label: "Required node labels",
        kind: Kind::TextList {
            placeholder: "gpu",
            check: Check::Id,
        },
        required: false,
        advanced: true,
        help: "Only nodes carrying all of these.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "startOrder",
        label: "Start order",
        kind: Kind::Number {
            unit: "",
            min: 0,
            max: 999,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Lower starts first; the same number is a group that starts \
               together. Matters after a power cut, when a node brings back \
               everything it holds at once and the database loses the race for \
               disk to a dozen web servers.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "startDelayS",
        label: "Let the group ahead settle",
        kind: Kind::Number {
            unit: "s",
            min: 0,
            max: 3600,
            step: 5,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Measured from the newest start in that group, not each member \
               in turn — a hundred guests at thirty seconds each would be \
               fifty minutes of nothing happening.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "onNodeLoss",
        label: "If its node stops answering",
        kind: Kind::Choice {
            options: &[
                choice("leave", "Leave it where it is"),
                choice("restart", "Start it on another node"),
            ],
        },
        required: false,
        advanced: true,
        help: "Only for a guest whose storage every node can reach. One on \
               local storage that is started elsewhere is an empty machine \
               wearing a familiar name. Nothing is moved until the node has \
               been quiet long enough that its own agent has certainly stopped \
               its guests — a node with no fencing deadline is never recovered \
               from.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "console",
        label: "Show console output",
        kind: Kind::Switch,
        required: false,
        // Advanced, and not by concession to a rule. Nobody decides this while
        // *creating* a guest — it is reached for when one is misbehaving, which
        // is a different visit to the same object.
        advanced: true,
        help: "Publishes what the guest writes to its serial console, so it \
               can be read here. Off by default because it costs a report \
               every time the guest logs a line. A guest that is not running \
               shows its last output whether this is on or not.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "devices",
        label: "Passed-through devices",
        kind: Kind::RefList {
            collection: "device-classes",
            also: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: true,
        help: "A class, not a machine's address. Two of the same class means \
               two devices. A guest holding one cannot be live-migrated — a \
               device's state is in hardware and cannot be transferred.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "placementPolicy.minCpuLevel",
        label: "Minimum CPU level",
        kind: Kind::Choice {
            options: &[
                choice("", "Any"),
                choice("x86-64-v2", "x86-64-v2"),
                choice("x86-64-v3", "x86-64-v3"),
                choice("x86-64-v4", "x86-64-v4"),
            ],
        },
        required: false,
        advanced: true,
        help: "What the image needs to run. RHEL 9 and CentOS Stream 9 need \
               x86-64-v2 or they will not boot.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const VOLUME_FIELDS: &[Field] = &[
    Field {
        key: "sizeGib",
        label: "Size",
        kind: Kind::Number {
            unit: "GiB",
            min: 1,
            max: 262_144,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "pool",
        label: "Pool",
        kind: Kind::Text {
            placeholder: "chosen for you",
            check: Check::Id,
        },
        // Which pool holds the bytes is the platform's business: left empty,
        // the cell picks the accepting pool with the most room and writes it
        // down. It was a required field, which meant a tenant had to name a
        // pool they are not allowed to list — a form no customer could fill in.
        required: false,
        advanced: true,
        help: "Left empty, the cell chooses: the accepting pool with the most \
               room. Naming one is for operators pinning a volume to specific \
               hardware — a tenant cannot list pools and does not need to.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "sourceImage",
        label: "From image",
        kind: Kind::Ref {
            collection: "images",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        advanced: false,
        help: "Left empty the volume comes up blank.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "sourceSnapshot",
        label: "From snapshot",
        // A name typed in, not a picker, and that is a limitation rather than a
        // choice: a snapshot's name hangs under a *volume*
        // (`projects/p1/volumes/data-1/snapshots/nightly`), and `Scope` knows
        // only Project and Global. Until the schema can express a collection
        // nested under an object, there is no snapshots screen for a picker to
        // read — and offering a dropdown backed by nothing would be worse than
        // asking for the name.
        kind: Kind::Text {
            placeholder: "projects/p1/volumes/data-1/snapshots/nightly",
            check: Check::Name,
        },
        required: false,
        // Beside "From image" rather than behind the disclosure: restoring a
        // snapshot is not an advanced variation on making a volume, it is the
        // other ordinary reason to make one. A volume is never restored in
        // place — restoring *is* making a new volume from a snapshot — so this
        // is where a person comes looking for it.
        advanced: false,
        help: "Restores that snapshot into a new volume. A volume is never \
               restored in place, so this is what restoring means.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "encryptionKey",
        label: "Encryption key",
        kind: Kind::Text {
            placeholder: "projects/p1/keys/data",
            check: Check::Name,
        },
        required: false,
        advanced: true,
        help: "Empty means the bytes are stored in the clear, which is a \
               decision rather than a default.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
];

const ATTACHMENT_FIELDS: &[Field] = &[
    Field {
        key: "volume",
        label: "Volume",
        kind: Kind::Ref {
            collection: "volumes",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "instance",
        label: "Instance",
        kind: Kind::Ref {
            collection: "instances",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "node",
        label: "Node",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: false,
        // Derived, not asked for: the contract says the API copies it from the
        // instance, and an attachment whose node is not the instance's is a
        // meaningless object — the node it names does not have the guest, and
        // the node that does is not watching for it. Shown all the same,
        // because which node will open the volume is worth knowing before
        // asking for it.
        help: "Where the volume will be opened. Taken from the instance.",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "readOnly",
        label: "Read only",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const NETWORK_FIELDS: &[Field] = &[
    Field {
        key: "hostBridge",
        label: "On the machine's own wire",
        kind: Kind::Text {
            placeholder: "br0",
            // An interface name, not a resource name: it names something on the
            // machine, and the machine's rules are the kernel's.
            check: Check::None,
        },
        required: false,
        advanced: true,
        help: "A bridge that already exists on the nodes — `br0`, `vmbr0`. \
               Guests on this network go straight onto whatever the machine is \
               on: the house LAN, a VLAN, a lab network. Their addresses come \
               from whatever serves that wire and this platform allocates \
               none, holds no gateway and enforces no security group. Empty is \
               the ordinary case. Only a cell operator may set it — it is a \
               decision about the machine, not about a project.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "external",
        label: "Carries real addresses",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "The prefixes on this network's subnets are routed to this cell \
               by whoever is above it, so an address from one is an address \
               the world can reach. Only a cell operator may say so — a tenant \
               who could would mint themselves a public range by typing a CIDR.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "announce",
        label: "Addresses announced by",
        kind: Kind::Choice {
            options: &[
                Choice {
                    value: "FromGateway",
                    label: "a gateway node",
                },
                Choice {
                    value: "FromHost",
                    label: "the machine holding the guest",
                },
            ],
        },
        required: false,
        advanced: true,
        help: "What this cell does by default for addresses from this network. \
               An individual address may say otherwise. Meaningless on a \
               network that carries no real addresses.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "mtu",
        label: "MTU",
        kind: Kind::Number {
            unit: "bytes",
            min: 1280,
            max: 9216,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "1450 when left empty: a VXLAN header is 50 bytes, and a tenant \\
               network handed the wire's own 1500 black-holes every large packet \\
               in a way that looks like an application bug for a week.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "vni",
        label: "VNI",
        kind: Kind::Number {
            unit: "",
            min: 0,
            max: 16_777_215,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "The VXLAN network identifier. Left empty, the cell assigns the \\
               smallest free one — which is the only correct answer, and not one \\
               a tenant can work out.",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
];

const SUBNET_FIELDS: &[Field] = &[
    Field {
        key: "network",
        label: "Network",
        kind: Kind::Ref {
            collection: "networks",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "No networks in this project yet. A network is the first of \
                     the three things a guest needs to be reachable: a network, \
                     a subnet on it, then a port.",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "cidr",
        label: "Range",
        kind: Kind::Text {
            placeholder: "10.0.0.0/24",
            check: Check::Cidr,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "gateway",
        label: "Gateway",
        kind: Kind::Text {
            placeholder: "10.0.0.1",
            check: Check::Address,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "dns",
        label: "Resolvers",
        kind: Kind::TextList {
            placeholder: "10.0.0.2",
            check: Check::Address,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "reserved",
        label: "Reserved",
        kind: Kind::TextList {
            placeholder: "10.0.0.5",
            check: Check::Address,
        },
        required: false,
        advanced: true,
        help: "Addresses the platform will not hand out.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const SECURITY_GROUP_FIELDS: &[Field] = &[Field {
    key: "rules",
    label: "Rules",
    kind: Kind::RuleList {
        remote_collection: "security-groups",
    },
    required: false,
    advanced: false,
    help: "Every rule permits; none of them forbids. A group with no rules \
               is a group that allows nothing extra, which is the platform's \
               default anyway: nothing in, everything out, replies always.",
    when_empty: "",
    derived: false,
    at_creation: false,
}];

const PORT_FIELDS: &[Field] = &[
    Field {
        key: "network",
        label: "Network",
        kind: Kind::Ref {
            collection: "networks",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "subnet",
        label: "Subnet",
        kind: Kind::Ref {
            collection: "subnets",
            filter_by: Some("network"),
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "No subnets on this network yet. A subnet is the range its \
                     addresses come from — make one before a port.",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "securityGroups",
        label: "Security groups",
        kind: Kind::RefList {
            collection: "security-groups",
            also: None,
            spelling: Spelling::Name,
        },
        required: false,
        advanced: false,
        help: "Rules only ever add allowances, so ordering does not matter and \
               two groups cannot contradict each other. With none, the port \
               keeps the platform's default: nothing in, everything out, \
               replies always.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "address",
        label: "Address",
        kind: Kind::Text {
            placeholder: "allocated by IPAM",
            check: Check::Address,
        },
        required: false,
        advanced: true,
        help: "Left empty IPAM allocates one. It never changes afterwards — an \
               address that moves under a running guest is an outage.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "mac",
        label: "MAC",
        kind: Kind::Text {
            placeholder: "generated",
            check: Check::Mac,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "rateLimitMbit",
        label: "Rate limit",
        kind: Kind::Number {
            unit: "Mbit/s",
            min: 0,
            max: 400_000,
            step: 100,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Zero is unlimited. Multi-tenancy without a ceiling is one noisy \
               neighbour away from an incident.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const IMAGE_SOURCE_FIELDS: &[Field] = &[
    Field {
        key: "family",
        label: "Family",
        kind: Kind::Text {
            placeholder: "debian-13",
            check: Check::Id,
        },
        required: true,
        advanced: false,
        help: "Everything this source publishes joins this family, and an \
               instance asking for `families/debian-13` gets the newest of them.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "url",
        label: "Image",
        kind: Kind::Text {
            placeholder: "http://cloud.example/debian-13-genericcloud-amd64.qcow2",
            check: Check::Url,
        },
        required: true,
        advanced: false,
        help: "Where the bytes are. This may be plain http: the digest below is \
               what makes fetching them safe, and a wrong byte gives a wrong \
               digest and fails the fetch.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "checksums",
        label: "Checksums",
        kind: Kind::Text {
            placeholder: "https://cloud.example/SHA256SUMS",
            check: Check::Url,
        },
        required: true,
        advanced: false,
        help: "A `sha256sum`-style file covering the image's filename. **https \
               only**, and refused otherwise — this is the one value the whole \
               arrangement trusts, and whoever can rewrite it chooses what every \
               new guest in this cell boots.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "paused",
        label: "Paused",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Stop looking, without forgetting where this came from.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "everyMs",
        label: "Check every",
        kind: Kind::Number {
            unit: "ms",
            min: 60_000,
            max: 30 * 24 * 60 * 60 * 1000,
            step: 60_000,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Six hours when left empty. Cloud images are published daily at \
               best, and a cell that asks every minute spends its day fetching a \
               checksums file to learn nothing.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "keep",
        label: "Keep",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 50,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "How many of this family to keep. Three when left empty. An image \
               an instance was built from is never taken away, however old — the \
               guest would be unable to start on its next move.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

const IMAGE_FIELDS: &[Field] = &[
    Field {
        key: "from",
        label: "Publish from",
        kind: Kind::Ref {
            collection: "images",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        // First, and in the common path, because it is the *only* thing most
        // people do on this form. Registering bytes by hand is the rare case; an
        // operator here is nearly always putting a tenant's captured guest into
        // the catalogue where everybody can boot it, and before this that meant
        // reading four fields off one object and typing them into another.
        advanced: false,
        help: "Take the digest, format, size and source from an image that \
               already exists. Nothing is copied — an image is addressed by its \
               digest, so every node that had the bytes still has them. Anything \
               you fill in below wins over what is taken.",
        when_empty: "",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "family",
        label: "Family",
        kind: Kind::Text {
            placeholder: "debian-13",
            check: Check::Id,
        },
        required: false,
        advanced: false,
        help: "What this image is, in the words somebody would use to ask for it. \
               An instance can then name `families/debian-13` and get the newest \
               one there is, resolved when it is created and written down — so a \
               guest never changes its operating system on a restart.",
        when_empty: "Without a family this image can only be asked for by its \
                     digest, and nothing will ever offer it as \"the newest\".",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "version",
        label: "Version",
        kind: Kind::Text {
            placeholder: "20260815",
            check: Check::None,
        },
        required: false,
        // A step deeper than the family, because the family is what somebody
        // has to decide and this is what they copy off the page they downloaded
        // from. Four questions is the cap on the common path, and the four are
        // what the image *is*, its bytes, its format and where they come from.
        advanced: true,
        help: "Which one in the family, for a person to read. Newest is decided \
               by when this cell learned of the image, not by comparing this — \
               every scheme for ordering version strings is wrong for somebody's.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "digest",
        label: "Digest",
        kind: Kind::Text {
            placeholder: "sha256:…",
            check: Check::Digest,
        },
        // Not required *here*, and that is the truth rather than a
        // loosening: `from` supplies this, and a form field is required or it is
        // not — there is no way to say "unless another one is filled in". The
        // API decides, and its refusal names the field and says what to do
        // instead. Marking it required in the browser would block the one flow
        // the form was rearranged for.
        required: false,
        advanced: false,
        help: "The bytes are addressed by this, so an image cannot be replaced \
               under an instance that was built from it.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "format",
        label: "Format",
        kind: Kind::Choice {
            options: &[choice("Raw", "Raw"), choice("Qcow2", "qcow2")],
        },
        // Not required *here*, and that is the truth rather than a
        // loosening: `from` supplies this, and a form field is required or it is
        // not — there is no way to say "unless another one is filled in". The
        // API decides, and its refusal names the field and says what to do
        // instead. Marking it required in the browser would block the one flow
        // the form was rearranged for.
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "sourceUrl",
        label: "Source",
        kind: Kind::Text {
            placeholder: "https://…",
            check: Check::Url,
        },
        // Not required *here*, and that is the truth rather than a
        // loosening: `from` supplies this, and a form field is required or it is
        // not — there is no way to say "unless another one is filled in". The
        // API decides, and its refusal names the field and says what to do
        // instead. Marking it required in the browser would block the one flow
        // the form was rearranged for.
        required: false,
        // Behind "More settings" since publishing arrived, and it is the
        // right one to move: it is provenance rather than a decision, and it is
        // the one field here that fails *loudly* when it is missing — a node
        // trying to fetch says so. The common path is four questions on purpose,
        // and `from` now answers all four at once for the case people are
        // actually here for.
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "sizeBytes",
        label: "Size",
        kind: Kind::Number {
            unit: "bytes",
            min: 0,
            max: u64::MAX / 2,
            step: 1,
            scale: Scale::Bytes,
        },
        required: false,
        advanced: true,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    // Behind the disclosure, and honest about what accepting it means: the
    // API checks it against the cell's keys and stores nothing that failed,
    // so what this box produces is either a verified signature or a refusal
    // at the field. It was absent while nothing verified it — a box that
    // records a security claim nothing checks is where the claim comes from.
    Field {
        key: "signature",
        label: "Signature",
        kind: Kind::Text {
            placeholder: "base64 Ed25519 signature over the digest line",
            check: Check::None,
        },
        required: false,
        advanced: true,
        help: "An Ed25519 signature over `sha256:<digest>`, base64. Accepted only when it \
               verifies under a key the cell was started with; refused otherwise, so a \
               stored signature is a verified one.",
        when_empty: "unsigned",
        derived: false,
        at_creation: false,
    },
];

const NODE_FIELDS: &[Field] = &[
    Field {
        key: "schedulable",
        label: "Accepts work",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Turning this off drains the node: nothing new is placed, what \
               runs keeps running.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "labels",
        label: "Labels",
        kind: Kind::TextList {
            placeholder: "gpu",
            check: Check::Id,
        },
        required: false,
        advanced: false,
        help: "What placement policies match on.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "evacuate",
        label: "Move its guests away",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Separate from draining. Draining says nothing new comes here; \
               this says none of the old stays either. One migration is started \
               per guest that can move — a guest holding a passed-through \
               device cannot, and stays with the reason on it.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "fenceAfterS",
        label: "Stop own guests after",
        kind: Kind::Number {
            unit: "s",
            min: 0,
            max: 3600,
            step: 10,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "How long this node may fail to report before it stops the \
               guests it holds. Zero means it never does — and a node that \
               never does is never recovered from, because nothing can tell \
               \"unreachable\" from \"stopped\". Set it and guests marked for \
               restart can be brought up elsewhere.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "gateway",
        label: "Carries external traffic",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "A public address whose network says so is announced from here, \
               and packets for it reach the guest over the overlay. Several \
               machines may carry it — the network above sees them as equal \
               next hops — and a cell with none simply cannot use that mode.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "vcpuOvercommit",
        label: "vCPUs per core",
        kind: Kind::Number {
            unit: "per core",
            min: 0,
            max: 32,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "How many vCPUs this machine may hand out per real core. Zero \
               or one means one for one. A processor can be shared — two \
               guests that both want a core get one each in turn, and being \
               wrong costs speed — which is how nearly every fleet in the \
               world is run. There is deliberately no setting for memory: a \
               guest promised 8 GiB and handed 4 is not slow, it is killed.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "cpuBaseline",
        label: "CPU baseline",
        kind: Kind::Choice {
            options: &[
                choice("", "The host's own CPU"),
                choice("x86-64-v2", "x86-64-v2"),
                choice("x86-64-v3", "x86-64-v3"),
                choice("x86-64-v4", "x86-64-v4"),
            ],
        },
        required: false,
        // Advanced because most cells never touch it, and because the sentence
        // below is not one to meet while filling in a name.
        advanced: true,
        help: "Present the same CPU as other nodes so guests can migrate \
               between them. Guests already running keep the CPU they started \
               with and adopt this one when they next restart.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// A tenant's router: which of its networks reach each other.
const ROUTER_FIELDS: &[Field] = &[Field {
    key: "networks",
    label: "Networks",
    // No subnets here, unlike a guest's. A router joins whole networks — every
    // subnet on a network it routes is reachable from every other — so offering
    // one would be offering a precision the object does not have.
    kind: Kind::RefList {
        collection: "networks",
        also: None,
        spelling: Spelling::Name,
    },
    required: true,
    // The only decision there is. A router with no networks routes nothing,
    // so asking for them is not an advanced variation on making one — it is
    // making one.
    advanced: false,
    help: "The networks whose subnets reach each other. A network belongs to \
               at most one router.",
    when_empty: "",
    derived: false,
    at_creation: false,
}];

/// A floating IP: the address, where it comes from, and what it points at.
const FLOATING_IP_FIELDS: &[Field] = &[
    // The question as a customer asks it: which of my machines gets a public
    // address. The platform finds the guest's port and the cell's pool; the
    // subnet moved behind More settings, where naming it is how you say v6 or
    // pick among pools.
    Field {
        key: "instance",
        label: "Instance",
        kind: Kind::Ref {
            collection: "instances",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        advanced: false,
        help: "The guest this address sits in front of. Its interface and the \
               cell's public pool are found for you — name the subnet under \
               More settings to choose v6, or a particular pool.",
        when_empty: "This project has no guests yet; a public address goes in \
                     front of one.",
        derived: false,
        at_creation: true,
    },
    Field {
        key: "subnet",
        label: "Subnet",
        kind: Kind::Ref {
            collection: "subnets",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        // Behind More settings since the pool is found for you: naming the
        // subnet is how you say v6, or choose among several pools.
        advanced: true,
        help: "Where the address comes from. Left empty, the cell's public \
               pool answers — IPv4 first. Naming a subnet is how you choose \
               v6, or a particular pool.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "address",
        label: "Address",
        kind: Kind::Text {
            placeholder: "the lowest free one",
            check: Check::Address,
        },
        required: false,
        // Pinning one is the unusual case — an operator moving a known address
        // — and leaving it empty is what almost everybody wants.
        advanced: true,
        help: "Leave empty to be given the lowest address nothing else holds.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "port",
        label: "Forwards to",
        kind: Kind::Ref {
            collection: "ports",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: false,
        // Behind More settings now that the instance above is the ordinary
        // ask. Clearing it still detaches — the state a floating IP exists to
        // be in while the machine behind it is replaced.
        advanced: true,
        help: "The port this address reaches. Clearing it holds the address \
               while the machine behind it is replaced.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "delivery",
        label: "The guest",
        kind: Kind::Choice {
            options: &[
                Choice {
                    value: "Nat",
                    label: "never sees it",
                },
                Choice {
                    value: "Routed",
                    label: "holds it itself",
                },
            ],
        },
        required: false,
        advanced: false,
        help: "Held by the guest means the address is bound to its port and \
               configured inside the machine — nothing rewrites a packet, and \
               the guest can tell anybody its own address, which SIP, FTP, \
               IPsec and mDNS all need. Translated means the edge answers for \
               it and the guest never knows. A held address has to come from \
               an external network.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "announce",
        label: "Announced by",
        kind: Kind::Choice {
            options: &[
                Choice {
                    value: "",
                    label: "as the network says",
                },
                Choice {
                    value: "FromHost",
                    label: "the machine holding the guest",
                },
                Choice {
                    value: "FromGateway",
                    label: "a gateway node",
                },
            ],
        },
        required: false,
        advanced: true,
        help: "The machine holding the guest is the shortest path: nothing is \
               encapsulated for traffic to and from the world, and the route \
               follows a live migration by itself. It needs every hypervisor \
               to be allowed to peer with the router above it. A gateway node \
               needs only that one machine to peer, and costs a detour in both \
               directions.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// "This node is out of service from then, for that long."
const MAINTENANCE_WINDOW_FIELDS: &[Field] = &[
    Field {
        key: "node",
        label: "Node",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        help: "The machine going out of service.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "startsAt",
        label: "Starts",
        kind: Kind::Moment {
            default_in_minutes: 60,
        },
        required: true,
        advanced: false,
        help: "In your own timezone. A start already past means now — work \
               that has already begun is a true thing to declare.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "minutes",
        label: "For",
        kind: Kind::Number {
            unit: "minutes",
            // The grid starts where it steps, or the numbers a person means — thirty
            // minutes, an hour, four hours — all fall between two valid ones.
            min: 15,
            max: 20_160,
            step: 15,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "How long it stays open. There is no end time to keep in step \
               with this one: two fields that can disagree about the same \
               fact are one field too many.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "drain",
        label: "Move the guests off",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Off — nothing new is placed here and everything already \
               running stays put, which is what a four-minute firmware update \
               wants. On — the guests are migrated away as well, which is \
               what pulling the machine wants. A guest that cannot move is \
               left where it is, and :explainMaintenance says which.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "note",
        label: "What it is for",
        kind: Kind::Text {
            placeholder: "swapping the failed DIMM in slot 3",
            check: Check::None,
        },
        required: false,
        // One disclosure deeper than the four decisions that make a window: a
        // window declared without a note is still correct, and a form that
        // asks five things before it asks anything advanced is a wall of
        // boxes. It is the first thing under "More", not buried.
        advanced: true,
        help: "Shown wherever this window is the reason something was \
               refused, so \"no capacity\" reads as \"node-b is out until \
               03:00 for the memory swap\" instead.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// "Keep a recent snapshot of this volume, and the last few."
const SNAPSHOT_SCHEDULE_FIELDS: &[Field] = &[
    Field {
        key: "volume",
        label: "Volume",
        kind: Kind::Ref {
            collection: "volumes",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "What is snapshotted.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "everyHours",
        label: "Snapshot every",
        kind: Kind::Number {
            unit: "hours",
            min: 1,
            max: 168,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "Cheap enough to run hourly. A snapshot lives in the volume's \
               own pool: it is the right tool for going back an hour, and it \
               is lost with the pool — which is what backups are for.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "keep",
        label: "Keep",
        kind: Kind::Number {
            unit: "snapshots",
            min: 1,
            max: 336,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "Only finished ones count, so a run of failures never expires \
               the last one that worked. Snapshots taken by hand are never \
               expired.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// "Make an image out of this guest." The template workflow: build one by
/// hand, get it right, capture it, stamp out copies.
const CAPTURE_FIELDS: &[Field] = &[
    Field {
        key: "instance",
        label: "Guest",
        kind: Kind::Ref {
            collection: "instances",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "It must be stopped. A disk copied from under a running machine \
               is crash-consistent, which a template stamped out a hundred \
               times must not be — if you want a copy of a live guest, take a \
               backup instead.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "label",
        label: "Name it",
        kind: Kind::Text {
            placeholder: "debian-13-golden",
            check: Check::Id,
        },
        required: true,
        advanced: false,
        help: "What the resulting image is called. Its digest is added to \
               this — a list of hashes is not something anybody chooses from.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "target",
        label: "Keep it on",
        kind: Kind::Ref {
            collection: "backup-targets",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        help: "Where the bytes go. Any node that can reach the same path can \
               then boot guests from the image.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// Where backups are kept. Not a pool: the whole point is that it is somewhere
/// else, so that losing a pool does not lose its copies with it.
const BACKUP_TARGET_FIELDS: &[Field] = &[
    Field {
        key: "path",
        label: "Path",
        kind: Kind::Text {
            placeholder: "/srv/backups",
            check: Check::None,
        },
        required: true,
        advanced: false,
        help: "An absolute path the agent can already write to. Mounting an \
               NFS or CIFS share is the host's job, not this platform's — one \
               that managed its own mounts would be a second, worse copy of \
               what init already does.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "accepting",
        label: "Accepts backups",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Turning this off stops new copies going here. What is already \
               here stays, and can still be restored from.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "agent",
        label: "Reported on by",
        kind: Kind::Ref {
            collection: "pools",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: false,
        help: "The pool agent that says whether this path is there, whether it \
               can be written, and how much room is left. Named rather than \
               claimed by whoever gets there first — a target assigned to \
               nobody is one any agent could report on. Leave it empty and \
               nobody looks: copies are still written by the pool holding the \
               volume, and a path it cannot reach fails on the backup instead.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "verifyEveryHours",
        label: "Read a copy back every",
        kind: Kind::Number {
            unit: "hours",
            min: 0,
            max: 8760,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: false,
        help: "How often a copy here is read back and checked against what it \
               hashed to when it was written. 0 never checks, which is the \
               default: verification reads every byte of a copy, and that is \
               real I/O somebody has to decide to spend. Without it, 'the \
               backup exists' is the only thing anybody can say about it. One \
               copy per pass, the one whose last check is stalest — so a target \
               holding more copies checks each of them less often rather than \
               falling behind on the work that is not optional.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// One copy of one volume, at one moment.
const BACKUP_FIELDS: &[Field] = &[
    Field {
        key: "volume",
        label: "Volume",
        kind: Kind::Ref {
            collection: "volumes",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "What is copied.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "target",
        label: "Target",
        kind: Kind::Ref {
            collection: "backup-targets",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        // Behind More settings: where the cell keeps copies is the cell's
        // business, and a tenant cannot even list the targets — for them this
        // picker is empty, which is fine, because empty means the cell's most
        // roomy accepting target answers.
        advanced: true,
        help: "Where the copy goes. Left empty, the cell's most roomy \
               accepting target answers. A target in the volume's own pool is \
               refused: a copy beside the original is a snapshot, and is lost \
               with the pool it is in.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// "Keep a copy of this volume on that target, no older than this."
const BACKUP_SCHEDULE_FIELDS: &[Field] = &[
    Field {
        key: "volume",
        label: "Volume",
        kind: Kind::Ref {
            collection: "volumes",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "What is copied.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "target",
        label: "Target",
        kind: Kind::Ref {
            collection: "backup-targets",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        help: "Where the copies go.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "everyHours",
        label: "Copy every",
        kind: Kind::Number {
            unit: "hours",
            min: 1,
            max: 8760,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "How stale the newest copy may get before another is made. An \
               interval rather than a cron line, because the only question \
               anybody asks a schedule is when it will next run.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "keep",
        label: "Keep",
        kind: Kind::Number {
            unit: "copies",
            min: 1,
            max: 365,
            step: 1,
            scale: Scale::None,
        },
        required: true,
        advanced: false,
        help: "How many of this schedule's copies to keep. Only finished \
               copies count, so a run of failures never expires the last one \
               that worked. Copies taken by hand are never expired.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// A named set of interchangeable PCI devices, across the cell.
const DEVICE_CLASS_FIELDS: &[Field] = &[
    Field {
        key: "matches",
        label: "PCI ids",
        kind: Kind::TextList {
            placeholder: "10de:2204",
            check: Check::None,
        },
        required: true,
        advanced: false,
        help: "vendor:device, as `lspci -n` prints it. Several, because a \
               fleet buys the same accelerator across board revisions.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "description",
        label: "Description",
        kind: Kind::Text {
            placeholder: "NVIDIA A100 80GB",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "What to call it in this console.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// A load balancer: one address in front of many ports. What most operators
/// set is which network it fronts and what it answers on; the VIP itself has a
/// defensible default (the lowest free address) and lives one level deeper.
const LOAD_BALANCER_FIELDS: &[Field] = &[
    Field {
        key: "network",
        label: "Network",
        kind: Kind::Ref {
            collection: "networks",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "The network the address lives on. It scopes the service, so two \
               projects may front the same address on different networks.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "subnet",
        label: "Subnet",
        kind: Kind::Ref {
            collection: "subnets",
            filter_by: Some("network"),
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "Where the address comes from. The same counting as a port's \
               address and a floating IP's, so no two of them are ever the \
               same address.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "listeners",
        label: "Listeners",
        kind: Kind::ListenerList,
        required: true,
        // The decision a load balancer exists to state. One with no listeners
        // answers on nothing, so asking for them is not an advanced variation
        // on making one — it is making one.
        advanced: false,
        help: "The ports the address answers on. Traffic is spread across the \
               pool by connection, so one client's connection stays on one \
               member.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "members",
        label: "Members",
        kind: Kind::RefList {
            collection: "ports",
            also: None,
            spelling: Spelling::Name,
        },
        required: false,
        // Not required on purpose: an empty pool is a drained pool, which is a
        // legitimate state to hold an address in while the machines behind it
        // are replaced.
        advanced: false,
        help: "The ports behind the address — ports, not addresses, so a \
               migrated guest stays in the pool. Empty holds the address and \
               forwards to nothing.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "vip",
        label: "Address",
        kind: Kind::Text {
            placeholder: "the lowest free one",
            check: Check::Address,
        },
        required: false,
        // Pinning one is the unusual case, exactly as it is for a floating IP.
        advanced: true,
        help: "Leave empty to be given the lowest address nothing else holds.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// A storage pool, which is infrastructure in the same sense a node is: an
/// operator decides whether it takes new work, and the agent reports the rest.
const POOL_FIELDS: &[Field] = &[
    Field {
        key: "accepting",
        label: "Accepts new volumes",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "Turning this off drains the pool: nothing new is provisioned \
               into it, what it holds stays.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "labels",
        label: "Labels",
        kind: Kind::TextList {
            placeholder: "ssd",
            check: Check::Id,
        },
        required: false,
        advanced: false,
        help: "What a volume's placement matches on.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// Moving a guest, asked for as an object.
///
/// The three things an operator decides are the guest, where it should go and
/// how much of an outage they are willing to have; everything else has a
/// defensible default and lives one level deeper. `fromNode` is not asked at
/// all — the platform knows where the guest is, and a second copy of that fact
/// is a copy that can disagree with it.
const MIGRATION_FIELDS: &[Field] = &[
    Field {
        key: "instance",
        label: "Instance",
        kind: Kind::Ref {
            collection: "instances",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "fromNode",
        label: "From",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: false,
        advanced: false,
        help: "Where the guest is now. Taken from the instance.",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "toNode",
        label: "To",
        kind: Kind::Ref {
            collection: "nodes",
            filter_by: None,
            spelling: Spelling::Id,
        },
        required: true,
        advanced: false,
        help: "Only the nodes that can receive this guest can be chosen. The \
               rest are shown with the reason they cannot.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "mode",
        label: "Mode",
        kind: Kind::Choice {
            options: &[
                choice("Live", "Live"),
                choice("PostCopy", "Post-copy"),
                choice("Reboot", "Reboot"),
            ],
        },
        required: false,
        advanced: false,
        help: "What a failure costs. Under Live the guest stays where it is; \
               under Post-copy a failure mid-flight loses it.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "downtimeMs",
        label: "Downtime budget",
        kind: Kind::Number {
            unit: "ms",
            // Fifty and its multiples: with a minimum of ten and a step of fifty, the
            // grid was 10, 60, 110 — and 100, 200 and 500 were all invalid.
            min: 50,
            max: 60_000,
            step: 50,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "The pause the guest may take at the end. A busy guest needs a \
               larger budget or the transfer never converges.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "timeoutS",
        label: "Give up after",
        kind: Kind::Number {
            unit: "s",
            min: 30,
            max: 86_400,
            step: 30,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "A migration that cannot converge is one that runs until somebody \
               notices.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
    Field {
        key: "connections",
        label: "Streams",
        kind: Kind::Number {
            unit: "",
            min: 1,
            max: 8,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "Parallel streams for the transfer. One is the only value a local \
               socket accepts.",
        when_empty: "",
        derived: false,
        at_creation: false,
    },
];

/// Nothing about a reading is somebody's to set. It is a fact about a moment
/// that has passed, written once and never edited — a usage record that could
/// be changed after the fact is a bill nobody can stand behind.
const USAGE_FIELDS: &[Field] = &[];

const OPERATION_FIELDS: &[Field] = &[
    Field {
        key: "target",
        label: "Target",
        kind: Kind::Text {
            placeholder: "",
            check: Check::Name,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "verb",
        label: "Verb",
        kind: Kind::Text {
            placeholder: "",
            check: Check::None,
        },
        required: true,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "targetGeneration",
        label: "Waiting for generation",
        kind: Kind::Number {
            unit: "",
            min: 0,
            max: u64::MAX / 2,
            step: 1,
            scale: Scale::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
    Field {
        key: "requestedBy",
        label: "Requested by",
        kind: Kind::Text {
            placeholder: "",
            check: Check::None,
        },
        required: false,
        advanced: false,
        help: "",
        when_empty: "",
        derived: true,
        at_creation: false,
    },
];

pub const COLLECTIONS: &[Collection] = &[
    Collection {
        id: "instances",
        title: "Instances",
        singular: "instance",
        recheck: 0,
        condition: "Ready",
        group: "Compute",
        scope: Scope::Project,
        blurb: "Guests. What was asked for is the spec; what the node reports is \
                the status, and they are shown together.",
        fields: INSTANCE_FIELDS,
        columns: &[
            Column {
                path: "status.state",
                label: "State",
                cell: Cell::Text,
                width: 96,
            },
            Column {
                path: "spec.desiredState",
                label: "Asked",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "spec.vcpus",
                label: "vCPU",
                cell: Cell::Number { unit: "" },
                width: 64,
            },
            Column {
                path: "spec.memoryMib",
                label: "Memory",
                cell: Cell::Number { unit: "MiB" },
                width: 104,
            },
            Column {
                path: "status.node",
                label: "Node",
                cell: Cell::Mono,
                width: 128,
            },
            Column {
                path: "status.addresses.0",
                label: "Address",
                cell: Cell::Mono,
                width: 136,
            },
            // Beside the vCPU and memory columns, deliberately: those two are
            // read off `spec`, and this is the column that says whether the
            // guest is actually running on them. Without it the board showed
            // eight vCPUs for a machine running on two, converged and green.
            Column {
                path: "status.pendingChanges.0.field",
                label: "Awaiting restart",
                cell: Cell::Text,
                width: 128,
            },
        ],
        agreements: &[
            Agreement {
                label: "Power",
                asked: "desiredState",
                is: "state",
                note: "The node has not brought the guest to the state that was \
                       asked for. Its Ready condition says why.",
            },
            Agreement {
                label: "Node",
                asked: "node",
                is: "node",
                note: "The scheduler has picked a node the reporting agent is \
                       not; a migration is in flight, or the pin was changed \
                       under a running guest.",
            },
        ],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: true,
    },
    Collection {
        id: "volumes",
        title: "Volumes",
        singular: "volume",
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Project,
        blurb: "Block devices. A volume knows nothing about who has it open — \
                that is an attachment.",
        fields: VOLUME_FIELDS,
        columns: &[
            Column {
                path: "spec.sizeGib",
                label: "Size",
                cell: Cell::Number { unit: "GiB" },
                width: 96,
            },
            Column {
                path: "status.actualSizeGib",
                label: "Provisioned",
                cell: Cell::Number { unit: "GiB" },
                width: 112,
            },
            Column {
                path: "spec.pool",
                label: "Pool",
                cell: Cell::Mono,
                width: 112,
            },
            Column {
                path: "spec.encryptionKey",
                label: "Encrypted",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "no",
                },
                width: 96,
            },
        ],
        agreements: &[Agreement {
            label: "Size",
            asked: "sizeGib",
            is: "actualSizeGib",
            note: "The pool has not finished growing the device to the size \
                   that was asked for.",
        }],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "attachments",
        title: "Attachments",
        singular: "attachment",
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Project,
        blurb: "Attaching is its own object, so a crash mid-way leaves a \
                truthful record rather than a volume nobody can reattach.",
        fields: ATTACHMENT_FIELDS,
        columns: &[
            Column {
                path: "spec.volume",
                label: "Volume",
                cell: Cell::Mono,
                width: 168,
            },
            Column {
                path: "spec.instance",
                label: "Instance",
                cell: Cell::Mono,
                width: 168,
            },
            Column {
                path: "status.attached",
                label: "Open",
                cell: Cell::Yes {
                    yes: "open",
                    no: "closed",
                },
                width: 88,
            },
            Column {
                path: "status.device",
                label: "Device",
                cell: Cell::Mono,
                width: 96,
            },
        ],
        agreements: &[Agreement {
            label: "Node",
            asked: "node",
            is: "node",
            note: "The node that was asked to open it is not the node that \
                   reported. Nothing is attached until they agree.",
        }],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "networks",
        title: "Networks",
        singular: "network",
        recheck: 0,
        // `Mirrored`, not `Ready`: the network controller writes that one, and
        // nothing writes `Ready` on a network at all. Reading the wrong name
        // made every network in every cell say "not reported" for ever —
        // including the ones the fabric had been told about perfectly well.
        condition: "Mirrored",
        group: "Network",
        scope: Scope::Project,
        blurb: "One VNI on the fabric. The nodes that have it programmed are on \
                the object.",
        fields: NETWORK_FIELDS,
        columns: &[
            Column {
                path: "spec.vni",
                label: "VNI",
                cell: Cell::Mono,
                width: 96,
            },
            Column {
                path: "spec.mtu",
                label: "MTU",
                cell: Cell::Number { unit: "" },
                width: 80,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "subnets",
        title: "Subnets",
        singular: "subnet",
        // Its two occupancy columns are counted from the *ports*, by the API, on
        // the way out — so a port created or deleted moves them with nothing
        // written to the subnet and no event on this collection. A console that
        // only listened showed the occupancy as of whenever the board was
        // opened, for as long as it stayed open, which is precisely the
        // staleness that computing them on read exists to remove.
        recheck: 15,
        // Nothing reports on a subnet. Its two occupancy numbers are counted
        // by the API on the way out, and there is no agent that owns the range
        // — the network it belongs to is what gets mirrored.
        condition: "",
        group: "Network",
        scope: Scope::Project,
        blurb: "A range on a network, and the count of what IPAM has handed out \
                of it.",
        fields: SUBNET_FIELDS,
        columns: &[
            Column {
                path: "spec.cidr",
                label: "Range",
                cell: Cell::Mono,
                width: 144,
            },
            Column {
                path: "spec.gateway",
                label: "Gateway",
                cell: Cell::Mono,
                width: 128,
            },
            Column {
                path: "spec.network",
                label: "Network",
                cell: Cell::Mono,
                width: 168,
            },
            Column {
                path: "status.allocated",
                label: "In use",
                cell: Cell::Number { unit: "" },
                width: 80,
            },
            Column {
                path: "status.available",
                label: "Free",
                cell: Cell::Number { unit: "" },
                width: 80,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "ports",
        title: "Ports",
        singular: "port",
        recheck: 0,
        condition: "Ready",
        group: "Network",
        scope: Scope::Project,
        blurb: "What an instance is attached to the fabric by. Programmed means \
                the agent has it in its maps.",
        fields: PORT_FIELDS,
        columns: &[
            Column {
                path: "spec.address",
                label: "Address",
                cell: Cell::Mono,
                width: 136,
            },
            Column {
                path: "spec.mac",
                label: "MAC",
                cell: Cell::Mono,
                width: 152,
            },
            Column {
                path: "spec.subnet",
                label: "Subnet",
                cell: Cell::Mono,
                width: 168,
            },
            Column {
                path: "status.programmed",
                label: "Datapath",
                cell: Cell::Yes {
                    yes: "programmed",
                    no: "absent",
                },
                width: 112,
            },
            Column {
                path: "status.node",
                label: "Node",
                cell: Cell::Mono,
                width: 120,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "security-groups",
        title: "Security groups",
        singular: "security group",
        // `Applied` is computed by the API from the *ports* that name the group.
        // Those change without anything being written to the group, so there is
        // no event on this collection to listen for — the same reason a
        // migration is asked again.
        recheck: 15,
        condition: "Applied",
        group: "Network",
        scope: Scope::Project,
        blurb: "What a port is allowed to carry. Rules only add allowances — \
                ingress is denied, egress is allowed and replies always come \
                back, so a port in no group is not an open one.",
        fields: SECURITY_GROUP_FIELDS,
        columns: &[Column {
            path: "spec.rules",
            label: "Rules",
            cell: Cell::Count,
            width: 96,
        }],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "image-sources",
        title: "Image sources",
        singular: "image source",
        recheck: 0,
        condition: "Checked",
        group: "Compute",
        scope: Scope::Global,
        blurb: "Where a family's images come from. The cell asks the checksums file \
                what the current digest is, over https so the certificate is \
                checked, and publishes an image when the answer is one it does not \
                have. Nothing about a running guest changes: a machine keeps the \
                bytes it was built from, and \"always the newest\" means new \
                machines get it.",
        fields: IMAGE_SOURCE_FIELDS,
        columns: &[
            Column {
                path: "spec.family",
                label: "Family",
                cell: Cell::Text,
                width: 160,
            },
            Column {
                path: "status.lastChecked",
                label: "Looked",
                cell: Cell::Ago,
                width: 110,
            },
            Column {
                path: "status.published",
                label: "Published",
                cell: Cell::Text,
                width: 220,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "families",
        title: "Catalogue",
        singular: "family",
        recheck: 0,
        condition: "",
        // Derived: the API groups the images by `spec.family` on the way out.
        // Nothing stores one, which is why nothing here creates, edits or
        // deletes one — publishing an image with a family in it is how a family
        // comes to exist, and deleting the last of them is how it goes away.
        group: "Compute",
        scope: Scope::Project,
        blurb: "What to boot, by the name that stays right when the bytes change. \
                A machine resolves its family once, when it is made, and keeps \
                the build it got.",
        fields: FAMILY_FIELDS,
        columns: &[
            Column {
                path: "spec.family",
                label: "Family",
                cell: Cell::Text,
                width: 200,
            },
            Column {
                path: "spec.version",
                label: "Newest",
                cell: Cell::Text,
                width: 160,
            },
            Column {
                path: "spec.sizeBytes",
                label: "Size",
                cell: Cell::Bytes,
                width: 120,
            },
            Column {
                path: "spec.public",
                label: "Who may boot it",
                cell: Cell::Yes {
                    yes: "Everybody",
                    no: "This project",
                },
                width: 150,
            },
        ],
        agreements: &[],
        creatable: false,
        editable: false,
        deletable: false,
        explainable: false,
    },
    Collection {
        id: "images",
        title: "Images",
        singular: "image",
        recheck: 0,
        condition: "",
        // Nothing reports on these: an image is bytes and a digest; which nodes have it cached is counted by the API on the way out, and no agent owns the object.
        group: "Compute",
        scope: Scope::Project,
        blurb: "Content-addressed and immutable. Cached copies are a placement \
                preference, never a requirement.",
        fields: IMAGE_FIELDS,
        columns: &[
            Column {
                path: "spec.family",
                label: "Family",
                cell: Cell::Text,
                width: 160,
            },
            Column {
                path: "spec.version",
                label: "Version",
                cell: Cell::Text,
                width: 120,
            },
            Column {
                path: "spec.format",
                label: "Format",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "spec.sizeBytes",
                label: "Size",
                cell: Cell::Bytes,
                width: 104,
            },
            Column {
                path: "spec.digest",
                label: "Digest",
                cell: Cell::Mono,
                width: 220,
            },
            Column {
                path: "status.fetchingOn",
                label: "Arriving",
                cell: Cell::Count,
                width: 100,
            },
            Column {
                path: "status.cachedOn",
                label: "Cached on",
                cell: Cell::Count,
                width: 112,
            },
            // `verified` or `unsigned`, and nothing else — because the API
            // stores a signature only after it verified under a configured key
            // and refuses one that did not, a stored signature *is* a verified
            // one. The column that used to read yes/no off an unchecked field
            // is what made the field be refused in the first place.
            Column {
                path: "spec.signature",
                label: "Signature",
                cell: Cell::Yes {
                    yes: "verified",
                    no: "unsigned",
                },
                width: 88,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "migrations",
        title: "Migrations",
        singular: "migration",
        // `Moved` is computed by the API when the object is read, not written
        // by an agent — so the reason that says a migration ran out of time
        // appears without anything being written, and therefore without an
        // event. Asked again, or it reads as transferring for ever.
        recheck: 15,
        // Not `Ready`: a migration has moved a guest or it has not, and the
        // model writes that as `Moved` — with the reason on it.
        condition: "Moved",
        group: "Compute",
        scope: Scope::Project,
        blurb: "Moving a running guest to another node. There is no migrating \
                state anywhere: this object is the ask, and whether it is \
                finished is read from where the instance actually runs. Start \
                one from the instance you want to move — the destination is \
                chosen where the guest is.",
        fields: MIGRATION_FIELDS,
        columns: &[
            Column {
                path: "spec.instance",
                label: "Instance",
                cell: Cell::Mono,
                width: 168,
            },
            Column {
                path: "spec.fromNode",
                label: "From",
                cell: Cell::Mono,
                width: 112,
            },
            Column {
                path: "spec.toNode",
                label: "To",
                cell: Cell::Mono,
                width: 112,
            },
            Column {
                path: "spec.mode",
                label: "Mode",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "status.receiverReady",
                label: "Receiver",
                cell: Cell::Yes {
                    yes: "listening",
                    no: "not yet",
                },
                width: 104,
            },
            Column {
                path: "status.transferredMib",
                label: "Copied",
                cell: Cell::Number { unit: "MiB" },
                width: 112,
            },
        ],
        agreements: &[Agreement {
            label: "Destination",
            asked: "toNode",
            is: "node",
            note: "The node this migration was assigned to is not the one that \
                   has reported on it. Nothing is listening for the guest until \
                   they agree — the destination has to act first.",
        }],
        // Created from the instance, never from here: the destination can only
        // be offered honestly against a particular guest, and a form that asks
        // for both at once is a form that has to allow a pair the platform will
        // refuse.
        creatable: false,
        // A migration is an ask you either let finish or abandon. Editing one
        // mid-flight would change what the source was told to do after it was
        // told to do it.
        editable: false,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "nodes",
        title: "Nodes",
        singular: "node",
        recheck: 0,
        condition: "Ready",
        group: "Fleet",
        scope: Scope::Global,
        blurb: "Hypervisors. The spec is what an operator decided about one; \
                the status is what its agent last reported. Adding one here \
                creates the object and mints its registration token — shown \
                once, because the platform keeps a hash and cannot show it \
                again.",
        fields: NODE_FIELDS,
        columns: &[
            Column {
                path: "spec.schedulable",
                label: "Accepts work",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "draining",
                },
                width: 120,
            },
            Column {
                path: "status.allocated.vcpus",
                label: "vCPU used",
                cell: Cell::Number { unit: "" },
                width: 96,
            },
            Column {
                path: "status.capacity.vcpus",
                label: "vCPU capacity",
                cell: Cell::Number { unit: "" },
                width: 112,
            },
            Column {
                path: "status.allocated.memoryMib",
                label: "Memory used",
                cell: Cell::Number { unit: "MiB" },
                width: 128,
            },
            Column {
                path: "status.agentVersion",
                label: "Agent",
                cell: Cell::Mono,
                width: 96,
            },
            Column {
                path: "status.lastHeartbeat",
                label: "Heard from",
                cell: Cell::Ago,
                width: 112,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: false,
        explainable: false,
    },
    Collection {
        id: "maintenance-windows",
        title: "Maintenance",
        singular: "maintenance window",
        // Nothing about a window changes without somebody writing it, and the
        // watch delivers writes. Whether it is open right now is arithmetic a
        // browser can do on numbers it already has — asking the API again would
        // be polling for an answer nobody is going to write down.
        recheck: 0,
        condition: "",
        // Nothing reports on these: a window is a statement about time, and time needs no agent.
        group: "Cell",
        scope: Scope::Global,
        blurb: "Say in advance that a machine is going out of service, and the \
                cell stops placing work on it when the time comes — without \
                anybody being awake to flip a switch, and without anything to \
                flip back afterwards. Whether a window is upcoming, open or \
                over is read off the clock, so a window that ends puts \
                everything back by ceasing to be open.",
        fields: MAINTENANCE_WINDOW_FIELDS,
        columns: &[
            Column {
                path: "spec.node",
                label: "Node",
                cell: Cell::Mono,
                width: 160,
            },
            Column {
                path: "spec.startsAt",
                label: "Starts",
                cell: Cell::Ago,
                width: 128,
            },
            Column {
                path: "spec.minutes",
                label: "For",
                cell: Cell::Number { unit: "min" },
                width: 96,
            },
            Column {
                path: "spec.drain",
                label: "Guests",
                cell: Cell::Yes {
                    yes: "moved off",
                    no: "stay put",
                },
                width: 112,
            },
            Column {
                path: "spec.note",
                label: "For",
                cell: Cell::Text,
                width: 260,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "snapshot-schedules",
        title: "Snapshot schedules",
        singular: "snapshot schedule",
        recheck: 0,
        condition: "",
        // Nothing reports on these: a schedule is a statement about time — the snapshots it produces report, it does not.
        group: "Storage",
        scope: Scope::Project,
        blurb: "The cheap half of the pair. A snapshot lives in the volume's \
                own pool — taken in a moment, costs almost nothing, and lost \
                with the pool it is in. For a copy that survives losing the \
                pool, use a backup schedule.",
        fields: SNAPSHOT_SCHEDULE_FIELDS,
        columns: &[
            Column {
                path: "spec.volume",
                label: "Volume",
                cell: Cell::Mono,
                width: 224,
            },
            Column {
                path: "spec.everyHours",
                label: "Every",
                cell: Cell::Number { unit: "h" },
                width: 96,
            },
            Column {
                path: "spec.keep",
                label: "Keep",
                cell: Cell::Number { unit: "" },
                width: 88,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "captures",
        title: "Captures",
        singular: "capture",
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Project,
        blurb: "Build a guest by hand, get it right, capture it — then every \
                guest made from the result starts where that one left off. The \
                guest must be stopped: a disk copied from under a running \
                machine is crash-consistent, and a template is stamped out by \
                people who assume it is clean.",
        fields: CAPTURE_FIELDS,
        columns: &[
            Column {
                path: "spec.instance",
                label: "Guest",
                cell: Cell::Mono,
                width: 200,
            },
            Column {
                path: "spec.label",
                label: "Name",
                cell: Cell::Text,
                width: 176,
            },
            Column {
                // The observed half, and the only "in progress" there is: a
                // capture with no digest is one still being copied.
                path: "status.digest",
                label: "Digest",
                cell: Cell::Mono,
                width: 200,
            },
            Column {
                path: "status.finishedAt",
                label: "Finished",
                cell: Cell::Ago,
                width: 112,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: false,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "backup-targets",
        title: "Backup targets",
        singular: "backup target",
        recheck: 0,
        condition: "",
        // Nothing reports on these: a target is somewhere to put bytes; the backups that use it report, it does not.
        group: "Storage",
        scope: Scope::Global,
        blurb: "Where backups are kept. Deliberately not a pool: a copy that \
                lives beside the original is a snapshot, and is lost with the \
                pool it is in. A target in a volume's own pool is refused.",
        fields: BACKUP_TARGET_FIELDS,
        columns: &[
            Column {
                path: "spec.path",
                label: "Path",
                cell: Cell::Mono,
                width: 240,
            },
            Column {
                path: "spec.accepting",
                label: "Accepts",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "closed",
                },
                width: 96,
            },
            Column {
                // The observed half. A target whose mount has gone is a target
                // whose backups are silently not happening, and that is the one
                // thing worth a column of its own.
                path: "status.writable",
                label: "Writable",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "unreachable",
                },
                width: 120,
            },
            Column {
                path: "status.freeGib",
                label: "Free",
                cell: Cell::Number { unit: "GiB" },
                width: 112,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "backups",
        title: "Backups",
        singular: "backup",
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Project,
        blurb: "One copy of one volume, at one moment. Restoring makes a new \
                volume from a copy — never writing one back over the original, \
                which would be a command living in a spec and carried out again \
                on every resync.",
        fields: BACKUP_FIELDS,
        columns: &[
            Column {
                path: "spec.volume",
                label: "Volume",
                cell: Cell::Mono,
                width: 200,
            },
            Column {
                path: "spec.target",
                label: "Target",
                cell: Cell::Mono,
                width: 152,
            },
            Column {
                path: "status.taken",
                label: "Copied",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "not yet",
                },
                width: 96,
            },
            Column {
                path: "status.sizeGib",
                label: "Source size",
                cell: Cell::Number { unit: "GiB" },
                width: 120,
            },
            Column {
                path: "status.takenAt",
                label: "Taken",
                cell: Cell::Ago,
                width: 112,
            },
            // "Copied" says an agent once wrote bytes. This says somebody has
            // since read them back, which is a different and much stronger
            // claim — and the one an operator actually wants before a restore.
            // Empty means nobody has looked: either the target was never asked
            // to (`verifyEveryHours` is 0) or its turn has not come.
            Column {
                path: "status.verifiedAt",
                label: "Read back",
                cell: Cell::Ago,
                width: 112,
            },
            // Last, and text: a copy that failed verification is the one row
            // on this board somebody has to act on, and it says what was found
            // rather than a flag they have to go and interpret.
            Column {
                path: "status.verifyError",
                label: "Trouble",
                cell: Cell::Text,
                width: 240,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: false,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "backup-schedules",
        title: "Backup schedules",
        singular: "backup schedule",
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Project,
        blurb: "An intention, not a job queue: there should be a copy of this \
                volume no older than the interval, and the last few kept. What \
                exists is what decides — a copy still being made holds the \
                schedule, and a stuck one holds it for one interval and no \
                longer.",
        fields: BACKUP_SCHEDULE_FIELDS,
        columns: &[
            Column {
                path: "spec.volume",
                label: "Volume",
                cell: Cell::Mono,
                width: 200,
            },
            Column {
                path: "spec.target",
                label: "Target",
                cell: Cell::Mono,
                width: 152,
            },
            Column {
                path: "spec.everyHours",
                label: "Every",
                cell: Cell::Number { unit: "h" },
                width: 96,
            },
            Column {
                path: "spec.keep",
                label: "Keep",
                cell: Cell::Number { unit: "" },
                width: 88,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "audit",
        title: "Audit",
        singular: "audit record",
        recheck: 0,
        condition: "",
        // Nothing reports on these: an audit record is a fact about something that already happened.
        group: "Fleet",
        scope: Scope::Global,
        blurb: "What was refused, and who signed in. Not a log of everything \
                that happened — every successful write already leaves an \
                operation carrying its target, its verb and who asked. What no \
                operation exists for is a request that was turned down, and \
                that is the one a multi-tenant cell gets asked about later.",
        // Nothing here is settable. A record of something that already
        // happened that somebody could edit is not a record.
        fields: &[],
        columns: &[
            Column {
                path: "spec.at",
                label: "When",
                cell: Cell::Ago,
                width: 112,
            },
            Column {
                path: "spec.kind",
                label: "What",
                cell: Cell::Text,
                width: 96,
            },
            Column {
                path: "spec.subject",
                label: "Who",
                cell: Cell::Mono,
                width: 200,
            },
            Column {
                path: "spec.verb",
                label: "Verb",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "spec.target",
                label: "Reaching for",
                cell: Cell::Mono,
                width: 240,
            },
        ],
        agreements: &[],
        creatable: false,
        editable: false,
        // Kept, and not expired by anything. A record that quietly went away
        // before somebody came looking is worse than a disk they can see
        // filling — but an operator who has read it may still remove it.
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "device-classes",
        title: "Device classes",
        singular: "device class",
        recheck: 0,
        condition: "",
        // Nothing reports on these: a device class is a declaration about hardware, not a thing that converges.
        group: "Fleet",
        scope: Scope::Global,
        blurb: "Names for interchangeable hardware. An instance asks for a \
                class, never an address: an address belongs to one machine, so \
                an instance naming one could only ever run there. A device is \
                offered only when everything in its IOMMU group is free — a \
                group is passed through whole or not at all.",
        fields: DEVICE_CLASS_FIELDS,
        columns: &[
            Column {
                path: "spec.matches",
                label: "PCI ids",
                cell: Cell::Mono,
                width: 176,
            },
            Column {
                path: "spec.description",
                label: "Description",
                cell: Cell::Text,
                width: 240,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "flavors",
        title: "Flavors",
        singular: "flavor",
        recheck: 0,
        condition: "",
        // Nothing reports on these: a flavor is a named size, not a thing that converges.
        group: "Compute",
        scope: Scope::Global,
        blurb: "Named machine sizes, offered by the cell. A guest is an                 m1-small, not a hand-entered triple of numbers — the sizes that                 land on the fleet are the shapes it was bought for. Whether a                 project may also size by hand is that project's policy.",
        fields: FLAVOR_FIELDS,
        columns: &[
            Column {
                path: "spec.vcpus",
                label: "vCPUs",
                cell: Cell::Text,
                width: 72,
            },
            Column {
                path: "spec.memoryMib",
                label: "Memory (MiB)",
                cell: Cell::Text,
                width: 112,
            },
            Column {
                path: "spec.rootDiskGib",
                label: "Root disk (GiB)",
                cell: Cell::Text,
                width: 120,
            },
            Column {
                path: "spec.description",
                label: "Description",
                cell: Cell::Text,
                width: 260,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "bgp-peers",
        title: "BGP peers",
        singular: "BGP peer",
        recheck: 0,
        condition: "Ready",
        group: "Network",
        scope: Scope::Global,
        blurb: "Sessions from the cell's gateways to the routers in front of \
                it. What gets announced is derived, never listed: every \
                external subnet, and a host route for each public address that \
                is in front of something — so the router ahead of the cell and \
                this console cannot disagree about what the cell claims.",
        fields: BGP_PEER_FIELDS,
        columns: &[
            Column {
                path: "spec.peer",
                label: "Peer",
                cell: Cell::Mono,
                width: 140,
            },
            Column {
                path: "spec.peerAs",
                label: "Peer AS",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "spec.node",
                label: "Speaks from",
                cell: Cell::Mono,
                width: 112,
            },
            Column {
                path: "status.session",
                label: "Session",
                cell: Cell::Text,
                width: 112,
            },
            Column {
                path: "status.announced",
                label: "Announced",
                cell: Cell::Text,
                width: 96,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "routers",
        title: "Routers",
        singular: "router",
        recheck: 0,
        condition: "Routed",
        group: "Network",
        scope: Scope::Project,
        blurb: "Which of this project's networks reach each other. A membership \
                rather than a box: there is nothing to place and nothing to fail \
                over — the gateway answers on whichever machine the packet is \
                already on.",
        fields: ROUTER_FIELDS,
        columns: &[
            Column {
                path: "spec.networks",
                label: "Networks",
                cell: Cell::Mono,
                width: 280,
            },
            Column {
                // Assigned, not asked for. Shown because it is what appears in a
                // packet capture and in the fabric's own tables.
                path: "status.l3Vni",
                label: "Routed VNI",
                cell: Cell::Number { unit: "" },
                width: 104,
            },
            Column {
                path: "status.gatewayMac",
                label: "Gateway MAC",
                cell: Cell::Mono,
                width: 152,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "floatingips",
        title: "Floating IPs",
        singular: "floating IP",
        recheck: 0,
        condition: "Allocated",
        group: "Network",
        scope: Scope::Project,
        blurb: "Addresses that outlive the machine answering on them. The \
                address is held by the declaration, not by the port, so \
                replacing a guest does not change what the outside world \
                reaches.",
        fields: FLOATING_IP_FIELDS,
        columns: &[
            Column {
                path: "spec.address",
                label: "Address",
                cell: Cell::Mono,
                width: 152,
            },
            Column {
                path: "spec.port",
                label: "Forwards to",
                cell: Cell::Mono,
                width: 240,
            },
            Column {
                // The *observed* half. It differing from the column beside it is
                // the whole reason both are shown: that is a reconcile in
                // flight, or one that could not finish.
                path: "status.associated",
                label: "Reaching",
                cell: Cell::Mono,
                width: 136,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "load-balancers",
        title: "Load balancers",
        singular: "load balancer",
        recheck: 0,
        condition: "Ready",
        group: "Network",
        scope: Scope::Project,
        blurb: "One address in front of many ports. The fabric balances by \
                connection on whichever host traffic arrives at — there is no \
                appliance to place and nothing to fail over. Nothing here \
                probes a member's health: the pool is what was declared, not a \
                judgement about it.",
        fields: LOAD_BALANCER_FIELDS,
        columns: &[
            Column {
                path: "spec.vip",
                label: "Address",
                cell: Cell::Mono,
                width: 152,
            },
            Column {
                path: "spec.listeners",
                label: "Listeners",
                cell: Cell::Count,
                width: 96,
            },
            Column {
                path: "spec.members",
                label: "Members",
                cell: Cell::Count,
                width: 96,
            },
            Column {
                // The *observed* half. It differing from the address beside it
                // is a reconcile in flight, or one that could not finish.
                path: "status.vip",
                label: "Serving",
                cell: Cell::Mono,
                width: 152,
            },
        ],
        agreements: &[Agreement {
            label: "Address",
            asked: "vip",
            is: "vip",
            note: "The address asked for is not the one the fabric serves yet. \
                   Usually the world catching up; if it stays, the Ready \
                   condition says what is in the way.",
        }],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "pools",
        title: "Pools",
        singular: "pool",
        recheck: 0,
        condition: "Ready",
        group: "Fleet",
        scope: Scope::Global,
        blurb: "Where volumes live. The spec is what an operator decided about \
                a pool; the backend, the capacity and what is used are what its \
                agent found.",
        fields: POOL_FIELDS,
        columns: &[
            Column {
                path: "spec.accepting",
                label: "Accepts work",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "draining",
                },
                width: 120,
            },
            Column {
                // Observed, never declared: an operator writing "zfs" over an
                // LVM pool would be describing a world that does not exist.
                path: "status.backend",
                label: "Backend",
                cell: Cell::Mono,
                width: 96,
            },
            Column {
                path: "status.allocatedGib",
                label: "Used",
                cell: Cell::Number { unit: "GiB" },
                width: 104,
            },
            Column {
                path: "status.capacityGib",
                label: "Capacity",
                cell: Cell::Number { unit: "GiB" },
                width: 112,
            },
            Column {
                path: "status.agentVersion",
                label: "Agent",
                cell: Cell::Mono,
                width: 96,
            },
            Column {
                path: "status.lastHeartbeat",
                label: "Heard from",
                cell: Cell::Ago,
                width: 112,
            },
        ],
        agreements: &[],
        // A pool is declared here first, then claimed by an agent — the same
        // two halves as a node, and for the same reason.
        //
        // This used to be false, on the argument that creating one would be
        // "describing a backend nobody has attached". That stopped being true
        // when nodes became creatable: the cell is told a machine is coming,
        // and the machine is then told what it is. A pool has exactly that
        // shape, and the id in a machine's seed has to match a pool object —
        // a mismatch is a pool that claims nothing and volumes that are never
        // provisioned, with nothing anywhere saying so. Creating the object
        // here, before the seed is written, is what makes the two agree.
        //
        // Unlike a node it mints no credential: a pool agent authenticates
        // with a token an operator supplies. Worth knowing, and not a reason
        // to keep the object un-creatable — it is a reason the form does not
        // pretend to hand one out.
        creatable: true,
        editable: true,
        deletable: false,
        explainable: false,
    },
    Collection {
        id: "usage",
        title: "Usage",
        singular: "usage record",
        recheck: 0,
        condition: "",
        // Nothing reports on these: a usage reading is a fact about a moment that has passed.
        group: "Access",
        scope: Scope::Project,
        blurb: "What this project had, read once an hour and kept for ninety \
                days. A reading is a sample, not a total: something created and \
                destroyed between two of them is in neither. Quota says what is \
                in use now; this is the only thing that remembers.",
        fields: USAGE_FIELDS,
        columns: &[
            Column {
                path: "spec.at",
                label: "Taken",
                cell: Cell::Ago,
                width: 120,
            },
            Column {
                path: "spec.used.instances",
                label: "Instances",
                cell: Cell::Number { unit: "" },
                width: 90,
            },
            Column {
                path: "spec.used.vcpus",
                label: "vCPUs",
                cell: Cell::Number { unit: "" },
                width: 80,
            },
            Column {
                path: "spec.used.memoryMib",
                label: "Memory",
                cell: Cell::Number { unit: "MiB" },
                width: 110,
            },
            Column {
                path: "spec.used.volumeGib",
                label: "Storage",
                cell: Cell::Number { unit: "GiB" },
                width: 110,
            },
            Column {
                path: "spec.used.floatingIps",
                label: "Public addresses",
                cell: Cell::Number { unit: "" },
                width: 130,
            },
        ],
        agreements: &[],
        creatable: false,
        editable: false,
        deletable: false,
        explainable: false,
    },
    Collection {
        id: "operations",
        title: "Operations",
        singular: "operation",
        recheck: 0,
        condition: "Ready",
        group: "Fleet",
        scope: Scope::Project,
        blurb: "Something that could not finish inside a request. Done is \
                computed from the target's own convergence, so an operation \
                cannot disagree with the object it describes.",
        fields: OPERATION_FIELDS,
        columns: &[
            Column {
                path: "spec.verb",
                label: "Verb",
                cell: Cell::Text,
                width: 80,
            },
            Column {
                path: "spec.target",
                label: "Target",
                cell: Cell::Mono,
                width: 260,
            },
            Column {
                path: "status.done",
                label: "Finished",
                cell: Cell::Yes {
                    yes: "yes",
                    no: "not yet",
                },
                width: 96,
            },
            Column {
                path: "status.error",
                label: "Error",
                cell: Cell::Text,
                width: 200,
            },
        ],
        agreements: &[],
        creatable: false,
        editable: false,
        deletable: false,
        explainable: false,
    },
    Collection {
        id: "users",
        title: "Users",
        singular: "user",
        recheck: 0,
        condition: "",
        // Nothing reports on these: an account is a declaration; no agent runs one.
        group: "Access",
        scope: Scope::Global,
        blurb: "Who can sign in. A password is set from the row rather than \
                shown on it — the platform stores a hash and cannot recover the \
                original, which is the point.",
        fields: USER_FIELDS,
        columns: &[
            Column {
                path: "spec.displayName",
                label: "Name",
                cell: Cell::Text,
                width: 176,
            },
            Column {
                path: "spec.email",
                label: "Email",
                cell: Cell::Text,
                width: 200,
            },
            Column {
                path: "spec.cellAdmin",
                label: "Operator",
                cell: Cell::Text,
                width: 88,
            },
            Column {
                path: "spec.disabled",
                label: "Disabled",
                cell: Cell::Text,
                width: 88,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "ceph-clusters",
        title: "Ceph",
        singular: "Ceph cluster",
        // Zero, like every collection whose status is *written*. A bootstrap
        // takes minutes, which makes polling tempting — and the controller
        // writes each step's result, so the watch delivers it. Polling here
        // would ask again for something already on its way.
        recheck: 0,
        condition: "Ready",
        group: "Storage",
        scope: Scope::Global,
        blurb: "Cluster storage every node reaches, instead of a pool per \
                machine. Optional: a cell with directory pools is a working \
                cell, and nothing here turns itself on. Choosing a disk for an \
                OSD erases it.",
        fields: CEPH_FIELDS,
        columns: &[
            Column {
                path: "status.phase",
                label: "Phase",
                cell: Cell::Text,
                width: 120,
            },
            Column {
                path: "status.monitorsUp",
                label: "Monitors",
                cell: Cell::Count,
                width: 96,
            },
            Column {
                path: "status.osdsUp",
                label: "OSDs",
                cell: Cell::Count,
                width: 80,
            },
        ],
        agreements: &[Agreement {
            label: "Monitors",
            asked: "monitors",
            is: "monitorsUp",
            note: "A monitor that was chosen is not running. Until the \
                       quorum is what was asked for, the cluster tolerates \
                       fewer failures than it looks like it does.",
        }],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "roles",
        title: "Roles",
        singular: "role",
        recheck: 0,
        condition: "",
        // Nothing reports on a role: it is a definition, not a thing an agent
        // runs.
        group: "Access",
        scope: Scope::Global,
        blurb: "The four rungs say how much somebody may do. A role here says \
                *what* — collection by collection, for the case a rung cannot \
                express: may restart the database machines, may not touch the \
                network.",
        fields: ROLE_FIELDS,
        columns: &[
            Column {
                path: "spec.displayName",
                label: "Name",
                cell: Cell::Text,
                width: 220,
            },
            Column {
                path: "spec.description",
                label: "What it is for",
                cell: Cell::Text,
                width: 320,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "folders",
        title: "Folders",
        singular: "folder",
        recheck: 0,
        condition: "",
        // Nothing reports on a folder: it is a place in a tree, not a thing an
        // agent runs.
        group: "Access",
        scope: Scope::Global,
        blurb: "A place to put projects, and a place to grant a role once instead \
                of forty times. Roles granted here reach everything below.",
        fields: FOLDER_FIELDS,
        columns: &[
            Column {
                path: "spec.displayName",
                label: "Name",
                cell: Cell::Text,
                width: 240,
            },
            Column {
                path: "spec.parent",
                label: "Inside",
                cell: Cell::Text,
                width: 240,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
    Collection {
        id: "projects",
        title: "Projects",
        singular: "project",
        recheck: 0,
        condition: "Ready",
        group: "Fleet",
        scope: Scope::Global,
        blurb: "The quota and access anchor. Usage is counted from what exists, \
                never decremented by hand.",
        fields: PROJECT_FIELDS,
        columns: &[
            Column {
                path: "spec.displayName",
                label: "Name",
                cell: Cell::Text,
                width: 200,
            },
            Column {
                // Second, right after the name: where a project lives is a fact
                // about the project itself, not one of its measurements.
                path: "spec.cell",
                label: "Cell",
                cell: Cell::Text,
                width: 96,
            },
            Column {
                path: "status.used.instances",
                label: "Instances",
                cell: Cell::Number { unit: "" },
                width: 96,
            },
            Column {
                path: "spec.quota.instances",
                label: "Instance quota",
                cell: Cell::Number { unit: "" },
                width: 112,
            },
            Column {
                path: "status.used.vcpus",
                label: "vCPUs",
                cell: Cell::Number { unit: "" },
                width: 88,
            },
            Column {
                path: "status.used.memoryMib",
                label: "Memory",
                cell: Cell::Number { unit: "MiB" },
                width: 120,
            },
        ],
        agreements: &[],
        creatable: true,
        editable: true,
        deletable: true,
        explainable: false,
    },
];

/// The collections, as the script reads them.
pub fn as_json() -> String {
    serde_json::to_string(COLLECTIONS).expect("the schema is plain data")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(id: &str) -> Option<&'static Collection> {
        COLLECTIONS.iter().find(|c| c.id == id)
    }

    #[test]
    fn every_collection_in_the_contract_has_a_screen() {
        // The list is from docs/rest-contract.md, plus `migrations`, which the
        // API serves and gRPC exposes but the document does not yet mention —
        // it is real in code and absent from the contract until the lines are
        // added, and this list is deliberately ahead of it rather than the
        // console being the last to know. A collection the API serves and the
        // console cannot show is a hole an operator falls into, and it is
        // invisible until somebody goes looking for it.
        for id in [
            "projects",
            "instances",
            "volumes",
            "attachments",
            "networks",
            "subnets",
            "ports",
            "routers",
            "security-groups",
            "images",
            "nodes",
            "pools",
            "operations",
            "migrations",
            "floatingips",
            "load-balancers",
            "device-classes",
            "backup-targets",
            "backups",
            "backup-schedules",
            "captures",
            "snapshot-schedules",
            "audit",
            "users",
            "ceph-clusters",
            "maintenance-windows",
            "usage",
            "families",
            "folders",
            "roles",
        ] {
            assert!(find(id).is_some(), "no screen for {id}");
        }
        assert_eq!(
            COLLECTIONS.len(),
            33,
            "a collection was added without a screen"
        );
        // This list is maintained by hand, and on 2026-08-19 it was two short:
        // `pools` and `snapshots` were served by the API and had no screen, and
        // this test passed anyway because it only checks what somebody
        // remembered to write down. The list that cannot fall behind is the
        // API's own, and comparing against it needs both crates — so that
        // check lives in `velstra-cloud-api/tests/console_covers_the_model.rs`
        // and this one is the fast local echo of it.
    }

    #[test]
    fn every_collection_says_which_condition_it_is_judged_by() {
        // The verdict comes from one function reading one condition. A
        // collection that names a condition **nothing ever writes** reads as
        // "not reported" for ever — which is the failure this catches, and it
        // is not hypothetical: on a real cell it put a hundred and nine objects
        // on the attention list, three of which were actually wrong.
        //
        // So a collection either names a condition somebody writes, or it says
        // outright that nobody does. The second list is written out here rather
        // than inferred, because "nothing reports on this" is a claim about the
        // whole platform and adding a collection to it should take a moment's
        // thought.
        let unreported: &[&str] = &[
            // Records of something that already happened.
            "audit",
            "usage",
            // Declarations. Nothing runs an account, a hardware class, a place
            // to put bytes, or a statement about time.
            "users",
            "device-classes",
            "backup-targets",
            "maintenance-windows",
            // Counted on the way out by the API, owned by no agent: the network
            // a subnet belongs to is what gets mirrored.
            "subnets",
            "images",
            "snapshot-schedules",
            // Derived on the way out of the API by grouping the images. There is
            // no object to report on.
            "families",
            // A place in a tree, not a thing an agent runs.
            "folders",
            // A definition. Nothing runs one either.
            "roles",
            // A named size. A definition, like a device class.
            "flavors",
        ];
        for c in COLLECTIONS {
            let says_nobody = c.condition.is_empty();
            assert_eq!(
                says_nobody,
                unreported.contains(&c.id),
                "{} {} that nothing reports on it, and the list here says {}",
                c.id,
                if says_nobody { "says" } else { "does not say" },
                unreported.contains(&c.id)
            );
        }
        assert_eq!(find("migrations").unwrap().condition, "Moved");
        assert_eq!(find("instances").unwrap().condition, "Ready");
    }

    /// A stepper's grid has to contain the numbers people mean.
    ///
    /// A `<input type=number>` counts valid values from the **minimum**, in
    /// steps: `min + n × step`. So a minimum of 256 with a step of 512 makes
    /// the valid memory sizes 256, 768, 1280, 1792, 2304 — and 2048, 4096 and
    /// 8192, which is what anybody actually asks for, all fall between two of
    /// them.
    ///
    /// Found by opening the form: the new-instance sheet showed its own
    /// default of 2048 MiB as invalid before anybody had touched it.
    ///
    /// The rule is that the grid starts on a round number — `min % step == 0`,
    /// or the minimum *is* the step. That is enough to put every multiple of
    /// the step inside the range, which is what a stepper is for.
    #[test]
    fn every_stepper_can_reach_the_numbers_people_mean() {
        for c in COLLECTIONS {
            for f in c.fields {
                let Kind::Number { min, max, step, .. } = f.kind else {
                    continue;
                };
                assert!(step > 0, "{}.{} steps by nothing", c.id, f.key);
                assert_eq!(
                    min % step,
                    0,
                    "{}.{} counts from {min} in steps of {step}, so the values it admits are \
                     {min}, {}, {} … — every round number a person would type falls between two \
                     of them",
                    c.id,
                    f.key,
                    min + step,
                    min + 2 * step
                );
                assert!(
                    max >= min + step,
                    "{}.{} has no room for a single step",
                    c.id,
                    f.key
                );
            }
        }
    }

    #[test]
    fn what_is_decided_by_a_clock_is_asked_again() {
        // A condition the API computes when the object is read can change with
        // nothing being written — a migration that has run past its timeout is
        // decided by time passing. No write, no event, so a screen that only
        // listens shows it as transferring until somebody reloads the page.
        // Everything whose status is written by an agent needs no such thing,
        // and polling it would be a request per screen per interval for nothing.
        // The three the API computes on read. A migration's verdict comes from
        // the clock; a security group's membership and a subnet's occupancy are
        // both counted from the ports that name them. None of the three
        // produces a write, so none produces an event on the object being
        // looked at, and a screen that only listens is stale for as long as it
        // is open.
        //
        // `subnets` was missing, and the assertion below is what kept it
        // missing: it asserts that everything not on this list polls for
        // *nothing*, so the list is not a note about which collections happen to
        // poll — it is the claim that the rest do not need to. A collection that
        // grows a computed field and is not added here is one this test now
        // insists is fine.
        let computed = ["migrations", "security-groups", "subnets"];
        for id in computed {
            assert!(
                find(id).unwrap().recheck > 0,
                "{id} has a computed condition and is only listened for"
            );
        }
        for c in COLLECTIONS.iter().filter(|c| !computed.contains(&c.id)) {
            assert_eq!(c.recheck, 0, "{} polls for a status that is written", c.id);
        }
    }

    #[test]
    fn a_migration_is_started_where_the_guest_is() {
        // Offering "New migration" over the collection would mean a form that
        // asks for a guest and a destination at once — and it cannot say which
        // destinations are possible until the guest is known, so it would have
        // to offer nodes the platform is about to refuse. The instance's own
        // screen is the only place both halves are known.
        let m = find("migrations").unwrap();
        assert!(!m.creatable, "migrations are offered as a blank form");
        assert!(!m.editable, "a migration in flight can be edited");
        assert!(m.deletable, "a migration cannot be abandoned");
        // Abandoning is genuinely different per mode, so the mode has to be on
        // the object rather than implied by a default.
        assert!(m.fields.iter().any(|f| f.key == "mode"));
        // Derived exactly as the contract derives an attachment's node: stated,
        // never asked.
        let from = m.fields.iter().find(|f| f.key == "fromNode").unwrap();
        assert!(from.derived && !from.required);
    }

    #[test]
    fn a_picker_can_only_point_at_a_collection_that_exists() {
        // A `Ref` whose collection is misspelled renders as an empty select,
        // which reads as "there are none" rather than as a bug.
        for c in COLLECTIONS {
            for f in c.fields {
                let target = match f.kind {
                    Kind::Ref { collection, .. }
                    | Kind::RefList { collection, .. }
                    // A disk picker points at a collection too — it reads
                    // `status.devices` off every object in it — and a misspelling
                    // there renders as a node list with no disks anywhere, which
                    // reads as "this cell has no spare disks".
                    | Kind::DiskList { collection, .. } => Some(collection),
                    _ => None,
                };
                if let Some(t) = target {
                    assert!(find(t).is_some(), "{}.{} points at {t}", c.id, f.key);
                }
            }
        }
    }

    #[test]
    fn every_refusal_is_a_sentence_whose_placeholders_close() {
        // The script substitutes `{field}` out of the state a device reports. An
        // unclosed brace is a refusal that renders with a brace in it — on the
        // one control where the sentence *is* the feature — and the API-side
        // test that pins these against the model would panic on it rather than
        // say what was wrong.
        for f in COLLECTIONS.iter().flat_map(|c| c.fields) {
            let Kind::DiskList {
                refusals,
                too_small,
                unknown,
                ..
            } = f.kind
            else {
                continue;
            };
            let texts = refusals
                .iter()
                .map(|r| r.text)
                .chain([too_small, unknown])
                .collect::<Vec<_>>();
            for text in texts {
                assert_eq!(
                    text.matches('{').count(),
                    text.matches('}').count(),
                    "a refusal has an unbalanced placeholder: {text}"
                );
                assert!(!text.is_empty(), "a refusal says nothing");
                assert!(
                    !refusals.iter().any(|r| r.kind == "Free"),
                    "a free disk is offered, not refused"
                );
            }
        }
    }

    #[test]
    fn the_disk_picker_says_what_cannot_be_undone_on_the_control() {
        // The collection's blurb says it too, at the top of the dialog — and by
        // the time somebody is reading a list of disks they are past the blurb.
        // This is the sentence that has to be in the way of the click.
        let ceph = find("ceph-clusters").unwrap();
        let Some(Kind::DiskList { warning, .. }) = ceph
            .fields
            .iter()
            .map(|f| f.kind)
            .find(|k| matches!(k, Kind::DiskList { .. }))
        else {
            panic!("the Ceph screen has no disk picker, so nobody can choose an OSD");
        };
        assert!(
            warning.contains("erases"),
            "the disk picker's warning does not say what happens to the disk: {warning}"
        );
    }

    #[test]
    fn a_node_is_referred_to_by_its_bare_id_and_everything_else_by_name() {
        // Ownership is decided by comparing `spec.node` to the agent's own name
        // by string equality. A full resource name there assigns the object to a
        // node that does not answer to it: the agent never becomes its owner,
        // nothing ever starts, and there is no error anywhere to find. The API
        // refuses the wrong spelling at the door, so a picker that offers one is
        // a form that cannot be submitted.
        for c in COLLECTIONS {
            for f in c.fields {
                let (collection, spelling) = match f.kind {
                    Kind::Ref {
                        collection,
                        spelling,
                        ..
                    }
                    | Kind::RefList {
                        collection,
                        spelling,
                        ..
                    } => (collection, spelling),
                    _ => continue,
                };
                // A cell-scoped root object is referred to by its bare id —
                // `hv-1`, not `nodes/hv-1`; `gpu-a100`, not
                // `device-classes/gpu-a100`. Everything else lives under a
                // project and is named in full.
                //
                // Expressed as the rule rather than as a list of collections,
                // because the list was the accident: the reason has always
                // been the scope, and hardcoding one name meant the second
                // cell-scoped collection to arrive looked like a mistake.
                let want = match find(collection).map(|t| t.scope) {
                    Some(Scope::Global) => Spelling::Id,
                    _ => Spelling::Name,
                };
                assert_eq!(
                    spelling, want,
                    "{}.{} points at {collection} and is spelled wrongly",
                    c.id, f.key
                );
            }
        }
    }

    #[test]
    fn a_filtered_picker_filters_on_a_field_the_form_has() {
        // `filter_by` narrows one picker by another's value, so the other has
        // to be on the same form — otherwise the filter silently matches
        // nothing and the picker looks empty.
        for c in COLLECTIONS {
            for f in c.fields {
                if let Kind::Ref {
                    filter_by: Some(by),
                    ..
                } = f.kind
                {
                    assert!(
                        c.fields.iter().any(|o| o.key == by),
                        "{}.{} filters by {by}, which is not on the form",
                        c.id,
                        f.key
                    );
                }
            }
        }
    }

    #[test]
    fn an_agreement_names_one_half_of_each_side() {
        // The pair is what makes drift legible; a typo in either path renders
        // as "asked for nothing, got nothing", which reads as agreement.
        for c in COLLECTIONS {
            for a in c.agreements {
                assert!(
                    !a.asked.is_empty() && !a.is.is_empty(),
                    "{} has a half-empty pair",
                    c.id
                );
                assert!(
                    !a.note.is_empty(),
                    "{}'s {} pair says nothing about what a gap means",
                    c.id,
                    a.label
                );
            }
        }
    }

    #[test]
    fn a_column_reads_from_spec_or_status_and_nothing_else() {
        for c in COLLECTIONS {
            for col in c.columns {
                assert!(
                    col.path.starts_with("spec.") || col.path.starts_with("status."),
                    "{}: column {} reads from neither half",
                    c.id,
                    col.path
                );
                assert!(
                    col.width >= 48,
                    "{}: column {} is too narrow to read",
                    c.id,
                    col.path
                );
            }
        }
    }

    #[test]
    fn what_cannot_be_edited_is_not_offered_as_a_form() {
        // Operations are computed; a form over one would send a PATCH the API
        // is required to refuse.
        let ops = find("operations").unwrap();
        assert!(!ops.creatable && !ops.editable && !ops.deletable);
        assert!(ops.fields.iter().all(|f| f.derived));
    }

    /// One question per field, asked once.
    ///
    /// Two `Field`s with the same key are two controls writing one value: the
    /// form renders both, the person answers whichever they meet first, and
    /// whichever renders last is what gets sent. A port's form asked for its
    /// security groups twice — once as a picker over the groups that exist and
    /// once as free text — and it took opening the form to see it.
    #[test]
    fn no_collection_asks_the_same_thing_twice() {
        for c in COLLECTIONS {
            let mut seen: Vec<&str> = Vec::new();
            for f in c.fields {
                assert!(
                    !seen.contains(&f.key),
                    "{} has two fields for `{}` — two controls writing one value, and the \
                     answer is whichever rendered last",
                    c.id,
                    f.key
                );
                seen.push(f.key);
            }
        }
    }

    #[test]
    fn the_common_path_is_short() {
        // Simplicity is showing the common path first. A create form with ten
        // fields before "More settings" is a form nobody reads.
        //
        // The bound was seven, and seven turned out to *be* the complaint: an
        // instance asked for an id, an image, three sizes, a power state, a
        // port list and a key list before it asked anything advanced, and the
        // form read as a wall of boxes. Seven is not a common path — it is
        // every field that happens not to be exotic. Four is the number that
        // forces the question "would almost everyone fill this in", and
        // everything else keeps existing exactly where it was, one disclosure
        // deeper.
        for c in COLLECTIONS.iter().filter(|c| c.creatable) {
            let common = c
                .fields
                .iter()
                .filter(|f| !f.advanced && !f.derived)
                .count();
            assert!(
                common <= 4,
                "{} asks {common} things before it asks anything advanced",
                c.id
            );
        }
    }

    #[test]
    fn a_required_field_is_never_hidden_behind_more_settings() {
        for c in COLLECTIONS {
            for f in c.fields {
                assert!(
                    !(f.required && f.advanced),
                    "{}.{} is required but only visible one level deeper",
                    c.id,
                    f.key
                );
            }
        }
    }

    #[test]
    fn the_schema_serialises_flat_enough_to_read() {
        let json = as_json();
        // The script branches on `kind` and `cell`; serde's internal tagging is
        // what puts them there, and a change to `#[serde(tag)]` would break the
        // script silently.
        assert!(
            json.contains(r#""kind":"ref""#),
            "field kinds lost their tag"
        );
        assert!(json.contains(r#""cell":"mono""#), "cells lost their tag");
        assert!(
            json.contains(r#""scope":"global""#),
            "scopes lost their spelling"
        );
    }
}
