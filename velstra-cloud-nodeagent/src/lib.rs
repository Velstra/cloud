//! The agent on a hypervisor.
//!
//! This is the component that makes "no message queue" true rather than a
//! slogan. A node holds **one** stream to its cell and nothing else: it watches
//! the objects assigned to itself, does what the pure reconcile functions in
//! [`velstra_cloud_model::reconcile`] say, and writes `status`. No controller
//! ever calls a node, there is no RPC to a hypervisor anywhere in this
//! codebase, and there is no broker in the middle to lose a message, replay
//! one, or become the thing that is down.
//!
//! What that buys, in the order it matters:
//!
//! - **Load per node is proportional to that node's own objects.** Not to the
//!   number of nodes, instances, tenants or controllers. This is the property
//!   OpenStack loses the moment every agent is on the same AMQP bus, where a
//!   node's work grows with the size of the cluster and a busy cell takes every
//!   hypervisor down with it.
//! - **Nothing is in flight.** An action is either done or not done, and the
//!   next pass can tell which by looking at the machine. Kill the agent at any
//!   instant and no object is left saying `BOOTING` with nobody left to finish.
//! - **The node is the only source of truth about the node.** State is
//!   re-derived from the VMM and the datapath on every start, never from a
//!   local database — see [`agent`].
//!
//! The pieces:
//!
//! | Module | What it is |
//! |---|---|
//! | [`agent`] | The loop: observe, decide, act, report. |
//! | [`host`] | [`host::Vmm`] and [`host::Datapath`] — what a node can be asked to do to itself. |
//! | [`fake`] | A hypervisor in a process. Deterministic, and good enough to exercise the whole platform. |
//! | [`cloud_hypervisor`] | One `cloud-hypervisor` per guest, under systemd. Parts of it are untested here and say so. |
//! | [`qemu`] | One `qemu-system-*` per guest, driven over QMP. The same, and untested here for the same reason. |
//! | [`hostfs`] | What both backends need from the machine: where things live, names that survive the filesystem, systemd. |
//! | [`metadata`] | `169.254.169.254`, answered locally, for the guests on this machine only. |

pub mod agent;
pub mod cloud_hypervisor;
pub mod fake;
pub mod host;
pub mod hostfs;
pub mod metadata;
pub mod qemu;

pub use agent::{Agent, AgentConfig, Pass};
pub use fake::{FakeDatapath, FakeNetwork, FakeVmm, Fault};
pub use host::{Datapath, HostError, HostState, Receiver, Transfer, VmObservation, VmRequest, Vmm};
pub use hostfs::Layout;
pub use metadata::{InstanceMetadata, MetadataRegistry};
