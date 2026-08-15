//! What the agent does, stated as properties of the composed system.
//!
//! Every test here runs the real store, the real access rules and the real
//! reconcile functions against an in-process machine. Nothing is stubbed out
//! except the hypervisor, and that one behaves like a hypervisor: guests
//! outlive the agent, a start refuses without an image, a crash is visible.

mod common;

use std::{sync::Arc, time::Duration};

use common::*;
use velstra_cloud_model::{
    meta::ConditionStatus,
    resources::{DesiredState, InstanceState},
};
use velstra_cloud_nodeagent::{FakeDatapath, FakeVmm, Fault, Pass, agent::Agent};

const I1: &str = "projects/p1/instances/i1";
const PORT_A: &str = "projects/p1/ports/port-a";

/// One node, one instance, one port, everything in place.
async fn one_instance_on(
    node: &str,
) -> (
    Arc<dyn velstra_cloud_store::Store>,
    FakeVmm,
    FakeDatapath,
    Agent,
) {
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", node).await;
    create_instance(&store, I1, Some(node), Some(node), &[PORT_A]).await;
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), node, &vmm, &datapath);
    (store, vmm, datapath, agent)
}

#[tokio::test]
async fn one_pass_takes_a_new_instance_all_the_way_to_running() {
    let (store, vmm, datapath, agent) = one_instance_on("node-a").await;

    let pass = agent.resync().await;

    // Image, disk, port, guest — in that order, and all of it inside one pass,
    // so the resync interval is not secretly the boot latency.
    assert_eq!(pass.actions, 4, "{pass:?}");
    assert_eq!(pass.failures, 0, "{pass:?}");
    assert!(vmm.is_running(I1));
    assert!(datapath.is_programmed(PORT_A));

    let stored = read_instance(&store, I1).await;
    assert_eq!(stored.status.state, InstanceState::Running);
    assert_eq!(stored.status.node.as_deref(), Some("node-a"));
    assert_eq!(stored.status.observed_generation, stored.meta.generation);
    assert_eq!(stored.status.addresses, vec!["10.0.0.5/24".to_string()]);
    assert!(
        stored.status.vmm_pid.is_some(),
        "no host process was reported"
    );
    assert_eq!(
        condition(&stored.status.conditions, "Ready").status,
        ConditionStatus::True
    );
}

