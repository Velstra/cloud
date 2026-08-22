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
    /// Why this exists, in a sentence, shown under the control. Empty when the
    /// label already says everything — a hint that repeats the label is noise.
    pub help: &'static str,
    /// Set by the platform, not by an operator: shown, never editable.
    pub derived: bool,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "monitors",
        label: "Monitors",
        kind: Kind::RefList {
            collection: "nodes",
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
        derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "paused",
        label: "Paused",
        kind: Kind::Switch,
        required: false,
        advanced: true,
        help: "Stops the deployment where it stands. Nothing is torn down, and \
               turning it off carries on from there.",
        derived: false,
    },
];

const PROJECT_FIELDS: &[Field] = &[
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
        derived: false,
    },
    Field {
        key: "parent",
        label: "Parent",
        kind: Kind::Text {
            placeholder: "organizations/o1",
            check: Check::Name,
        },
        required: false,
        advanced: true,
        help: "Policies and quota are inherited from here.",
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
    },
];

const INSTANCE_FIELDS: &[Field] = &[
    Field {
        key: "image",
        label: "Image",
        kind: Kind::Ref {
            collection: "images",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "",
        derived: false,
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
        derived: false,
    },
    Field {
        key: "memoryMib",
        label: "Memory",
        kind: Kind::Number {
            unit: "MiB",
            min: 256,
            max: 4_194_304,
            step: 512,
            scale: Scale::Mib,
        },
        required: true,
        advanced: false,
        help: "",
        derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "ports",
        label: "Ports",
        kind: Kind::RefList {
            collection: "ports",
            spelling: Spelling::Name,
        },
        required: false,
        // A guest with no NIC is unusual but it is not the thing most people
        // are deciding while they name a machine.
        advanced: true,
        help: "Attached in this order.",
        derived: false,
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
        help: "",
        derived: false,
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
        derived: false,
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
        derived: false,
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
        help: "Instances in one group are never placed on the same node.",
        derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "pool",
        label: "Pool",
        kind: Kind::Text {
            placeholder: "nvme",
            check: Check::Id,
        },
        required: true,
        advanced: false,
        help: "",
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: true,
    },
    Field {
        key: "readOnly",
        label: "Read only",
        kind: Kind::Switch,
        required: false,
        advanced: false,
        help: "",
        derived: false,
    },
];

const NETWORK_FIELDS: &[Field] = &[
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
        required: true,
        advanced: false,
        help: "",
        derived: false,
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
        help: "Assigned by the controller from the cell's range.",
        derived: true,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
    derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "securityGroups",
        label: "Security groups",
        kind: Kind::RefList {
            collection: "security-groups",
            spelling: Spelling::Name,
        },
        required: false,
        advanced: false,
        help: "Rules only ever add allowances, so ordering does not matter and \
               two groups cannot contradict each other. With none, the port \
               keeps the platform's default: nothing in, everything out, \
               replies always.",
        derived: false,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "securityGroups",
        label: "Security groups",
        kind: Kind::TextList {
            placeholder: "web",
            check: Check::Id,
        },
        required: false,
        advanced: false,
        help: "",
        derived: false,
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
        derived: false,
    },
];

const IMAGE_FIELDS: &[Field] = &[
    Field {
        key: "digest",
        label: "Digest",
        kind: Kind::Text {
            placeholder: "sha256:…",
            check: Check::Digest,
        },
        required: true,
        advanced: false,
        help: "The bytes are addressed by this, so an image cannot be replaced \
               under an instance that was built from it.",
        derived: false,
    },
    Field {
        key: "format",
        label: "Format",
        kind: Kind::Choice {
            options: &[choice("Raw", "Raw"), choice("Qcow2", "qcow2")],
        },
        required: true,
        advanced: false,
        help: "",
        derived: false,
    },
    Field {
        key: "sourceUrl",
        label: "Source",
        kind: Kind::Text {
            placeholder: "https://…",
            check: Check::Url,
        },
        required: true,
        advanced: false,
        help: "",
        derived: false,
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
        derived: false,
    },
    // There is deliberately no `signature` field. It used to be here, with the
    // help text "Verified before a node will boot it" — and nothing in the
    // platform has ever verified it. A box that records a security claim
    // nothing checks is not a neutral convenience: it is where the claim comes
    // from. The API now refuses one, so a box here could only produce an error.
    // See `ImageSpec::signature`.
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
        derived: false,
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
        derived: false,
    },
];

