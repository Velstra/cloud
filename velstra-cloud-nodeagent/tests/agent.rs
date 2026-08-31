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
    security::{Direction, PortRange, Protocol, Remote, ResolvedRule, SecurityRule},
};
use velstra_cloud_nodeagent::{Datapath, FakeDatapath, FakeVmm, Fault, Pass, agent::Agent};

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

// ---- security groups -----------------------------------------------------
//
// That a group *resolves* correctly is proved in the model, where it is pure
// arithmetic. What cannot be proved there is that the answer ever reaches the
// datapath — which is precisely the gap this feature sat in: the port carried
// group names, the platform stored them, and nothing downstream ever asked what
// they meant.

#[tokio::test]
async fn what_a_port_is_allowed_reaches_the_datapath() {
    let store = store();
    create_security_group(
        &store,
        "projects/p1/security-groups/web",
        vec![SecurityRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange { from: 443, to: 443 }),
            remote: Remote::Cidr("0.0.0.0/0".into()),
        }],
    )
    .await;
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.5/24",
        "node-a",
        &["projects/p1/security-groups/web"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    let rules = datapath
        .rules_programmed(PORT_A)
        .expect("the port was never programmed at all");
    assert_eq!(
        rules,
        vec![ResolvedRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange { from: 443, to: 443 }),
            remote: "0.0.0.0/0".into(),
        }],
        "the datapath was programmed with something other than what the group says"
    );
}

#[tokio::test]
async fn a_port_in_no_group_is_programmed_with_no_allowances() {
    // Not the same as being unprogrammed, and the distinction is the whole
    // safety story: no allowances means the platform's default — ingress
    // denied — and not "the datapath was never told about this port".
    let (_store, _vmm, datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;
    assert_eq!(datapath.rules_programmed(PORT_A), Some(vec![]));
}

#[tokio::test]
async fn a_group_naming_another_follows_its_members_as_they_arrive() {
    // The property a stored expansion would break: nothing about `db` or its
    // port changes here, and what it is allowed changes anyway, because
    // membership is read off the ports on every pass.
    let store = store();
    create_security_group(
        &store,
        "projects/p1/security-groups/db",
        vec![SecurityRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange {
                from: 5432,
                to: 5432,
            }),
            remote: Remote::Group("projects/p1/security-groups/web".into()),
        }],
    )
    .await;
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.20/24",
        "node-a",
        &["projects/p1/security-groups/db"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;
    assert_eq!(
        datapath.rules_programmed(PORT_A),
        Some(vec![]),
        "an empty group let something through"
    );

    // A web server appears somewhere in the cell — on another node, which is the
    // point: membership is a property of the cell, not of this machine.
    create_port_in_groups(
        &store,
        "projects/p1/ports/port-web",
        "10.0.0.5/24",
        "node-b",
        &["projects/p1/security-groups/web"],
    )
    .await;
    agent.resync().await;
    let rules = datapath.rules_programmed(PORT_A).unwrap();
    assert_eq!(
        rules.iter().map(|r| r.remote.as_str()).collect::<Vec<_>>(),
        vec!["10.0.0.5/24"],
        "the new member never reached the datapath"
    );
}

