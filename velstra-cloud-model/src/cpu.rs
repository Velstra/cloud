//! What CPU a node has, what a guest was given, and who may migrate to whom.
//!
//! ## The shape of the problem
//!
//! A guest is compiled against an instruction set. Move it to a machine that
//! lacks one of those instructions and it does not fail at the move — it fails
//! later, inside the guest, at whatever moment the missing instruction is next
//! executed. That is the failure this module exists to make impossible, and
//! the reason every decision here fails closed.
//!
//! ## The three pieces
//!
//! - [`NodeCpu`] — what a node *has*. Reported by the agent, decided by
//!   nobody. The disk and Ceph reporting already work this way.
//! - [`GuestCpu`] — what a guest *was given*, recorded when it booted. Not
//!   derived from its node, ever: see the invariant below.
//! - [`migration_domains`] and [`advise`] — pure functions over those two,
//!   computed on read so they cannot drift from the fleet they describe.
//!
//! ## The invariant
//!
//! **A guest's CPU is a fact recorded at boot, not a property of the node it
//! sits on.** Ask "can the destination present what this guest already sees",
//! never "what could the destination present". The difference only shows up
//! once a baseline is declared over a fleet that is already running guests —
//! and at that point the derived-from-node answer will happily move a guest
//! onto a machine missing instructions it has been executing for hours.
//!
//! ## Why a capability rather than an assumption
//!
//! QEMU can present a CPU other than the host's. Cloud Hypervisor cannot: it
//! derives the guest CPUID from `KVM_GET_SUPPORTED_CPUID` and has no model to
//! name. Neither can on aarch64. So "can this VMM mask" is carried as a
//! reported fact ([`NodeCpu::can_mask`]) rather than assumed either way. When
//! Cloud Hypervisor gains CPU models the flag flips and domains merge; nothing
//! else here changes.
//!
//! ## What is tested
//!
//! Everything in this file, because all of it is pure. The parts that cannot
//! be tested here — whether a given host really can present a given model —
//! are deliberately not decided here: they are enforced at boot by the VMM
//! itself (`-cpu <model>,enforce`), which is the only authority that knows.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A psABI feature level: the coarse currency.
///
/// Vendor-neutral, stable, and the thing distributions actually state as a
/// requirement — which is what makes it the right unit for a person to reason
/// in and for an instance to demand. It is deliberately *not* the unit of
/// enforcement: two v3 machines can still differ in ways that matter, so a
/// refusal always names flags (see [`difference_for`]).
///
/// Spelled the same everywhere — in JSON, in the proto, on a `-cpu` command
/// line and in a sentence to a person — because a value with two spellings is
/// a value somebody will eventually compare against the wrong one. The
/// renames below are not cosmetic: without them serde writes `v3`, which is
/// neither what an operator types nor what QEMU calls the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CpuLevel {
    /// Baseline x86-64. What `qemu64` provides, and what modern distributions
    /// have stopped targeting.
    #[serde(rename = "x86-64-v1")]
    V1,
    /// Adds SSE3, SSSE3, SSE4.1, SSE4.2, POPCNT, CMPXCHG16B and LAHF/SAHF.
    /// RHEL 9 and CentOS Stream 9 require this to boot at all.
    #[serde(rename = "x86-64-v2")]
    V2,
    /// Adds AVX, AVX2, BMI1, BMI2, F16C, FMA, LZCNT and MOVBE.
    #[serde(rename = "x86-64-v3")]
    V3,
    /// Adds AVX-512: F, BW, CD, DQ and VL.
    #[serde(rename = "x86-64-v4")]
    V4,
}

impl CpuLevel {
    /// The flags each level adds over the one below it.
    ///
    /// Lowercase, matching how both Linux and QEMU spell them, so a comparison
    /// never turns into a spelling argument.
    const fn added_flags(self) -> &'static [&'static str] {
        match self {
            CpuLevel::V1 => &[],
            CpuLevel::V2 => &[
                "cx16", "lahf_lm", "popcnt", "sse3", "sse4_1", "sse4_2", "ssse3",
            ],
            CpuLevel::V3 => &[
                "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe",
            ],
            CpuLevel::V4 => &["avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"],
        }
    }

    /// Every level from v1 up to and including this one.
    pub const fn up_to(self) -> &'static [CpuLevel] {
        match self {
            CpuLevel::V1 => &[CpuLevel::V1],
            CpuLevel::V2 => &[CpuLevel::V1, CpuLevel::V2],
            CpuLevel::V3 => &[CpuLevel::V1, CpuLevel::V2, CpuLevel::V3],
            CpuLevel::V4 => &[CpuLevel::V1, CpuLevel::V2, CpuLevel::V3, CpuLevel::V4],
        }
    }

    /// The highest level a flag set satisfies.
    ///
    /// Strictly cumulative: a machine with every AVX-512 flag but missing
    /// `sse4_2` is v1, not v4. That is not a hypothetical — it is what a
    /// carelessly masked CPU model looks like, and calling it v4 would let it
    /// through a check it should fail.
    pub fn of(flags: &BTreeSet<String>) -> CpuLevel {
        let mut level = CpuLevel::V1;
        for candidate in [CpuLevel::V2, CpuLevel::V3, CpuLevel::V4] {
            if candidate
                .added_flags()
                .iter()
                .all(|f| flags.contains(*f))
            {
                level = candidate;
            } else {
                break;
            }
        }
        level
    }

    /// Every flag this level implies, cumulatively.
    ///
    /// This is what a node declares it presents under a baseline. Its exact
    /// membership matters less than its *consistency*: every node on the same
    /// baseline computes the same set from the same code, so two of them
    /// compare equal — which is the question a migration actually asks.
    pub fn flags(self) -> BTreeSet<String> {
        self.up_to()
            .iter()
            .flat_map(|l| l.added_flags().iter().map(|f| f.to_string()))
            .collect()
    }

    /// How this level is written for a person: `x86-64-v3`.
    ///
    /// Also, and not by coincidence, the name QEMU gives the matching CPU
    /// model. The coarse currency and the thing passed to `-cpu` are the same
    /// string, so a baseline needs no lookup table to become a command line.
    pub fn as_str(self) -> &'static str {
        match self {
            CpuLevel::V1 => "x86-64-v1",
            CpuLevel::V2 => "x86-64-v2",
            CpuLevel::V3 => "x86-64-v3",
            CpuLevel::V4 => "x86-64-v4",
        }
    }
}

