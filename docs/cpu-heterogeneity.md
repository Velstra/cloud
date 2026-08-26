# CPU models, mixed fleets, and what may migrate where

Written 2026-08-24, against the code as it stands.

The question this answers: a cell has nodes with different CPUs. What does the
platform do about it — and what does it tell the operator to do about it?

## 0. Where we start, honestly

Three facts about the code today, all verified rather than assumed:

- `qemu_args` (`velstra-cloud-nodeagent/src/qemu.rs:606`) passes **no `-cpu` at
  all**. QEMU's x86_64 default is `qemu64`, an Opteron-G1-era model with no
  SSSE3, SSE4.1, SSE4.2 or POPCNT. That is below `x86-64-v2`, which RHEL 9,
  CentOS Stream 9 and a growing list of distributions require. Those images do
  not boot on a Velstra QEMU node. This is a bug, not a design position, and it
  is fixed as part of this work.
- The Cloud Hypervisor backend passes `--cpus boot=N` and nothing else, which
  is all Cloud Hypervisor has to offer (§2).
- `migration::Refusal` has a good, specific list — `DestinationTooSmall`,
  `DestinationLacksImage`, `VersionsTooFarApart` — and **nothing about the
  CPU**. A node does not report what CPU it has, so nothing could.

So: the platform currently gives guests a 2006 CPU on one backend and the raw
host CPU on the other, and will happily migrate a guest between two machines
that share no instruction set.

## 1. The two ideas are one idea

An operator looking at a mixed fleet has two instincts, and both are right:

- **"Baseline them"** — make several different machines present the *same* CPU
  to guests, so guests can move between them.
- **"Group them"** — accept that some machines will never exchange guests, and
  make the groups explicit so nobody is surprised.

These are not alternatives. A **migration domain** is what you *have*: the set
of nodes that can exchange a given guest right now. A **baseline** is what you
*declare*: a promise about the CPU presented to guests, which merges several
would-be domains into one.

The platform therefore does three things, in this order:

1. **Computes the domains** and shows them. Always. This is the truth about the
   fleet and it is never a stored value that can drift.
2. **Accepts a declared baseline**, which changes the domains.
3. **Recommends baselines** when heterogeneity splits a fleet that need not be
   split — and says what each one would cost.

## 2. Cloud Hypervisor cannot be baselined, and the design must say so

QEMU has a CPU model system: named models (`Skylake-Server-v4`), feature flags,
`enforce` to fail rather than silently drop, and QMP verbs
(`query-cpu-model-expansion`, `-comparison`, `-baseline`) that compute
compatibility properly.

**Cloud Hypervisor has none of this.** It derives the guest CPUID from the
host's `KVM_GET_SUPPORTED_CPUID` with a small fixed set of patches. `--cpus`
takes `boot`, `max`, `topology`, `kvm_hyperv`, `max_phys_bits` and a tiny
`features` allowlist. There is no model to name and no mask to apply. It is
permanently the equivalent of `-cpu host`.

Two consequences that no amount of platform cleverness removes:

- Two Cloud Hypervisor nodes are in the same migration domain **only if their
  CPU signature is identical**. There is no way to bring them together.
- Cloud Hypervisor has no emulation at all — KVM or MSHV, never TCG. So a
  different *architecture* is not slow, it is impossible.

This is why the design carries a per-VMM capability rather than an assumption:

```rust
struct VmmCapabilities { can_mask_cpu: bool }   // qemu: true, cloud-hypervisor: false
```

That flag is the seam. Cloud Hypervisor has an open upstream discussion about
CPU model support; if it lands, the flag flips, the domains merge, and nothing
else in this design changes. Designing as though every VMM can mask would have
to be unpicked later; designing around one that cannot ages badly in the other
direction. A capability ages in neither.

The same flag covers aarch64, where neither VMM has CPU models: on that
architecture both report `can_mask_cpu: false` and the identical-or-nothing
rule applies to both backends.

## 3. The invariant that makes the rest correct

**A guest's CPU is a fact recorded when it booted, not a property derived from
the node it sits on.**

This is the part that is easy to get wrong and expensive to get wrong. If
migration compatibility is computed from "what could the destination present",
then declaring a baseline retroactively changes the answer for guests that are
already running with something else — and the platform will cheerfully move a
guest onto a host that cannot give it the instructions it has been using.

So: `InstanceStatus.cpu` is written by the agent when it starts the guest, by
the one writer that can know it, and is never re-derived. Compatibility is
always asked as:

> can the destination present **exactly what this guest already sees**?

A baseline declared today governs guests started after it. Guests already
running keep what they booted with until they are restarted, and the console
says so rather than quietly implying otherwise — the honesty rule the console
design already applies to fields applies here to CPUs.

## 4. Two currencies, used for different jobs