#[tokio::test]
async fn naming_a_group_that_does_not_exist_does_not_stop_the_port_working() {
    // Rules only add allowances, so a missing group is strictly fewer of them —
    // the safe direction. Refusing to program the port would turn a typo into an
    // outage, which is a worse failure than the one being guarded against.
    let store = store();
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.5/24",
        "node-a",
        &["projects/p1/security-groups/typo"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    assert_eq!(datapath.rules_programmed(PORT_A), Some(vec![]));
    assert!(
        datapath.is_programmed(PORT_A),
        "a typo cost the guest its network"
    );
}

#[tokio::test]
async fn a_teardown_that_half_happened_is_asked_for_again() {
    // The guard on this branch used to be `is_deleting() && taps.contains_key(…)`,
    // and that second half was not a cheap early exit — it made the retry
    // impossible in exactly the case that needs one. `unprogram` has more to
    // undo than the tap: on the fabric datapath it also removes the port and
    // its security group, which hold an address and a MAC. Remove the tap, fail
    // on the rest, and the next pass sees no tap and concludes there is nothing
    // to do — so the fabric keeps the port for ever, *because* the teardown
    // half-succeeded.
    //
    // No instance uses the port, and that is load-bearing rather than
    // minimalism. With a live guest plugged into it, `instance_pass` programs
    // the port again on every pass, the tap comes back, and the teardown is
    // asked for a second time for a reason that has nothing to do with the
    // guard being right — a test that passes whether or not the fix is there.
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", "node-a").await;
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    // This node carries the port, which is the state a previous pass left.
    datapath
        .program(
            PORT_A,
            &velstra_cloud_model::resources::PortSpec::default(),
            &velstra_cloud_model::resources::NetworkSpec::default(),
            &[],
        )
        .await
        .expect("the fake datapath refused a port");
    assert!(datapath.is_programmed(PORT_A));

    datapath.fail_teardown(PORT_A, "the fabric would not let go of the port");
    request_delete_port(&store, PORT_A).await;

    let first = agent.resync().await;
    assert_eq!(first.failures, 1, "a failed teardown was not counted");
    assert_eq!(datapath.unprograms(PORT_A), 1);
    assert!(
        !datapath.is_programmed(PORT_A),
        "the tap survived, so this test is not exercising the half-done case"
    );

    // The second pass is the one that matters: the tap is genuinely gone now,
    // so anything that decided from the machine alone would say "released" and
    // let the guard come off over a fabric still holding an address.
    let second = agent.resync().await;
    assert_eq!(
        datapath.unprograms(PORT_A),
        2,
        "the teardown was never retried, because the half of it that succeeded \
         removed the evidence that the other half had not"
    );
    assert_eq!(second.failures, 1, "the retry's failure was not counted");
    let stored = read_port(&store, PORT_A).await;
    assert_ne!(
        condition(&stored.status.conditions, "Released").status,
        ConditionStatus::True,
        "a teardown that is still failing reported itself released: {:?}",
        stored.status.conditions
    );

    datapath.heal_teardown(PORT_A);
    agent.resync().await;
    let stored = read_port(&store, PORT_A).await;
    assert_eq!(
        condition(&stored.status.conditions, "Released").status,
        ConditionStatus::True,
        "a finished teardown never said so, so the guard would never come off: {:?}",
        stored.status.conditions
    );
}

/// A node fetches an image from the source the operator registered.
///
/// The gap this closes: `ImageSpec.source_url` was carried through the wire,
/// rendered in the console, set in every fixture — and read by nothing. A node
/// could *verify* an image (the sha256 is in its own name) and had no way to
/// *obtain* one, because the reconcile action carried only the digest. A fresh
/// cell could register an image, create an instance from it, and the guest
/// would never boot until somebody copied the file onto the node by hand.
///
/// The half that matters is the second assertion: the node must be handed the
/// *registered* source. Pulling from the wrong place and pulling from nowhere
/// look identical from outside.
#[tokio::test]
async fn a_node_is_told_where_to_fetch_an_image_from() {
    let (_store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    agent.resync().await;

    assert_eq!(
        vmm.pulled_from(IMAGE).as_deref(),
        Some("file:///var/lib/velstra/images/abc.raw"),
        "the node was not handed the registered image's source"
    );
}

/// An instance naming an image nothing registered says so on its own object,
/// rather than failing further down where the reason is gone.
#[tokio::test]
async fn an_unregistered_image_is_named_on_the_object() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    edit_instance(&store, I1, |spec| {
        spec.image = "projects/p1/images/sha256-nothingregistered".into()
    })
    .await;

    agent.resync().await;

    assert!(
        vmm.pulled_from("projects/p1/images/sha256-nothingregistered")
            .is_none(),
        "the node pulled an image that is not registered anywhere"
    );
    let after = read_instance(&store, I1).await;
    let said = format!("{:?}", after.status.conditions);
    assert!(
        said.contains("not a registered image"),
        "the object does not say why it cannot boot: {said}"
    );
}

/// The same thing again, read the way production reads it.
///
/// ## Why this test is a near-duplicate on purpose
///
/// `a_group_naming_another_follows_its_members_as_they_arrive` above builds its
/// agent over the store, which hands it every port in the cell — so working out
/// "who is in group web" from the ports is a question it can answer. Through the
/// API a node is handed **only its own** ports, and the same working-out yields
/// nothing: the web server is on `node-b`, whose port this node never sees.
///
/// The API computes that membership centrally and puts it on the group for
/// exactly this reason. The agent used to drop it — it kept `g.spec` and threw
/// the status away — so against a real API a rule naming another group expanded
/// to nothing, silently, and a guest lost traffic its tenant had allowed. Every
/// test was green, because every test was asking the wrong reader.
#[tokio::test]
async fn a_group_naming_another_expands_through_the_api_shaped_reader_too() {
    let store = store();
    create_security_group(
        &store,
        "projects/p1/security-groups/db",
        vec![SecurityRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange {
                from: 5432,
                to: 5432,
            }),
            remote: Remote::Group("projects/p1/security-groups/web".into()),
        }],
    )
    .await;
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.20/24",
        "node-a",
        &["projects/p1/security-groups/db"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    // The referenced group exists as an object, which is what the API computes
    // membership *for*. A group that is only ever named by ports and never
    // created is a different case, and one the API cannot answer either — there
    // is no object to put a `status` on.
    create_security_group(&store, "projects/p1/security-groups/web", vec![]).await;

    // The member is on another node, so this agent never sees its port object.
    // That is the whole point of the test.
    create_port_in_groups(
        &store,
        "projects/p1/ports/port-web",
        "10.0.0.5/24",
        "node-b",
        &["projects/p1/security-groups/web"],
    )
    .await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = common::api_shaped_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    let rules = datapath.rules_programmed(PORT_A).unwrap();
    assert_eq!(
        rules.iter().map(|r| r.remote.as_str()).collect::<Vec<_>>(),
        vec!["10.0.0.5/24"],
        "a member on another machine never reached the datapath: the node worked membership \
         out from the ports it can see instead of reading what the API computed"
    );
}