impl std::fmt::Display for CpuLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a node reports about its own processor.
///
/// Observation only. The node states facts; every decision made from them is
/// made elsewhere, by a pure function, from the whole fleet's worth of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCpu {
    /// `x86_64` or `aarch64`. Carried rather than assumed because it is the
    /// one difference no baseline can ever bridge.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    /// `GenuineIntel`, `AuthenticAMD`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    /// The brand string, for a human looking at a mixed cell.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_name: String,
    #[serde(default)]
    pub family: u32,
    #[serde(default)]
    pub model: u32,
    #[serde(default)]
    pub stepping: u32,
    /// The compatibility-relevant flag set. The precise currency: this is what
    /// a refusal names, because "incompatible CPU" is not actionable and
    /// "missing avx512f, avx512dq" is.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub flags: BTreeSet<String>,
    /// What guests started on this node are given: `host`, or a declared
    /// baseline model.
    ///
    /// Distinct from what the silicon has, and the distinction is the whole
    /// point of a baseline: a v4 machine presenting `Skylake-Server-v4` offers
    /// guests less than it holds, on purpose, so that its guests can move to a
    /// machine that holds less.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub presents: String,
    /// The flags a guest started here would actually see.
    ///
    /// Equal to [`Self::flags`] when `presents` is `host`. Smaller under a
    /// baseline. This — not `flags` — is what decides who may exchange guests
    /// with whom; `flags` decides what a baseline *could* be.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub presented_flags: BTreeSet<String>,
    /// Whether this node's VMM can present a CPU other than the host's.
    ///
    /// Reported rather than inferred from the VMM's name: the answer depends
    /// on the architecture as well as the VMM, and it is expected to change
    /// for Cloud Hypervisor without this platform changing.
    #[serde(default)]
    pub can_mask: bool,
}

impl NodeCpu {
    /// The highest psABI level this processor satisfies.
    ///
    /// `None` off x86, where the levels are not defined and inventing an
    /// answer would be worse than having none.
    pub fn level(&self) -> Option<CpuLevel> {
        (self.arch == "x86_64").then(|| CpuLevel::of(&self.flags))
    }

    /// The level of what this node *presents*, which is what a guest gets.
    pub fn presented_level(&self) -> Option<CpuLevel> {
        (self.arch == "x86_64").then(|| CpuLevel::of(&self.presented_flags))
    }

    /// Whether two nodes offer guests the same machine.
    ///
    /// Compares what they *present*, not what they hold: a v4 host and a v2
    /// host both presenting `Skylake-Server-v4` are interchangeable, and two
    /// identical v4 hosts presenting `host` are too. Brand strings are not
    /// consulted — the guest's view is the only one that decides whether it
    /// keeps running.
    pub fn indistinguishable(&self, other: &NodeCpu) -> bool {
        self.arch == other.arch && self.presented_flags == other.presented_flags
    }
}

/// What a guest was actually given, recorded when it started.
///
/// Written by the one writer that can know it — the agent that launched the
/// VMM — and never re-derived. See the module invariant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestCpu {
    /// The model presented, as it was asked for: `host`, or a named model.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    /// What the guest can see. The set a destination must be able to cover.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub flags: BTreeSet<String>,
}

impl GuestCpu {
    pub fn level(&self) -> Option<CpuLevel> {
        (self.arch == "x86_64").then(|| CpuLevel::of(&self.flags))
    }
}

/// How what a destination presents differs from what a guest is running with.
///
/// Returns `(missing, extra)`: flags the guest has and the destination would
/// not present, and flags the destination would present that the guest does
/// not have.
///
/// **Both halves are disqualifying, and that surprises people.** A running
/// guest's CPU state is restored onto the destination, so the destination must
/// present the *same* processor — not a bigger one. A guest cannot be handed
/// extra features mid-flight: it has already read CPUID, and software inside
/// it has already decided what it may execute. This is why VMware and oVirt
/// require identical CPUs or an explicitly declared model, and it is the whole
/// reason a baseline is worth declaring.
pub fn difference_for(guest: &GuestCpu, destination: &NodeCpu) -> (Vec<String>, Vec<String>) {
    if guest.arch != destination.arch {
        // Not a flag problem, and not expressible as one. The caller
        // distinguishes this case before asking; returning the guest's whole
        // flag set here would be technically true and completely useless.
        return (Vec::new(), Vec::new());
    }
    (
        guest
            .flags
            .difference(&destination.presented_flags)
            .cloned()
            .collect(),
        destination
            .presented_flags
            .difference(&guest.flags)
            .cloned()
            .collect(),
    )
}