#[tokio::test]
async fn a_converged_node_performs_no_actions_and_writes_nothing() {
    // The property that makes the resync interval a matter of taste. If a pass
    // over a settled node did anything at all, every node in a cell would be
    // doing it, forever, at whatever rate the interval was set to.
    let (_store, _vmm, _datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    let second = agent.resync().await;
    assert_eq!(second, Pass::default(), "a settled node was not quiet");

    let third = agent.resync().await;
    assert_eq!(third, Pass::default());
}

#[tokio::test]
async fn the_agent_that_does_not_own_an_object_is_refused_and_does_not_fight_over_it() {
    // The shape of a half-finished migration: the scheduler has re-assigned the
    // instance to node-b, and node-a has not let go of it. If node-b acted on
    // the assignment alone there would be two machines running one guest.
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    create_instance(&store, I1, Some("node-b"), Some("node-a"), &[PORT_A]).await;

    let a_vmm = FakeVmm::new();
    let a_datapath = FakeDatapath::new();
    let node_a = node_agent(store.clone(), "node-a", &a_vmm, &a_datapath);

    let b_vmm = FakeVmm::new();
    let b_datapath = FakeDatapath::new();
    let node_b = node_agent(store.clone(), "node-b", &b_vmm, &b_datapath);

    let owner = node_a.resync().await;
    let stranger = node_b.resync().await;

    assert!(a_vmm.is_running(I1), "the owner did not run its own guest");
    assert_eq!(
        stranger.actions, 0,
        "a node acted on an object whose status belongs to somebody else"
    );
    assert_eq!(stranger.refused, 1, "the store did not refuse the stranger");
    assert_eq!(
        b_vmm.count(Fault::Start, I1),
        0,
        "a second guest was started"
    );
    assert!(!b_datapath.is_programmed(PORT_A));

    let stored = read_instance(&store, I1).await;
    assert_eq!(
        stored.status.node.as_deref(),
        Some("node-a"),
        "ownership moved"
    );
    assert!(owner.refused == 0, "the owner was refused its own object");
}

#[tokio::test]
async fn an_agent_that_died_before_reporting_re_derives_and_does_not_act_twice() {
    // The window that matters: the guest is started, and the process that
    // started it dies before it can say so. Nothing on disk remembers the
    // start, so the only way back is to look at the machine.
    let inner = store();
    create_port(&inner, PORT_A, "10.0.0.5/24", "node-a").await;
    create_instance(&inner, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let brittle = Brittle::wrapping(inner.clone());
    let store: Arc<dyn velstra_cloud_store::Store> = brittle.clone();
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();

    let doomed = node_agent(store.clone(), "node-a", &vmm, &datapath);
    brittle.cut();
    let last = doomed.resync().await;
    drop(doomed);

    assert!(vmm.is_running(I1), "the guest never started");
    assert_eq!(vmm.count(Fault::Start, I1), 1);
    assert!(last.failures > 0, "the lost report was not noticed");
    let stored = read_instance(&inner, I1).await;
    assert_eq!(
        stored.status.state,
        InstanceState::Unknown,
        "the cell somehow heard about it"
    );

    // A new agent, over the same machine, told nothing.
    brittle.restore();
    let successor = node_agent(store, "node-a", &vmm, &datapath);
    let recovery = successor.resync().await;

    assert_eq!(
        vmm.count(Fault::Start, I1),
        1,
        "the successor started the guest a second time"
    );
    assert_eq!(
        recovery.actions, 0,
        "the successor repeated work: {recovery:?}"
    );
    let stored = read_instance(&inner, I1).await;
    assert_eq!(stored.status.state, InstanceState::Running);
    assert_eq!(stored.status.observed_generation, 1);
}

#[tokio::test]
async fn killing_the_agent_does_not_kill_the_guests() {
    let (store, vmm, datapath, agent) = one_instance_on("node-a").await;
    let agent = Arc::new(agent);

    let running = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .run(async {
                    // Never completes: this loop is killed, not asked to stop.
                    std::future::pending::<()>().await;
                })
                .await
        })
    };

    // Wait for the guest rather than for a duration.
    for _ in 0..200 {
        if vmm.is_running(I1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(vmm.is_running(I1), "the loop never got the guest up");

    running.abort();
    let _ = running.await;
    drop(agent);

    // The agent is gone. The tenant's workload is not.
    assert!(vmm.is_running(I1), "killing the agent killed the guest");

    // And a fresh one finds it without being told.
    let successor = node_agent(store, "node-a", &vmm, &datapath);
    let pass = successor.resync().await;
    assert_eq!(pass.actions, 0, "{pass:?}");
    assert_eq!(vmm.count(Fault::Start, I1), 1);
}

#[tokio::test]
async fn the_loop_converges_on_a_watch_without_waiting_for_the_timer() {
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let mut config = velstra_cloud_nodeagent::AgentConfig::new("node-a", REGION, CELL);
    // Long enough that a test that passes cannot have been the timer.
    config.resync = Duration::from_secs(3600);
    let agent = Arc::new(Agent::new(
        store.clone(),
        config,
        Arc::new(vmm.clone()),
        Arc::new(datapath.clone()),
    ));

    let running = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.run(std::future::pending::<()>()).await })
    };
    // Created *after* the loop is up, so only the watch can carry it.
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let mut seen = InstanceState::Unknown;
    for _ in 0..400 {
        seen = read_instance(&store, I1).await.status.state;
        if seen == InstanceState::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    running.abort();
    assert_eq!(
        seen,
        InstanceState::Running,
        "the watch did not wake the loop"
    );
}

#[tokio::test]
async fn a_crashed_guest_is_restarted_and_the_object_says_so() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    vmm.crash(I1);
    let pass = agent.resync().await;

    assert_eq!(pass.actions, 1, "{pass:?}");
    assert_eq!(
        vmm.count(Fault::Start, I1),
        2,
        "the guest was not restarted"
    );
    let stored = read_instance(&store, I1).await;
    assert_eq!(stored.status.state, InstanceState::Running);
}