/// A member that is not bound to any node still counts.
///
/// The filter the API applies rejects a port with neither `spec.node` nor
/// `status.node`, so a node that recomputed membership from what it was handed
/// would miss one even in a single-machine cell — which is the case that makes
/// this a bug about *correctness* and not about topology.
#[tokio::test]
async fn an_unbound_member_of_a_referenced_group_still_expands() {
    let store = store();
    create_security_group(
        &store,
        "projects/p1/security-groups/db",
        vec![SecurityRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange {
                from: 5432,
                to: 5432,
            }),
            remote: Remote::Group("projects/p1/security-groups/web".into()),
        }],
    )
    .await;
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.20/24",
        "node-a",
        &["projects/p1/security-groups/db"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;
    create_security_group(&store, "projects/p1/security-groups/web", vec![]).await;
    // Created, addressed, and not yet placed anywhere.
    create_port_in_groups(
        &store,
        "projects/p1/ports/port-web",
        "10.0.0.9/24",
        "",
        &["projects/p1/security-groups/web"],
    )
    .await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = common::api_shaped_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    let rules = datapath.rules_programmed(PORT_A).unwrap();
    assert_eq!(
        rules.iter().map(|r| r.remote.as_str()).collect::<Vec<_>>(),
        vec!["10.0.0.9/24"],
        "an unbound member of the referenced group was dropped"
    );
}

/// A datapath that keeps its rules somewhere this process cannot read back
/// still converges.
///
/// ## The bug this pins
///
/// The check for "is this port current" used to compare the rules the datapath
/// *reported* against the rules this pass wanted. The fabric datapath cannot
/// report them: it observes through the tap layer, which is deliberately
/// programmed with no rules because the fabric is what enforces them. So the
/// comparison was `[] == [something]` on every pass, for ever.
///
/// The consequence was not a warning. `reconcile_instance` gates starting a
/// guest on its ports being current, so an instance with any security group at
/// all never reached `StartVm` on a real fabric — the pass returned `Ok`, the
/// port was re-programmed, and the instance sat at `Ready=False` beside a host
/// that had done everything it was asked.
#[tokio::test]
async fn a_datapath_that_cannot_report_its_rules_still_converges() {
    let store = store();
    create_security_group(
        &store,
        "projects/p1/security-groups/web",
        vec![SecurityRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange { from: 80, to: 82 }),
            remote: Remote::Cidr("0.0.0.0/0".into()),
        }],
    )
    .await;
    create_port_in_groups(
        &store,
        PORT_A,
        "10.0.0.5/24",
        "node-a",
        &["projects/p1/security-groups/web"],
    )
    .await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new().holding_rules_elsewhere();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    agent.resync().await;
    assert!(
        vmm.is_running(I1),
        "the guest never started: its port read as out of date because the datapath cannot \
         report the rules it holds"
    );

    // And the second pass is quiet. Re-programming a port that is already right,
    // every pass, for ever, is the other half of the same bug — and the half
    // that would keep a converged node permanently busy.
    let settled = agent.resync().await;
    assert_eq!(
        settled.actions, 0,
        "a converged node kept working: {settled:?}"
    );
}