/// Why a guest cannot move to a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpuMismatch {
    /// Different instruction set architectures. Never bridgeable, by anything.
    DifferentArch { guest: String, node: String },
    /// The node's VMM cannot present anything but the host CPU, and the host
    /// is not the machine this guest booted on.
    ///
    /// Separate from [`CpuMismatch::MissingFeatures`] on purpose: "three flags
    /// short" invites a baseline, and "this VMM can never mask" tells the
    /// operator that no amount of configuration will help. Collapsing them
    /// leaves somebody retrying a thing that cannot work.
    CannotMask { node_model: String },
    /// The node would present a different processor than the guest is running
    /// with. Either half is disqualifying — see [`difference_for`].
    NotIdentical {
        missing: Vec<String>,
        extra: Vec<String>,
    },
}

impl std::fmt::Display for CpuMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CpuMismatch::DifferentArch { guest, node } => {
                write!(f, "the guest is {guest} and the node is {node}")
            }
            CpuMismatch::CannotMask { node_model } => write!(
                f,
                "this node's VMM cannot present a CPU other than its own ({node_model}), \
                 so only an identical machine can take this guest"
            ),
            // Says what to *do*, because "different CPU" leaves an operator
            // with a running guest and no next step. Declaring a baseline both
            // machines can present is the actual remedy, and it is the one
            // thing the platform can offer that hardware cannot.
            CpuMismatch::NotIdentical { missing, extra } => {
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("lacks {}", missing.join(", ")));
                }
                if !extra.is_empty() {
                    parts.push(format!("adds {}", extra.join(", ")));
                }
                write!(
                    f,
                    "it presents a different cpu ({}); a running guest cannot change cpu, \
                     so declare a baseline both nodes can present",
                    parts.join("; ")
                )
            }
        }
    }
}

/// Whether a **running** guest may be taken over by a node.
///
/// Asked only about guests that are running, which is why it demands an exact
/// match rather than a superset: a guest that has not started yet will be given
/// whatever the node presents, and has no CPU to preserve. That is also why
/// [`crate::resources::InstanceStatus::cpu`] is cleared when a guest stops —
/// the field describes a running machine, and a stale one would over-constrain
/// the next placement.
///
/// Fails closed: a node that has reported nothing about its CPU cannot be
/// shown to match, so it does not. An empty report is the state of a node
/// running an agent too old to say, and guessing on its behalf is how a guest
/// ends up somewhere it cannot execute.
pub fn may_run_on(guest: &GuestCpu, node: &NodeCpu) -> Result<(), CpuMismatch> {
    if guest.arch != node.arch {
        return Err(CpuMismatch::DifferentArch {
            guest: guest.arch.clone(),
            node: node.arch.clone(),
        });
    }
    let (missing, extra) = difference_for(guest, node);
    if missing.is_empty() && extra.is_empty() {
        return Ok(());
    }
    // A node that cannot mask can never be made to present anything else, so
    // the remedy in `NotIdentical` — declare a baseline — does not apply to
    // it. Handing an operator advice that cannot work is worse than handing
    // them none.
    if !node.can_mask {
        return Err(CpuMismatch::CannotMask {
            node_model: if node.model_name.is_empty() {
                node.vendor.clone()
            } else {
                node.model_name.clone()
            },
        });
    }
    Err(CpuMismatch::NotIdentical { missing, extra })
}

/// Whether a node could present a baseline, and what it would be short of.
///
/// Answered **before** the baseline is declared, because the alternative is
/// finding out at the moment a guest fails to start: `enforce` makes QEMU
/// refuse rather than quietly present less, which is right, but "your guests
/// stopped booting" is a bad way to learn that a machine is a generation too
/// old for the level somebody typed.
///
/// A node that has reported no flags is short of everything, which is the
/// honest answer and keeps this from waving through an agent too old to say.
pub fn can_present(node: &NodeCpu, level: CpuLevel) -> Result<(), Vec<String>> {
    let missing: Vec<String> = level
        .flags()
        .difference(&node.flags)
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A guest still running with a CPU its node no longer hands out.
///
/// The ordinary, expected consequence of changing a baseline over a live
/// fleet — not a fault. It resolves itself the next time the guest stops and
/// starts, because its CPU is recorded at boot. What matters is that it is
/// *visible* in the meantime: while a guest is in this state it can only move
/// to a node still presenting the old CPU, and after a fleet-wide change that
/// may be nowhere at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAdoption {
    pub instance: String,
    pub node: String,
    /// What it is running with.
    pub running: Option<CpuLevel>,
    /// What it would get if it were restarted now.
    pub would_get: Option<CpuLevel>,
}