- **Coarse — the psABI levels `x86-64-v1..v4`.** Vendor-neutral, stable, and
  the thing distributions actually state as a requirement. This is what the
  console shows, what an instance may demand
  (`placement_policy.min_cpu_level`), and what a recommendation is phrased in.
- **Precise — the flag set.** This is what a refusal names: *"destination
  lacks avx512f, avx512dq"* is actionable; *"incompatible CPU"* is not.

Never enforce on the coarse currency — two v3 machines can still differ in ways
that matter. Never lead the UX with the precise one — nobody reads 200 flags.

## 5. What a node reports

Observation only, in the pattern the disk and Ceph reporting already follow:
the node states facts, it decides nothing.

```rust
struct NodeCpu {
    arch: String,                        // x86_64 | aarch64
    vendor: String,                      // GenuineIntel
    model_name: String,                  // Intel(R) Xeon(R) Gold 6248R
    family: u32, model: u32, stepping: u32,
    flags: BTreeSet<String>,             // what the silicon has
    presents: String,                    // "host", or "x86-64-v3"
    presented_flags: BTreeSet<String>,   // what a guest here actually sees
    can_mask: bool,                      // from the VMM, not guessed
}
```

`flags` and `presented_flags` are separate on purpose, and the separation is
the baseline: a v4 machine presenting `x86-64-v3` holds more than it offers.
`flags` decides what a baseline *could* be; `presented_flags` decides who may
exchange guests with whom. `level()` and `presented_level()` derive the coarse
currency from each.

Read from `/proc/cpuinfo`, not from a CPUID crate. The kernel has already
decoded CPUID, applied errata masking, and — the part that matters — hidden
what it will not let KVM expose. A guest can only ever see what the kernel
permits, so the kernel's view is the one that predicts whether a guest keeps
running.