#[tokio::test]
async fn a_guest_that_will_not_start_says_why_on_its_own_object() {
    // The reason has to be where the person looking at the instance is, not in
    // a log file on whichever machine happened to be running it.
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    vmm.fail(Fault::Start, I1, "no hugepages left on this host");

    let pass = agent.resync().await;
    assert_eq!(pass.failures, 1, "{pass:?}");

    let stored = read_instance(&store, I1).await;
    let host = condition(&stored.status.conditions, "HostActions");
    assert_eq!(host.status, ConditionStatus::False);
    assert!(host.message.contains("hugepages"), "{host:?}");
    // It has still been seen: the object is not "converging" forever while a
    // real failure hides behind it.
    assert_eq!(stored.status.observed_generation, stored.meta.generation);
    assert_eq!(stored.status.state, InstanceState::Stopped);

    // And when the host recovers, the next pass fixes it with no help.
    vmm.heal(Fault::Start, I1);
    let pass = agent.resync().await;
    assert_eq!(pass.failures, 0, "{pass:?}");
    assert!(vmm.is_running(I1));
    let stored = read_instance(&store, I1).await;
    assert_eq!(
        condition(&stored.status.conditions, "HostActions").status,
        ConditionStatus::True
    );
}

#[tokio::test]
async fn stopping_an_instance_is_a_spec_change_the_node_notices() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    edit_instance(&store, I1, |spec| {
        spec.desired_state = DesiredState::Stopped
    })
    .await;
    let pass = agent.resync().await;

    assert_eq!(pass.actions, 1, "{pass:?}");
    assert!(!vmm.is_running(I1));
    let stored = read_instance(&store, I1).await;
    assert_eq!(stored.status.state, InstanceState::Stopped);
    assert_eq!(
        stored.status.observed_generation, 2,
        "the new generation was not acknowledged"
    );
    // Stopped as asked is a healthy object, not a broken one.
    assert_eq!(
        condition(&stored.status.conditions, "Ready").status,
        ConditionStatus::True
    );
}