/// Guests whose CPU no longer matches what their node presents.
///
/// Pure, and computed on read like everything else here. Takes `(instance,
/// node, GuestCpu)` triples rather than the resource types so this module
/// keeps no dependency on how they are stored.
pub fn pending_adoption(
    guests: &[(String, String, GuestCpu)],
    nodes: &[NodeEntry],
) -> Vec<PendingAdoption> {
    let mut pending: Vec<PendingAdoption> = guests
        .iter()
        .filter_map(|(instance, node, guest)| {
            let entry = nodes.iter().find(|n| &n.node == node)?;
            // Compared against what the node presents *now*. Equal means the
            // guest is already on the current baseline, whether or not it was
            // started before the change — a baseline that happens to match
            // what a guest already had is not something to nag about.
            if guest.flags == entry.cpu.presented_flags && guest.arch == entry.cpu.arch {
                return None;
            }
            Some(PendingAdoption {
                instance: instance.clone(),
                node: node.clone(),
                running: guest.level(),
                would_get: entry.cpu.presented_level(),
            })
        })
        .collect();
    pending.sort_by(|a, b| a.instance.cmp(&b.instance));
    pending
}

/// A set of nodes that can exchange guests.
///
/// Computed, never stored. A stored grouping drifts from the fleet the moment
/// a node is replaced; a computed one cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// Node ids, sorted, so the same fleet always yields the same answer.
    pub nodes: Vec<String>,
    pub arch: String,
    /// The level every node in the domain satisfies.
    pub level: Option<CpuLevel>,
    /// Whether the members can be baselined together at all.
    pub can_mask: bool,
}

/// One node's CPU, with the id it belongs to. The input to every function
/// below, kept separate from the resource types so this module stays pure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEntry {
    pub node: String,
    pub cpu: NodeCpu,
}

/// Group nodes into the sets that can exchange guests *today*.
///
/// Two nodes share a domain when a guest booted on either could run on the
/// other. With no baseline declared each node presents its own CPU, so that
/// reduces to: same architecture, same flags. Declaring a baseline is what
/// merges domains, and it does so by changing what the nodes present — which
/// is why this function takes the presented CPU, not the raw host one.
pub fn migration_domains(nodes: &[NodeEntry]) -> Vec<Domain> {
    let mut domains: Vec<Domain> = Vec::new();
    let mut members: Vec<Vec<&NodeEntry>> = Vec::new();

    for entry in nodes {
        match members
            .iter_mut()
            .find(|group| group[0].cpu.indistinguishable(&entry.cpu))
        {
            Some(group) => group.push(entry),
            None => members.push(vec![entry]),
        }
    }

    for group in members {
        let mut ids: Vec<String> = group.iter().map(|e| e.node.clone()).collect();
        ids.sort();
        domains.push(Domain {
            nodes: ids,
            arch: group[0].cpu.arch.clone(),
            level: group[0].cpu.level(),
            // A domain can be baselined only if every member's VMM can mask.
            // One Cloud Hypervisor node in the set is enough to make the whole
            // set unbaselineable, which is exactly the fact an operator needs.
            can_mask: group.iter().all(|e| e.cpu.can_mask),
        });
    }
    domains.sort_by(|a, b| a.nodes.cmp(&b.nodes));
    domains
}

/// Why a set of nodes cannot be brought into one domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CannotMerge {
    /// At least one node's VMM has no CPU model to mask with.
    VmmCannotMask { nodes: Vec<String> },
    /// Masking down would land below the level some node's guests need.
    WouldDropBelow { level: CpuLevel },
}

/// What the platform suggests an operator do about a mixed fleet.
///
/// Every variant is a complete thought: what would change, and what it costs.
/// A recommendation that names only the benefit arrives wearing the
/// platform's authority and is worse than none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Advice {
    /// The fleet is already one domain. Nothing to do, said out loud so an
    /// operator can stop looking.
    /// One domain, and these are the machines in it.
    ///
    /// The names rather than a count, and that is not decoration: every other
    /// piece of advice here carries `nodes` as the machines it is about, and
    /// one variant carrying a number under the same name is a field that means
    /// two things — which is how a screen comes to print "3" where it meant to
    /// list three machines. Found by the recorded-shape test, which read the
    /// number where the fixture had a list.
    AlreadyUniform { nodes: Vec<String>, level: Option<CpuLevel> },
    /// These domains could be merged by declaring a baseline.
    BaselineWouldMerge {
        /// The nodes that would end up together.
        nodes: Vec<String>,
        /// The level the merged domain would satisfy.
        level: CpuLevel,
        /// The flags each node's future guests would give up. Per node,
        /// because "somebody loses something" is not a decision anybody can
        /// make. A node that loses nothing is not listed.
        features_lost: Vec<(String, Vec<String>)>,
    },
    /// These nodes cannot be merged, and here is why.
    CannotMerge {
        nodes: Vec<String>,
        reason: CannotMerge,
    },
    /// Architectures never merge. Listed separately because it is the one
    /// split no configuration can address, and grouping it with the others
    /// would imply otherwise.
    SplitByArch { groups: Vec<(String, Vec<String>)> },
    /// A node presents something its neighbours do not, and could join them.
    ///
    /// The case a machine added after the fleet was baselined lands in. It is
    /// not a fault and nothing is broken — the node simply forms its own
    /// migration domain until somebody says otherwise. Said out loud because
    /// the alternative to noticing it here is noticing it the first time a
    /// migration onto that node is refused.
    NodeOutsideTheAggregate {
        node: String,
        presents: String,
        /// What its neighbours present, and how many of them do.
        aggregate: String,
        aggregate_nodes: usize,
        /// Empty when it could join. Otherwise what its silicon is short of,
        /// which means it can never join this aggregate and the honest
        /// remedy is a second one.
        missing: Vec<String>,
    },
    /// A baseline is declared and some guests have not adopted it yet.
    ///
    /// The expected state after changing a baseline over a live fleet, and it
    /// clears itself: each guest adopts on its next stop-and-start. Reported
    /// so an operator can see the change land instead of wondering whether it
    /// did, and because until a guest adopts, it can only move to a node
    /// still presenting the CPU it booted with.
    AdoptionPending {
        /// Instance names, sorted.
        guests: Vec<String>,
        /// What they will get once restarted.
        target: Option<CpuLevel>,
    },
}