/// A tenant's router: which of its networks reach each other.
const ROUTER_FIELDS: &[Field] = &[Field {
    key: "networks",
    label: "Networks",
    kind: Kind::RefList {
        collection: "networks",
        spelling: Spelling::Name,
    },
    required: true,
    // The only decision there is. A router with no networks routes nothing,
    // so asking for them is not an advanced variation on making one — it is
    // making one.
    advanced: false,
    help: "The networks whose subnets reach each other. A network belongs to \
               at most one router.",
    derived: false,
}];

/// A floating IP: the address, where it comes from, and what it points at.
const FLOATING_IP_FIELDS: &[Field] = &[
    Field {
        key: "subnet",
        label: "Subnet",
        kind: Kind::Ref {
            collection: "subnets",
            filter_by: None,
            spelling: Spelling::Name,
        },
        required: true,
        advanced: false,
        help: "Where the address comes from. The same counting as a port's \
               address, so the two are never the same address.",
        derived: false,
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
        derived: false,
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
        // Not advanced, and not required: detaching is the ordinary state a
        // floating IP exists to be in, so it has to be as easy to clear as to
        // set.
        advanced: false,
        help: "The port this address reaches. Clearing it holds the address \
               while the machine behind it is replaced.",
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: false,
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
        derived: true,
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
        derived: false,
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
        derived: false,
    },
    Field {
        key: "downtimeMs",
        label: "Downtime budget",
        kind: Kind::Number {
            unit: "ms",
            min: 10,
            max: 60_000,
            step: 50,
            scale: Scale::None,
        },
        required: false,
        advanced: true,
        help: "The pause the guest may take at the end. A busy guest needs a \
               larger budget or the transfer never converges.",
        derived: false,
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
        derived: false,
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
        derived: false,
    },
];

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
        derived: true,
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
        derived: true,
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
        derived: true,
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
        derived: true,
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
        condition: "Ready",
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
        condition: "Ready",
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
        id: "images",
        title: "Images",
        singular: "image",
        recheck: 0,
        condition: "Ready",
        group: "Compute",
        scope: Scope::Project,
        blurb: "Content-addressed and immutable. Cached copies are a placement \
                preference, never a requirement.",
        fields: IMAGE_FIELDS,
        columns: &[
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
                path: "status.cachedOn",
                label: "Cached on",
                cell: Cell::Count,
                width: 112,
            },
            // And deliberately no `Signed` column. It read yes or no off a
            // string nothing had checked, at a glance, in a list — which is the
            // worst possible place for an unverified claim to appear, because
            // that is exactly how somebody decides an image is safe.
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
                the status is what its agent last reported.",
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
        creatable: false,
        editable: true,
        deletable: false,
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
        // A pool comes into existence when its agent registers one, the same
        // way a node does. Creating one here would be describing a backend
        // nobody has attached.
        creatable: false,
        editable: true,
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
        condition: "Ready",
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
            "users",
            "ceph-clusters",
        ] {
            assert!(find(id).is_some(), "no screen for {id}");
        }
        assert_eq!(
            COLLECTIONS.len(),
            17,
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
        // collection that names a condition nothing ever writes reads as "not
        // reported" forever, which is the failure this catches — and an empty
        // one would make the function fall back to the whole list and guess.
        for c in COLLECTIONS {
            assert!(!c.condition.is_empty(), "{} names no condition", c.id);
        }
        assert_eq!(find("migrations").unwrap().condition, "Moved");
        assert_eq!(find("instances").unwrap().condition, "Ready");
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
                    } => (collection, spelling),
                    _ => continue,
                };
                let want = if collection == "nodes" {
                    Spelling::Id
                } else {
                    Spelling::Name
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