// ---- captures --------------------------------------------------------------

/// A capture is real bytes with a real digest, or it is nothing.
///
/// Until this pass existed the object was created, assigned to the node holding
/// the disk, and no agent ever claimed it — so the controller that turns a
/// finished capture into an image never had a finished one to act on. The whole
/// feature was a chain with the middle link missing.
#[tokio::test]
async fn a_stopped_guest_is_captured_into_bytes_with_a_digest() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    let dir = std::env::temp_dir().join(format!("velstra-capture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A guest with a disk, stopped — which is the state a template is made
    // from, because a disk copied from under a running machine is
    // crash-consistent and a template is stamped out by people who assume it
    // is clean.
    let disk = dir.join("root.raw");
    std::fs::write(&disk, b"a golden image, more or less").unwrap();
    vmm.with_disk_file(I1, disk);
    stop_instance(&store, I1).await;
    agent.resync().await;

    create_target(&store, "archive", dir.to_string_lossy().as_ref()).await;
    create_capture(&store, "golden", I1, "node-a").await;

    // One pass claims it, the next makes it. Claiming first is not ceremony: a
    // node that copied before claiming would have two nodes copying one disk
    // the first time an assignment moved.
    agent.resync().await;
    let claimed = read_capture(&store, "golden").await;
    assert_eq!(claimed.status.node.as_deref(), Some("node-a"));
    assert!(
        claimed.status.digest.is_none(),
        "it was copied before it was claimed"
    );

    agent.resync().await;
    let done = read_capture(&store, "golden").await;
    let digest = done
        .status
        .digest
        .clone()
        .expect("a capture that finished says what the bytes hashed to");
    assert!(digest.starts_with("sha256:"), "{digest}");
    assert!(done.status.size_bytes > 0);
    assert!(done.status.finished_at.is_some());

    // The bytes are on the target, under the name a pull will ask for — the
    // digest, which is what makes fetching one verifiable.
    let landed = dir.join(digest.replace(':', "-"));
    assert!(landed.exists(), "nothing was written to the target");
    assert_eq!(
        std::fs::read(&landed).unwrap(),
        b"a golden image, more or less",
        "the copy is not the disk"
    );

    // And a finished capture is never made twice.
    let before = read_capture(&store, "golden").await.meta.revision;
    agent.resync().await;
    assert_eq!(
        read_capture(&store, "golden").await.meta.revision,
        before,
        "a finished capture was written again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The refusal the whole thing exists for, said on the object rather than in a
/// log on whichever machine happened to hold the guest.
#[tokio::test]
async fn a_running_guest_is_refused_and_told_what_to_use_instead() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    let dir = std::env::temp_dir().join(format!("velstra-capture-running-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("root.raw");
    std::fs::write(&disk, b"still being written to").unwrap();
    vmm.with_disk_file(I1, disk);

    // Running, which is the ordinary state and the one that must be refused.
    agent.resync().await;
    create_target(&store, "archive", dir.to_string_lossy().as_ref()).await;
    create_capture(&store, "golden", I1, "node-a").await;
    for _ in 0..3 {
        agent.resync().await;
    }

    let refused = read_capture(&store, "golden").await;
    assert!(
        refused.status.digest.is_none(),
        "a running guest was captured"
    );
    let ready = velstra_cloud_model::meta::condition(&refused.status.conditions, "Ready")
        .expect("a capture that did not happen says why");
    assert!(
        ready.message.contains("crash-consistent"),
        "the reason is not the model's own words: {}",
        ready.message
    );
    assert!(
        ready.message.contains("take a backup"),
        "it does not say which tool does what they wanted: {}",
        ready.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_volume_whose_pool_has_not_said_where_it_is_waits_instead_of_guessing() {
    // The bug this is about, stated plainly: the node used to build a path —
    // `…/instances/<guest>/<volume>` — out of the two names it had. Nothing ever
    // writes that path, so every attach failed with `No such file or directory`
    // and the attachment sat at `attached: false` for as long as the cell ran.
    // It survived because attaching a disk took three manual steps, so nobody
    // did it; the tests passed because their fake hypervisor opened whatever it
    // was handed.
    //
    // Now the pool says where the bytes are and the node opens that. Which means
    // there is a moment where it does not know yet — and the honest behaviour
    // then is to wait and say so, not to invent a path.
    let store = store();
    let attachment = "projects/p1/attachments/a1";
    let volume = "projects/p1/volumes/nirgends";
    create_attachment(&store, attachment, volume, I1, "node-a").await;

    // No place on it: a pool that has claimed the volume and not yet said where
    // it put the bytes, which is what a node sees whenever an attachment is made
    // before the volume behind it is provisioned.
    let store_of = velstra_cloud_store::TypedStore::<
        velstra_cloud_model::resources::AttachmentSpec,
        velstra_cloud_model::resources::AttachmentStatus,
    >::new(store.clone(), CELL, "attachments");
    let mut stored = store_of.get(attachment).await.unwrap().unwrap();
    stored.spec.at = String::new();
    // A change to a spec bumps the generation; the store refuses one that does
    // not, which is what stops a silent edit reading as an old object.
    stored.meta.generation += 1;
    store_of
        .update(
            &stored,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    let pass = agent.resync().await;

    let after = read_attachment(&store, attachment).await;
    assert!(
        !after.status.attached,
        "a disk was reported open with nothing to open"
    );
    assert_eq!(
        vmm.count(Fault::OpenVolume, I1),
        0,
        "the hypervisor was told to open something the pool had not placed"
    );
    let why = condition(&after.status.conditions, "HostActions");
    assert!(
        why.message.contains("has not been placed"),
        "the attachment does not say what it is waiting for: {}",
        why.message
    );
    let _ = pass;
}

/// A guest whose instance is gone from the cell is stopped, not carried.
///
/// The delete pipeline stops a guest while its record still exists; a guest
/// that survives past that (an agent that was down for the whole deletion) has
/// nobody left to ask for its end. Found live as a QEMU running two days after
/// its instance was deleted, holding a tap the sweep could never remove.
#[tokio::test]
async fn a_guest_whose_instance_is_gone_is_stopped() {
    let store = store();
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);

    // The guest is running and the store has never heard of it.
    vmm.start_detached(I1);
    assert!(vmm.is_running(I1));

    agent.resync().await;
    assert!(
        !vmm.is_running(I1),
        "a guest with no instance anywhere kept running"
    );
}

/// A small wobble in the free-memory reading is not a report.
///
/// /proc never answers the same twice on a busy machine, so "did anything
/// change" was answered yes on every pass — and every write woke the watch
/// that started the next pass. The fresh figure still goes out on the
/// heartbeat cadence.
#[tokio::test]
async fn memory_jitter_alone_is_not_worth_a_write() {
    let (store, vmm, _datapath, agent) = one_instance_on("node-a").await;
    create_node(&store, "node-a").await;
    agent.resync().await;
    agent.resync().await;
    let settled = agent.resync().await;
    assert_eq!(settled, Pass::default());

    // A few MiB of jitter, as any second look at /proc produces.
    vmm.wobble_free_memory(16);
    let jittered = agent.resync().await;
    assert_eq!(
        jittered,
        Pass::default(),
        "a MiB-scale wobble in free memory was written to the store"
    );

    // A real movement is reported without waiting for the heartbeat.
    vmm.wobble_free_memory(2048);
    let moved = agent.resync().await;
    assert_eq!(moved.reports, 1, "a real memory movement was not reported: {moved:?}");
    let node = read_node(&store, "node-a").await;
    assert!(node.status.capacity.memory_mib > 0);
}

/// A gateway keeps the routing daemon honest and reports the far end's answer.
///
/// The announcements are derived — every external subnet, a host route per
/// floating address in front of something — and a settled cell reloads
/// nothing: the second pass applies zero times.
#[tokio::test]
async fn a_gateway_announces_what_the_cell_claims_and_a_settled_one_is_quiet() {
    use velstra_cloud_model::resources::{
        BgpPeerSpec, BgpPeerStatus, FloatingIpSpec, FloatingIpStatus, NetworkSpec, NetworkStatus,
        SubnetSpec, SubnetStatus,
    };
    use velstra_cloud_store::TypedStore;
    use velstra_cloud_model::resources::Resource;
    let store = store();
    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let bgp = velstra_cloud_nodeagent::fake::FakeBgp::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath)
        .with_bgp(Arc::new(bgp.clone()));

    let writer = velstra_cloud_model::access::Writer::controller("test");
    let peers: TypedStore<BgpPeerSpec, BgpPeerStatus> =
        TypedStore::new(store.clone(), CELL, "bgp-peers");
    peers
        .create(
            &Resource::new(
                meta("bgp-peers/edge"),
                BgpPeerSpec {
                    peer: "10.10.10.1".into(),
                    peer_as: 65000,
                    local_as: 65010,
                    node: "node-a".into(),
                    description: String::new(),
                },
                BgpPeerStatus::default(),
            ),
            &writer,
        )
        .await
        .unwrap();
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), CELL, "networks");
    let public = NetworkSpec { external: true, ..Default::default() };
    networks
        .create(
            &Resource::new(meta("networks/public"), public, NetworkStatus::default()),
            &writer,
        )
        .await
        .unwrap();
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(store.clone(), CELL, "subnets");
    subnets
        .create(
            &Resource::new(
                meta("subnets/public-v4"),
                SubnetSpec {
                    network: "networks/public".into(),
                    cidr: "203.0.113.0/24".into(),
                    gateway: String::new(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ),
            &writer,
        )
        .await
        .unwrap();
    let floating: TypedStore<FloatingIpSpec, FloatingIpStatus> =
        TypedStore::new(store.clone(), CELL, "floatingips");
    let fip = FloatingIpSpec {
        address: Some("203.0.113.7".into()),
        port: "projects/p1/ports/x".into(),
        ..Default::default()
    };
    floating
        .create(
            &Resource::new(meta("projects/p1/floatingips/a"), fip, FloatingIpStatus::default()),
            &writer,
        )
        .await
        .unwrap();

    bgp.answer("10.10.10.1", "Established", 2);
    agent.resync().await;

    let said = bgp.applied().expect("the daemon was never programmed");
    assert_eq!(said.networks_v4, vec!["203.0.113.0/24"]);
    assert_eq!(said.hosts_v4, vec!["203.0.113.7/32"]);
    let after = peers.get("bgp-peers/edge").await.unwrap().unwrap();
    assert_eq!(after.status.node.as_deref(), Some("node-a"));
    assert_eq!(after.status.session, "Established");
    assert_eq!(after.status.announced, 2);
    assert_eq!(
        velstra_cloud_model::meta::condition(&after.status.conditions, "Ready")
            .map(|c| c.status),
        Some(velstra_cloud_model::meta::ConditionStatus::True)
    );

    // Settled means settled: nothing changed, so the daemon is not reloaded
    // and the object is not rewritten.
    let applies = bgp.applies();
    let rev = after.meta.revision;
    agent.resync().await;
    assert_eq!(bgp.applies(), applies, "a settled cell reloaded the daemon");
    let again = peers.get("bgp-peers/edge").await.unwrap().unwrap();
    assert_eq!(again.meta.revision, rev, "a settled session was rewritten");

    // A machine that is not the speaker leaves everything alone.
    let other_bgp = velstra_cloud_nodeagent::fake::FakeBgp::new();
    let other = node_agent(store.clone(), "node-b", &FakeVmm::new(), &FakeDatapath::new())
        .with_bgp(Arc::new(other_bgp.clone()));
    other.resync().await;
    assert!(other_bgp.applied().is_none(), "a bystander programmed its daemon");
}