/// Look at a fleet and say what could be done about its CPU spread.
///
/// Pure, and computed on read. The recommendation is never stored, because a
/// stored recommendation outlives the fleet that justified it.
pub fn advise(nodes: &[NodeEntry], guests: &[(String, String, GuestCpu)]) -> Vec<Advice> {
    let known: Vec<&NodeEntry> = nodes
        .iter()
        .filter(|e| !e.cpu.arch.is_empty())
        .collect();
    if known.is_empty() {
        return Vec::new();
    }

    let mut advice = Vec::new();

    // Architecture first: it is the split that cannot be bridged, and every
    // suggestion below only makes sense inside one architecture.
    let mut arches: Vec<String> = known.iter().map(|e| e.cpu.arch.clone()).collect();
    arches.sort();
    arches.dedup();
    if arches.len() > 1 {
        let groups = arches
            .iter()
            .map(|arch| {
                let mut ids: Vec<String> = known
                    .iter()
                    .filter(|e| &e.cpu.arch == arch)
                    .map(|e| e.node.clone())
                    .collect();
                ids.sort();
                (arch.clone(), ids)
            })
            .collect();
        advice.push(Advice::SplitByArch { groups });
    }

    for arch in &arches {
        let here: Vec<&NodeEntry> = known.iter().copied().filter(|e| &e.cpu.arch == arch).collect();
        let entries: Vec<NodeEntry> = here.iter().map(|e| (*e).clone()).collect();
        let domains = migration_domains(&entries);

        if domains.len() <= 1 {
            if arches.len() == 1 {
                advice.push(Advice::AlreadyUniform {
                    nodes: here.iter().map(|e| e.node.clone()).collect(),
                    level: domains.first().and_then(|d| d.level),
                });
            }
            continue;
        }

        // More than one domain on one architecture: the interesting case.
        let unmaskable: Vec<String> = here
            .iter()
            .filter(|e| !e.cpu.can_mask)
            .map(|e| e.node.clone())
            .collect();

        if !unmaskable.is_empty() {
            let mut nodes: Vec<String> = here.iter().map(|e| e.node.clone()).collect();
            nodes.sort();
            advice.push(Advice::CannotMerge {
                nodes,
                reason: CannotMerge::VmmCannotMask { nodes: unmaskable },
            });
            continue;
        }

        // Every node here can mask, so a baseline is possible. It is the
        // intersection of what they all have — masking only ever removes.
        let mut common: BTreeSet<String> = here[0].cpu.flags.clone();
        for entry in &here[1..] {
            common = common.intersection(&entry.cpu.flags).cloned().collect();
        }
        let level = CpuLevel::of(&common);

        let features_lost: Vec<(String, Vec<String>)> = here
            .iter()
            .filter_map(|e| {
                let lost: Vec<String> = e.cpu.flags.difference(&common).cloned().collect();
                (!lost.is_empty()).then(|| (e.node.clone(), lost))
            })
            .collect();

        let mut ids: Vec<String> = here.iter().map(|e| e.node.clone()).collect();
        ids.sort();
        advice.push(Advice::BaselineWouldMerge {
            nodes: ids,
            level,
            features_lost,
        });
    }

    // A node standing outside what its neighbours present. Reported only when
    // there *is* a majority to stand outside of: two nodes disagreeing is a
    // fleet with two domains, already covered above, not one stray machine.
    let mut presented: std::collections::BTreeMap<String, Vec<&NodeEntry>> =
        std::collections::BTreeMap::new();
    for entry in &known {
        presented
            .entry(entry.cpu.presents.clone())
            .or_default()
            .push(entry);
    }
    if presented.len() > 1 {
        if let Some((aggregate, members)) = presented.iter().max_by_key(|(_, m)| m.len()) {
            if members.len() > 1 {
                let level = parse_level(aggregate);
                for (presents, strays) in &presented {
                    if presents == aggregate {
                        continue;
                    }
                    for stray in strays {
                        advice.push(Advice::NodeOutsideTheAggregate {
                            node: stray.node.clone(),
                            presents: presents.clone(),
                            aggregate: aggregate.clone(),
                            aggregate_nodes: members.len(),
                            // Whether it *could* join, which decides whether
                            // the remedy is "declare the baseline here" or
                            // "this machine needs an aggregate of its own".
                            missing: match level {
                                Some(level) => can_present(&stray.cpu, level).err().unwrap_or_default(),
                                None => Vec::new(),
                            },
                        });
                    }
                }
            }
        }
    }

    // Guests still carrying a CPU their node no longer hands out.
    let pending = pending_adoption(guests, nodes);
    if !pending.is_empty() {
        let target = pending[0].would_get;
        advice.push(Advice::AdoptionPending {
            guests: pending.iter().map(|p| p.instance.clone()).collect(),
            target,
        });
    }

    advice
}