On a machine with asymmetric cores (Intel's P/E split, ARM big.LITTLE) the
**intersection** across cores is reported. A guest is pinned nowhere, so
promising it `avx2` because core 0 has it produces a fault on some later core
with no discoverable cause.

## 6. Enforcement: fail closed, and name the reason

**At boot.** A baseline is passed to QEMU as `-cpu <model>,enforce`. Without
`enforce`, QEMU silently drops features the host cannot provide and the guest
gets less than the platform promised — which is exactly the class of quiet
degradation this platform refuses everywhere else. With it, the guest does not
start and the node says which features were missing.

**At placement.** New `Rejected` variants, joining the existing explain chain:

```rust
Rejected::CpuLevelTooLow { has: Option<CpuLevel>, want: CpuLevel }
Rejected::CpuIncompatible { why: CpuMismatch }
```

**At migration.** New `Refusal` variants, joining the existing list:

```rust
Refusal::DestinationCpuIncompatible { node: String, why: CpuMismatch }
Refusal::GuestCpuUnknown { node: String }
```

Both carry the mismatch rather than a flattened sentence, because
`CpuMismatch` distinguishes three facts that lead to three different actions:

```rust
enum CpuMismatch {
    DifferentArch { .. },                       // nothing can bridge this
    CannotMask { node_model: String },          // no baseline will ever help
    NotIdentical { missing: Vec<String>, extra: Vec<String> },
}
```

`NotIdentical` carries `extra` as well as `missing`, and that is the
counter-intuitive half of the rule: **a destination with *more* than the guest
is refused too.** A running guest's CPU state is restored onto the
destination, which must therefore present the *same* processor — the guest has
already read CPUID and software inside it has already chosen what to execute.
This is why VMware and oVirt require identical CPUs or an explicit model, and
it is the reason a baseline is worth declaring at all. The message says so:

> *it presents a different cpu (adds avx2, bmi1); a running guest cannot change
> cpu, so declare a baseline both nodes can present*

`GuestCpuUnknown` is separate because "we know this will break" and "we cannot
know" call for different actions — the second is fixed by restarting the guest
under an agent new enough to report, not by finding another node. It has one
deliberate exemption: when the two hosts are **indistinguishable**, the move is
provably safe without knowing what the guest was given, because whatever the
source presented out of that machine the destination can present too. Without
that exemption every guest started before this feature existed would be
stranded until restarted — and avoiding a restart is what migration is for.

## 7. Default behaviour, and why

**With no baseline declared, QEMU nodes get `-cpu host`.**

This fixes the `qemu64` bug, gives guests the machine they paid for, and makes
every differing host its own migration domain. That last part sounds like a
regression and is not: the platform now *shows* the domains and *refuses*
across them by name, instead of pretending and then corrupting a guest. Truth
by default; uniformity is opted into.

Cloud Hypervisor nodes get what they always got, because there is no choice.

## 8. The recommendation

The part an operator actually feels. Computed, never stored:

```rust
fn migration_domains(nodes: &[Node], baselines: &[CpuBaseline]) -> Vec<Domain>
fn advise(nodes: &[Node], baselines: &[CpuBaseline]) -> Vec<Advice>
```

```rust
enum Advice {
    BaselineWouldMerge {
        domains: Vec<DomainId>,
        model: String,
        level: CpuLevel,
        // Named per node, because this is the cost and it must not be buried.
        features_lost: Vec<(String, Vec<String>)>,
    },
    CannotMerge { nodes: Vec<String>, reason: CannotMerge },
    SplitByArch { groups: Vec<(String, Vec<String>)> },
    AlreadyUniform,
}
```

Read out, that is:

> This cell has 5 nodes in 3 migration domains.
>
> **node-a, node-b, node-c** (QEMU) run Skylake-SP, Skylake-SP and Cascade
> Lake. Baselining them to `Skylake-Server-v4` would put all three in one
> domain. Cost: node-c's guests lose `avx512_vnni`. No other node loses
> anything.
>
> **node-d, node-e** (Cloud Hypervisor) run EPYC Milan and EPYC Genoa. These
> cannot be brought together: Cloud Hypervisor has no CPU model to mask with.
> Live migration between them will stay refused. To make them one domain they
> must run the same CPU, or move to QEMU.

The first block is a recommendation with a named price. The second is a refusal
with a named cause and two real remedies. Neither is a warning icon that leaves
the operator to work it out.

**The rule that keeps this honest:** a recommendation always states what is
lost, per node. A baseline that quietly removes AVX-512 from a fleet whose
workloads use it is worse than no recommendation at all, because it arrives
wearing the platform's authority.

## 9. The lifecycle: what happens when a third generation arrives

The interesting part is not declaring a baseline. It is what happens to a
fleet that is already running guests when the baseline has to change.

### Where a baseline is declared

On the node: `NodeSpec.cpu_baseline`, a level. Not a resource with a label
selector, though that reads better on paper. A selector means a machine racked
next year silently joins an aggregate because of a label somebody set for an
unrelated reason — and quietly deciding what processor a new machine offers is
not a decision to make on an operator's behalf. Per-node keeps one writer per
object and needs no controller.

The cost is that a new node does not join by itself. That is paid back by §8:
`NodeOutsideTheAggregate` says so out loud, with the one distinction that
matters — whether the machine *could* join and simply has not been told, or is
a generation short and never can.

**An aggregate is not a resource.** It is a computed domain. Two baselines over
two sets of nodes are two aggregates; there is nothing else to create.

### Declaring one is checked before it is accepted

`cpu::can_present` compares the level's flags against what the node reported,
and the API refuses with the shortfall named:

> `node-c cannot present x86-64-v3: it lacks avx2, bmi1, bmi2, fma`

`enforce` would catch it eventually — QEMU refuses to start a guest it cannot
give the promised processor — but "the guests on node-c stopped booting" is a
long way from the sentence above.

### Changing one over a live fleet

**No guest is restarted, and none is moved.** A guest's CPU is recorded at
boot (§3), so a baseline governs guests started *after* it. The fleet adopts
the change as guests come and go, which is the third of the three options an
operator would otherwise have to choose between — and it is the default,
because it falls out of the invariant rather than being a feature bolted on.

While adoption is incomplete the cell is honestly mixed, and
`cpu::pending_adoption` lists exactly who is still on the old CPU, what they
are running with, and what they would get:

> 3 guests are still running x86-64-v3, started before the current baseline
> x86-64-v2. Each adopts it on its next restart.

Two things follow, and both are stated rather than discovered:

- A guest that has not adopted can only move to a node still presenting the
  CPU it booted with. After a fleet-wide change that may be **nowhere** — so
  such a guest is temporarily unmigratable, and the refusal says why.
- Nothing degrades. A pending guest keeps running exactly as it was.

### So the three options an operator has are all real

1. **Re-baseline lower** and let the fleet adopt it as guests restart. No
   forced outage; migration is limited until each guest has.
2. **Re-baseline lower and restart deliberately** to get the whole fleet into
   one domain now. The pending list is the work queue.
3. **A second aggregate.** Leave the existing nodes as they are and give the
   new generation its own baseline. Correct when the newcomer cannot reach the
   old level at all, which is the case `NodeOutsideTheAggregate` names.

The platform recommends; it never picks. Changing what processor a fleet
presents has a blast radius, and §8 exists to inform that decision rather than
pre-empt it.

## 10. What is deliberately not modelled yet

- **Per-instance CPU model pinning.** An instance may demand a *level*; it may
  not name a model. Naming a model makes an instance placeable on one kind of
  machine and turns a fleet-wide property into thousands of per-guest ones.
  If a real case appears, it arrives with the code that reads it.
- **Automatic baselining.** The platform recommends; it never silently changes
  what guests see. See §9 — adoption is automatic once a baseline is
  *declared*, but declaring one is always an operator's act.
- **Nested virtualisation flags, MSR-level compatibility.** Real, and beyond
  what flag-set comparison can honestly promise. When they matter, they matter
  as an explicit refusal, not an optimistic guess.
