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
//! - **No shared bus.** A node's work does not grow with the number of *other*
//!   nodes, tenants or controllers, and no broker in the middle can lose a
//!   message, replay one, or become the thing that is down. This is the property
//!   OpenStack gives up the moment every agent is on one AMQP exchange, where a
//!   busy cell takes every hypervisor with it.
//! - **Load per node is proportional to that node's own objects** — with
//!   `--api`, and not without it.
//!
//!   The store cannot filter a range read by anything but a key prefix, and
//!   which node holds an object is a *field*, because it changes when a guest
//!   moves. So an agent reading the store directly lists every instance, port,
//!   attachment and migration in the cell on every pass and watches them
//!   unfiltered — O(cell) per node, and every write delivered once per node.
//!   That, and not the store's capacity, is what used to bound a cell.
//!
//!   Pointed at the API instead ([`cell::CellReader`], [`api_cell::ApiCell`]), a
//!   node is handed the objects it holds or has been given, and the API serves
//!   every node from **one** watch per collection. Measured in
//!   `velstra-cloud-api/tests/scaling.rs`: a node's list costs the store zero
//!   reads whatever the size of the cell, and fifty subscribers are one watcher.
//!
//!   Writes go straight to the store either way, deliberately: a node's writes
//!   are already proportional to its own work, and putting a second process in
//!   the path of a status report buys nothing.
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
//! | [`guests`] | Who is on this machine, and how a request is recognised as one of them. |
//! | [`metadata`] | `169.254.169.254`, answered locally, for the guests on this machine only. |
//! | [`dhcp`] | An address, a gateway and a name on each tap — publishing what the Port already says. |

pub mod agent;
pub mod api_cell;
pub mod cell;
pub mod ceph_deploy;
pub mod ceph_pool;
pub mod cephadm;
pub mod cloud_hypervisor;
pub mod datapath;
pub mod devices;
pub mod dhcp;
pub mod directory_pool;
pub mod fabric;
pub mod fake;
pub mod guests;
pub mod host;
pub mod hostfs;
pub mod metadata;
pub mod pool;
pub mod qemu;
pub(crate) mod reporting;

pub use agent::{Agent, AgentConfig, Pass};
pub use cell::{CellReader, StoreCell};
pub use cloud_hypervisor::CloudHypervisorVmm;
pub use fake::{FakeDatapath, FakeNetwork, FakeVmm, Fault};
pub use guests::{GuestRegistry, GuestView, Interface};
pub use host::{
    Datapath, HostError, HostState, Nic, Receiver, Result as HostResult, Transfer, VmObservation,
    VmRequest, Vmm,
};
pub use hostfs::{Boot, Layout, Scope};
pub use pool::{FakePool, PoolAgent, PoolConfig, PoolState, Storage};
pub use qemu::QemuVmm;