/// A level from the way a node writes what it presents.
///
/// `None` for `host` and for anything unrecognised, both of which mean the
/// same thing here: not a level this build can reason about, so no claim is
/// made about whether another machine could match it.
fn parse_level(presents: &str) -> Option<CpuLevel> {
    match presents {
        "x86-64-v1" => Some(CpuLevel::V1),
        "x86-64-v2" => Some(CpuLevel::V2),
        "x86-64-v3" => Some(CpuLevel::V3),
        "x86-64-v4" => Some(CpuLevel::V4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Every flag up to and including a level, so a test can name a machine by
    /// what it is rather than by 20 strings.
    fn at_level(level: CpuLevel) -> BTreeSet<String> {
        level
            .up_to()
            .iter()
            .flat_map(|l| l.added_flags().iter().map(|f| f.to_string()))
            .collect()
    }

    fn node(id: &str, level: CpuLevel, can_mask: bool) -> NodeEntry {
        NodeEntry {
            node: id.to_string(),
            cpu: NodeCpu {
                arch: "x86_64".into(),
                flags: at_level(level),
                presents: "host".into(),
                presented_flags: at_level(level),
                can_mask,
                ..NodeCpu::default()
            },
        }
    }

    /// A level is spelled the same in JSON as it is on a command line.
    ///
    /// Without the renames serde writes `v3`, and the API would take a value
    /// no operator would type and no `-cpu` would accept.
    #[test]
    fn a_level_is_spelled_the_same_on_the_wire_as_everywhere_else() {
        for level in [CpuLevel::V1, CpuLevel::V2, CpuLevel::V3, CpuLevel::V4] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{}\"", level.as_str()));
            let back: CpuLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn a_level_is_cumulative_rather_than_a_best_match() {
        assert_eq!(CpuLevel::of(&at_level(CpuLevel::V1)), CpuLevel::V1);
        assert_eq!(CpuLevel::of(&at_level(CpuLevel::V3)), CpuLevel::V3);

        // Every AVX-512 flag, but nothing below it. A "highest matching level"
        // reading calls this v4; it is v1, and calling it v4 would wave it
        // past a check it must fail.
        let broken = flags(&["avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"]);
        assert_eq!(CpuLevel::of(&broken), CpuLevel::V1);
    }

    /// A destination with *more* than the guest is refused too, and this is
    /// the counter-intuitive half of the rule.
    ///
    /// A running guest has already read CPUID and software inside it has
    /// already chosen what to execute. Handing it extra features mid-flight is
    /// not a gift, it is a different machine. The refusal therefore points at
    /// the remedy that does work: a baseline both nodes present.
    #[test]
    fn a_destination_with_more_than_the_guest_is_still_a_different_machine() {
        let guest = GuestCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V2),
            ..GuestCpu::default()
        };
        let bigger = NodeCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V4),
            presents: "host".into(),
            presented_flags: at_level(CpuLevel::V4),
            can_mask: true,
            ..NodeCpu::default()
        };
        let (missing, extra) = difference_for(&guest, &bigger);
        assert!(missing.is_empty(), "the guest needs nothing the node lacks");
        assert!(extra.contains(&"avx2".to_string()), "{extra:?}");
        assert!(matches!(
            may_run_on(&guest, &bigger),
            Err(CpuMismatch::NotIdentical { .. })
        ));
    }

    /// Two machines that differ in silicon but present the same baseline are
    /// interchangeable. This is what declaring a baseline buys, and the test
    /// that proves the feature does anything at all.
    #[test]
    fn a_baseline_makes_unlike_machines_interchangeable() {
        let guest = GuestCpu {
            model: "Skylake-Server-v4".into(),
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V2),
        };
        let big_host_small_promise = NodeCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V4),
            presents: "Skylake-Server-v4".into(),
            presented_flags: at_level(CpuLevel::V2),
            can_mask: true,
            ..NodeCpu::default()
        };
        assert_eq!(may_run_on(&guest, &big_host_small_promise), Ok(()));
    }

    #[test]
    fn a_destination_short_of_the_guest_is_refused_by_name() {
        let guest = GuestCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V3),
            ..GuestCpu::default()
        };
        let smaller = NodeCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V2),
            presents: "host".into(),
            presented_flags: at_level(CpuLevel::V2),
            can_mask: true,
            ..NodeCpu::default()
        };
        let Err(CpuMismatch::NotIdentical { missing, .. }) = may_run_on(&guest, &smaller) else {
            panic!("a v2 node took a v3 guest");
        };
        assert!(missing.contains(&"avx2".to_string()), "{missing:?}");
    }

    #[test]
    fn a_vmm_that_cannot_mask_is_refused_with_that_reason_not_a_flag_list() {
        // The two refusals lead to different actions: one invites a baseline,
        // the other says no configuration will help.
        let guest = GuestCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V3),
            ..GuestCpu::default()
        };
        let ch = NodeCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V2),
            presents: "host".into(),
            presented_flags: at_level(CpuLevel::V2),
            can_mask: false,
            model_name: "AMD EPYC 9654".into(),
            ..NodeCpu::default()
        };
        assert!(matches!(
            may_run_on(&guest, &ch),
            Err(CpuMismatch::CannotMask { .. })
        ));
    }

    #[test]
    fn a_node_that_has_reported_nothing_is_not_assumed_compatible() {
        // An agent too old to report its CPU must not be treated as a match.
        let guest = GuestCpu {
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V2),
            ..GuestCpu::default()
        };
        assert!(may_run_on(&guest, &NodeCpu::default()).is_err());
    }

    #[test]
    fn architectures_never_match_however_the_flags_line_up() {
        let guest = GuestCpu {
            arch: "x86_64".into(),
            flags: BTreeSet::new(),
            ..GuestCpu::default()
        };
        let arm = NodeCpu {
            arch: "aarch64".into(),
            can_mask: false,
            ..NodeCpu::default()
        };
        assert!(matches!(
            may_run_on(&guest, &arm),
            Err(CpuMismatch::DifferentArch { .. })
        ));
    }

    #[test]
    fn identical_machines_form_one_domain_and_different_ones_do_not() {
        let fleet = vec![
            node("a", CpuLevel::V3, true),
            node("b", CpuLevel::V3, true),
            node("c", CpuLevel::V2, true),
        ];
        let domains = migration_domains(&fleet);
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].nodes, ["a", "b"]);
        assert_eq!(domains[0].level, Some(CpuLevel::V3));
        assert_eq!(domains[1].nodes, ["c"]);
    }

    #[test]
    fn one_unmaskable_node_makes_its_whole_domain_unbaselineable() {
        // The property belongs to the set, not the node: a domain is only
        // baselineable if every member can be told what to present.
        let fleet = vec![
            node("a", CpuLevel::V3, true),
            node("b", CpuLevel::V3, false),
        ];
        let domains = migration_domains(&fleet);
        assert_eq!(domains.len(), 1, "identical CPUs are one domain either way");
        assert!(!domains[0].can_mask);
    }

    #[test]
    fn a_mixed_maskable_fleet_is_advised_to_baseline_and_told_the_price() {
        let fleet = vec![
            node("a", CpuLevel::V4, true),
            node("b", CpuLevel::V2, true),
        ];
        let advice = advise(&fleet, &[]);
        let Some(Advice::BaselineWouldMerge {
            nodes,
            level,
            features_lost,
        }) = advice.first()
        else {
            panic!("no baseline was suggested for a fleet that can be baselined: {advice:?}");
        };
        assert_eq!(nodes, &["a", "b"]);
        assert_eq!(*level, CpuLevel::V2, "the baseline is the intersection");

        // The price is named, per node, and only for nodes that pay it.
        assert_eq!(features_lost.len(), 1, "{features_lost:?}");
        assert_eq!(features_lost[0].0, "a");
        assert!(features_lost[0].1.contains(&"avx2".to_string()));
    }

    #[test]
    fn a_mixed_fleet_with_an_unmaskable_node_is_told_it_cannot_be_merged() {
        // The Cloud Hypervisor case. The advice must not suggest a baseline
        // that cannot be applied.
        let fleet = vec![
            node("a", CpuLevel::V4, false),
            node("b", CpuLevel::V2, false),
        ];
        let advice = advise(&fleet, &[]);
        let Some(Advice::CannotMerge {
            reason: CannotMerge::VmmCannotMask { nodes },
            ..
        }) = advice.first()
        else {
            panic!("a baseline was suggested for nodes that cannot mask: {advice:?}");
        };
        assert_eq!(nodes, &["a", "b"]);
    }

    #[test]
    fn a_uniform_fleet_is_told_so_rather_than_left_silent() {
        let fleet = vec![
            node("a", CpuLevel::V3, true),
            node("b", CpuLevel::V3, true),
        ];
        assert_eq!(
            advise(&fleet, &[]),
            vec![Advice::AlreadyUniform {
                nodes: vec!["a".into(), "b".into()],
                level: Some(CpuLevel::V3),
            }]
        );
    }

    #[test]
    fn mixed_architectures_are_reported_as_the_split_nothing_can_bridge() {
        let mut arm = node("arm-1", CpuLevel::V1, false);
        arm.cpu.arch = "aarch64".into();
        arm.cpu.flags = BTreeSet::new();
        let fleet = vec![node("x86-1", CpuLevel::V3, true), arm];

        let advice = advise(&fleet, &[]);
        let Some(Advice::SplitByArch { groups }) = advice.first() else {
            panic!("a mixed-architecture fleet was not reported as such: {advice:?}");
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "aarch64");
        assert_eq!(groups[1].0, "x86_64");
    }

    #[test]
    fn nodes_that_have_not_reported_a_cpu_are_left_out_rather_than_guessed_at() {
        // A node running an agent too old to report must not be silently
        // folded into a domain it may not belong to.
        let fleet = vec![
            node("a", CpuLevel::V3, true),
            NodeEntry {
                node: "old".into(),
                cpu: NodeCpu::default(),
            },
        ];
        assert_eq!(
            advise(&fleet, &[]),
            vec![Advice::AlreadyUniform {
                // The one that reported, by name. The node running an agent
                // too old to say anything is not in the domain and not
                // counted into it.
                nodes: vec!["a".into()],
                level: Some(CpuLevel::V3),
            }]
        );
    }
    /// Declaring a baseline a machine cannot reach is refused *before* it is
    /// declared, with the shortfall named.
    ///
    /// `enforce` would catch it later — QEMU would refuse to start the guest —
    /// but "your guests stopped booting" is a bad way to learn that a machine
    /// is a generation too old for the level somebody typed.
    #[test]
    fn a_baseline_a_node_cannot_reach_is_refused_with_the_shortfall_named() {
        let v2 = node("a", CpuLevel::V2, true).cpu;
        assert_eq!(can_present(&v2, CpuLevel::V2), Ok(()));

        let Err(missing) = can_present(&v2, CpuLevel::V3) else {
            panic!("a v2 machine was cleared to present v3");
        };
        assert!(missing.contains(&"avx2".to_string()), "{missing:?}");

        // A node that has reported nothing is short of everything, rather than
        // waved through for having no contradicting evidence.
        assert!(can_present(&NodeCpu::default(), CpuLevel::V2).is_err());
    }

    /// The lifecycle the whole feature exists for: a third generation arrives,
    /// the fleet is re-baselined lower, and guests adopt it as they restart.
    #[test]
    fn guests_adopt_a_new_baseline_as_they_restart_and_are_listed_until_they_do() {
        // Two nodes now presenting v2 — the fleet was just re-baselined down
        // to admit a weaker third machine.
        let mut a = node("a", CpuLevel::V3, true);
        a.cpu.presents = "x86-64-v2".into();
        a.cpu.presented_flags = CpuLevel::V2.flags();
        let mut b = node("b", CpuLevel::V3, true);
        b.cpu.presents = "x86-64-v2".into();
        b.cpu.presented_flags = CpuLevel::V2.flags();
        let fleet = vec![a, b];

        let old = GuestCpu {
            model: "host".into(),
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V3),
        };
        let adopted = GuestCpu {
            model: "x86-64-v2".into(),
            arch: "x86_64".into(),
            flags: CpuLevel::V2.flags(),
        };
        let guests = vec![
            ("i-old".to_string(), "a".to_string(), old),
            ("i-new".to_string(), "b".to_string(), adopted),
        ];

        // Only the one that has not restarted is listed, and it is told what
        // it would get.
        let pending = pending_adoption(&guests, &fleet);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].instance, "i-old");
        assert_eq!(pending[0].running, Some(CpuLevel::V3));
        assert_eq!(pending[0].would_get, Some(CpuLevel::V2));

        // And the advice says so, rather than leaving an operator to wonder
        // whether the change landed.
        let advice = advise(&fleet, &guests);
        assert!(
            advice.iter().any(|a| matches!(
                a,
                Advice::AdoptionPending { guests, .. } if guests == &["i-old".to_string()]
            )),
            "{advice:?}"
        );
    }

    /// A machine added after the fleet was baselined stands outside it, and is
    /// told whether it could join.
    ///
    /// The scenario is "a third generation arrives". Two answers are possible
    /// and they lead to opposite actions: declare the baseline on it, or give
    /// it an aggregate of its own because its silicon cannot reach the others.
    #[test]
    fn a_node_added_after_the_baseline_is_told_whether_it_can_join() {
        let baselined = |id: &str, silicon: CpuLevel| {
            let mut n = node(id, silicon, true);
            n.cpu.presents = "x86-64-v3".into();
            n.cpu.presented_flags = CpuLevel::V3.flags();
            n
        };

        // A newcomer whose silicon is plenty: it just has not been told.
        let fleet = vec![
            baselined("a", CpuLevel::V4),
            baselined("b", CpuLevel::V4),
            node("new", CpuLevel::V4, true),
        ];
        let advice = advise(&fleet, &[]);
        let outside: Vec<_> = advice
            .iter()
            .filter_map(|a| match a {
                Advice::NodeOutsideTheAggregate { node, missing, .. } => Some((node, missing)),
                _ => None,
            })
            .collect();
        assert_eq!(outside.len(), 1, "{advice:?}");
        assert_eq!(outside[0].0, "new");
        assert!(
            outside[0].1.is_empty(),
            "a machine that can reach the aggregate was told it cannot: {:?}",
            outside[0].1
        );

        // A newcomer that is genuinely a generation short: it can never join,
        // and the advice must not pretend otherwise.
        let fleet = vec![
            baselined("a", CpuLevel::V4),
            baselined("b", CpuLevel::V4),
            node("weak", CpuLevel::V2, true),
        ];
        let advice = advise(&fleet, &[]);
        let Some(Advice::NodeOutsideTheAggregate { node, missing, .. }) = advice
            .iter()
            .find(|a| matches!(a, Advice::NodeOutsideTheAggregate { .. }))
        else {
            panic!("a stray node went unreported: {advice:?}");
        };
        assert_eq!(node, "weak");
        assert!(
            missing.contains(&"avx2".to_string()),
            "the shortfall that makes joining impossible was not named: {missing:?}"
        );
    }

    /// A guest whose node happens to present exactly what it already had is
    /// not nagged about adopting anything.
    #[test]
    fn a_guest_already_matching_its_node_is_not_listed_as_pending() {
        let fleet = vec![node("a", CpuLevel::V3, true)];
        let guest = GuestCpu {
            model: "host".into(),
            arch: "x86_64".into(),
            flags: at_level(CpuLevel::V3),
        };
        let guests = vec![("i1".to_string(), "a".to_string(), guest)];
        assert!(pending_adoption(&guests, &fleet).is_empty());
    }

}