#[tokio::test]
async fn deleting_tears_down_in_order_and_then_says_it_has_let_go() {
    let (store, vmm, datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    request_delete_instance(&store, I1).await;
    let pass = agent.resync().await;

    assert!(pass.actions >= 2, "{pass:?}");
    assert!(!vmm.is_running(I1));
    assert!(
        !datapath.is_programmed(PORT_A),
        "the port outlived the guest"
    );

    let stored = read_instance(&store, I1).await;
    assert_eq!(
        stored.status.state,
        InstanceState::Unknown,
        "the node still holds it"
    );
    assert!(stored.status.addresses.is_empty());
    let released = condition(&stored.status.conditions, "Released");
    assert_eq!(released.status, ConditionStatus::True, "{released:?}");
    // The finalizer is still there: dropping it is a metadata write, and the
    // node may not make one. It has published the fact a controller acts on.
    assert!(stored.meta.has_finalizer("node.velstra.io/release"));

    // Nothing destructive is repeated afterwards, and nothing more is written.
    //
    // The one action that does recur is the port unprogram: the delete branch
    // of `reconcile_instance` asks for every port in the spec whether or not it
    // is programmed, so a torn-down instance keeps being told to unprogram a
    // port that is already gone until a controller collects the object. It is
    // idempotent and harmless — and it is the one place in the model where an
    // action list does not go empty on a converged object. Worth fixing there,
    // not here.
    let after = agent.resync().await;
    assert_eq!(
        after.reports, 0,
        "a torn-down object was written again: {after:?}"
    );
    assert_eq!(after.failures, 0, "{after:?}");
    assert_eq!(
        vmm.count(Fault::Delete, I1),
        1,
        "the guest was deleted twice"
    );
}

#[tokio::test]
async fn an_attachment_is_opened_and_only_released_once_it_is_closed() {
    // No instance object on purpose: an attachment carries the node it belongs
    // to in its own spec, so the volume half of a node's work does not depend
    // on the compute half having arrived.
    let store = store();
    let attachment = "projects/p1/attachments/a1";
    let volume = "projects/p1/volumes/v1";
    create_attachment(&store, attachment, volume, I1, "node-a").await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    let pass = agent.resync().await;
    assert_eq!(pass.actions, 1, "{pass:?}");
    let stored = read_attachment(&store, attachment).await;
    assert!(stored.status.attached);
    assert_eq!(stored.status.device.as_deref(), Some("/dev/vdb"));
    assert_eq!(
        condition(&stored.status.conditions, "Released").status,
        ConditionStatus::False
    );

    request_delete_attachment(&store, attachment).await;
    let pass = agent.resync().await;
    assert_eq!(pass.actions, 1, "{pass:?}");

    let stored = read_attachment(&store, attachment).await;
    assert!(
        !stored.status.attached,
        "the volume is still open somewhere"
    );
    assert_eq!(
        condition(&stored.status.conditions, "Released").status,
        ConditionStatus::True,
        "the volume can never be attached elsewhere"
    );
    assert_eq!(agent.resync().await, Pass::default());
}

#[tokio::test]
async fn a_port_is_reported_by_the_node_whose_datapath_carries_it() {
    let (store, _vmm, _datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    let port = read_port(&store, PORT_A).await;
    assert!(port.status.programmed);
    assert_eq!(port.status.node.as_deref(), Some("node-a"));
    assert_eq!(port.status.tap_device.as_deref(), Some("vt-port-a"));
    assert_eq!(port.status.observed_generation, port.meta.generation);
}

#[tokio::test]
async fn an_instance_whose_port_has_not_arrived_waits_and_says_what_it_is_waiting_for() {
    // The two halves of a create do not arrive together, and the node's answer
    // to "the port is not here yet" must not be a guest on a dead network.
    let store = store();
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    let pass = agent.resync().await;
    assert_eq!(pass.failures, 1, "{pass:?}");
    assert!(!vmm.is_running(I1), "a guest came up without its network");
    let stored = read_instance(&store, I1).await;
    assert!(
        condition(&stored.status.conditions, "HostActions")
            .message
            .contains("port-a"),
        "{:#?}",
        stored.status.conditions
    );

    // The port arrives; nobody has to tell the agent anything.
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    agent.resync().await;
    assert!(vmm.is_running(I1));
}

#[tokio::test]
async fn a_node_reports_its_own_capacity_and_heartbeat() {
    // This was pinned here as a finding and has since been fixed in the model:
    // `NodeStatus::self_owned()` says a hypervisor is its own owner, because
    // nothing assigns a node to a node. Without it `access::judge` refused every
    // agent write to a node object, and the consequence was not small — the
    // scheduler places on `node.status.capacity` and liveness rests on
    // `last_heartbeat`, so a cell could never learn what it was made of.
    //
    // The agent was already written the way it should work: capacity read off
    // the machine, `allocated` counted from the objects it holds rather than
    // tracked. Only the rule was wrong.
    let store = store();
    create_node(&store, "node-a").await;
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    let pass = agent.resync().await;

    assert_eq!(
        pass.refused, 0,
        "a node was refused its own report: {pass:?}"
    );
    let node = read_node(&store, "node-a").await;
    assert!(node.status.capacity.vcpus > 0, "no capacity was reported");
    assert!(
        node.status.last_heartbeat.0 > 0,
        "no heartbeat was reported"
    );
}

#[tokio::test]
async fn an_object_assigned_here_and_owned_by_nobody_is_claimed() {
    // The other half of the same finding, also fixed in the model. `judge` now
    // takes ownership from two places: the node the *status* names, and — when
    // that is empty — the node a controller put in the *spec*. Requiring the
    // first alone was a deadlock, because ownership could only come from the
    // report that ownership was needed to make, so a freshly scheduled instance
    // could never start.
    //
    // The order still matters and is asserted in the model: while somebody owns
    // an object, only they may write it, even after a controller has re-assigned
    // it. A migration completes when the old owner lets go, not when the new one
    // grabs.
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    create_instance(&store, I1, Some("node-a"), None, &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    let pass = agent.resync().await;
    assert_eq!(pass.refused, 0, "the claim was refused: {pass:?}");

    // The first pass claims the object; acting on it comes after, once the
    // status says who owns it — a guest started before that would be a guest
    // the control plane does not know exists.
    let after = read_instance(&store, I1).await;
    assert_eq!(
        after.status.node.as_deref(),
        Some("node-a"),
        "the node did not take ownership of an object assigned to it"
    );

    agent.resync().await;
    assert!(
        vmm.is_running(I1),
        "the guest never started once ownership was settled"
    );
}
