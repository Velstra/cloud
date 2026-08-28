//! Moving a running guest between two nodes, as properties of the two agents.
//!
//! Both nodes here are real agents over real machines, running against the real
//! store and the real access rules; the only thing standing in for a hypervisor
//! is [`FakeVmm`], and it behaves like one — a receiver that can fail to bind, a
//! transfer that can fail half way and leave the guest where it was, and a wire
//! between two machines that a URL either leads to or does not.
//!
//! The order every test below depends on is not this codebase's invention: both
//! Cloud Hypervisor and QEMU require the receiving side to be listening before
//! the sending side may send. What is ours is that the model makes it
//! impossible to get wrong and these tests say so out loud.

mod common;

use common::*;
use velstra_cloud_model::{
    meta::ConditionStatus,
    migration::{MigrationMode, MigrationStatus},
    resources::InstanceState,
};
use velstra_cloud_nodeagent::{
    Agent, FakeDatapath, FakeNetwork, FakeVmm, Fault, Pass, VmRequest, Vmm,
};

const I1: &str = "projects/p1/instances/i1";
const PORT_A: &str = "projects/p1/ports/port-a";
const M1: &str = "projects/p1/migrations/m1";
const SOURCE: &str = "node-a";
const DESTINATION: &str = "node-b";

/// Two nodes on one wire, a guest running on the first of them, and a migration
/// asking for it to be on the second.
struct Cell {
    store: std::sync::Arc<dyn velstra_cloud_store::Store>,
    source: Agent,
    source_vmm: FakeVmm,
    destination: Agent,
    destination_vmm: FakeVmm,
    destination_datapath: FakeDatapath,
}

async fn two_nodes(status: MigrationStatus) -> Cell {
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", SOURCE).await;
    create_instance(&store, I1, Some(SOURCE), Some(SOURCE), &[PORT_A]).await;

    // One network, so a URL one machine publishes is a URL the other can
    // actually send to. Two unconnected fakes would let every test pass by
    // never really moving anything.
    let wire = FakeNetwork::new();
    let source_vmm = wire.host(SOURCE);
    let destination_vmm = wire.host(DESTINATION);
    let source_datapath = FakeDatapath::new();
    let destination_datapath = FakeDatapath::new();

    let cell = Cell {
        source: node_agent(store.clone(), SOURCE, &source_vmm, &source_datapath),
        destination: node_agent(
            store.clone(),
            DESTINATION,
            &destination_vmm,
            &destination_datapath,
        ),
        source_vmm,
        destination_vmm,
        destination_datapath,
        store,
    };
    // The guest is up on the source before anything about the migration
    // happens, which is the only situation a live migration starts from.
    cell.source.resync().await;
    assert!(cell.source_vmm.is_running(I1), "the guest never started");
    // The migration is created *after* the guest is up, which is the only order
    // the platform allows: `may_migrate` refuses one for an instance that is not
    // running. Creating it first also asked the source to start a guest it was
    // simultaneously forbidden to start.
    create_migration(&cell.store, M1, I1, SOURCE, DESTINATION, status).await;
    cell
}

impl Cell {
    /// The destination's two first passes: claim the object, then listen.
    ///
    /// Two passes and not one on purpose — claiming is a write and nothing on
    /// the machine happens until the cell knows which node is running this
    /// migration, exactly as for an instance.
    async fn destination_listens(&self) {
        self.destination.resync().await;
        self.destination.resync().await;
    }
}

#[tokio::test]
async fn the_destination_publishes_a_url_before_the_source_does_anything_at_all() {
    let cell = two_nodes(MigrationStatus::default()).await;

    // The source, given a migration with nothing published on it, must do
    // nothing whatsoever — not begin, not "prepare", not touch the guest.
    let quiet = cell.source.resync().await;
    assert_eq!(quiet, Pass::default(), "the source acted first: {quiet:?}");
    assert_eq!(cell.source_vmm.count(Fault::Send, I1), 0);
    assert!(cell.source_vmm.is_running(I1));

    cell.destination_listens().await;

    let migration = read_migration(&cell.store, M1).await;
    assert_eq!(
        migration.status.node.as_deref(),
        Some(DESTINATION),
        "the destination did not take ownership of the migration"
    );
    let url = migration
        .status
        .receiver_url
        .as_deref()
        .expect("no receiver URL was published");
    assert!(url.starts_with("tcp:") || url.starts_with("unix:"), "{url}");
    assert!(migration.status.receiver_ready);
    assert!(cell.destination_vmm.is_receiving(I1));
    assert_eq!(
        migration.status.observed_generation,
        migration.meta.generation
    );

    // And the destination has everything the guest will need the moment it
    // lands: the transfer carries memory and device state, not the disk it
    // resumes into and not the tap its configuration names.
    let host = cell.destination_vmm.observe().await.unwrap();
    assert!(
        host.disks.contains(I1),
        "the guest would arrive with no disk"
    );
    assert!(
        host.images.contains(IMAGE_STORED),
        "the destination has no verified copy of the bytes"
    );
    assert!(
        cell.destination_datapath.is_programmed(PORT_A),
        "the guest would resume onto a tap that does not exist"
    );
    assert!(
        !cell.destination_vmm.is_running(I1),
        "the destination started a second copy of the guest"
    );
}

#[tokio::test]
async fn the_source_waits_for_a_receiver_that_is_listening_not_merely_for_a_url() {
    // A URL that was published and is no longer being listened to is the exact
    // shape of a receiver whose process died: the field is still there, and
    // sending to it sends into nothing.
    let cell = two_nodes(MigrationStatus {
        node: Some(DESTINATION.to_string()),
        receiver_url: Some("tcp:node-b:4901".to_string()),
        receiver_ready: false,
        ..Default::default()
    })
    .await;

    let pass = cell.source.resync().await;

    assert_eq!(
        cell.source_vmm.count(Fault::Send, I1),
        0,
        "the source sent to a receiver that had not confirmed it was listening"
    );
    assert_eq!(pass, Pass::default(), "{pass:?}");
    assert!(cell.source_vmm.is_running(I1));
    assert_eq!(
        read_instance(&cell.store, I1).await.status.node.as_deref(),
        Some(SOURCE)
    );
}

#[tokio::test]
async fn a_failed_send_leaves_the_guest_running_where_it_was() {
    // Pre-copy's one great property, and the reason it is the default: until
    // the last pages are handed over the source still has a running guest, so a
    // transfer that dies costs the memory that was copied and nothing else.
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    cell.source_vmm
        .fail(Fault::Send, I1, "the far end stopped answering");

    let pass = cell.source.resync().await;

    assert!(
        cell.source_vmm.is_running(I1),
        "a failed migration took the guest with it"
    );
    assert_eq!(pass.failures, 1, "{pass:?}");
    let instance = read_instance(&cell.store, I1).await;
    assert_eq!(
        instance.status.node.as_deref(),
        Some(SOURCE),
        "the source let go of a guest it still has"
    );
    assert_eq!(instance.status.state, InstanceState::Running);
    // And the reason is on the object an operator is already looking at,
    // because the source may not write the migration's own status.
    let host = condition(&instance.status.conditions, "HostActions");
    assert_eq!(host.status, ConditionStatus::False);
    assert!(host.message.contains("stopped answering"), "{host:?}");

    // What was copied before it failed is not pretended away: the destination
    // reports it, because the destination is the one that received it.
    cell.destination.resync().await;
    let migration = read_migration(&cell.store, M1).await;
    assert!(
        migration.status.transferred_mib > 0,
        "a transfer that copied memory reported none: {:?}",
        migration.status
    );
    assert!(
        migration.status.receiver_ready,
        "the receiver was torn down after one failure"
    );

    // The machine recovers and nobody has to intervene: the next pass sends
    // again, on the receiver that is still listening.
    cell.source_vmm.heal(Fault::Send, I1);
    cell.source.resync().await;
    assert!(
        !cell.source_vmm.is_running(I1),
        "the retry did not move the guest"
    );
    assert!(cell.destination_vmm.is_running(I1));
}

#[tokio::test]
async fn a_finished_send_is_reported_by_letting_go_and_by_nothing_else() {
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    let before = read_migration(&cell.store, M1).await.meta.revision;

    cell.source.resync().await;

    // The guest is on the other machine, and it is the same guest: the fake
    // carries its start time across, the way a live migration does.
    assert!(!cell.source_vmm.is_running(I1));
    assert!(cell.destination_vmm.is_running(I1));

    let instance = read_instance(&cell.store, I1).await;
    assert_eq!(
        instance.status.node, None,
        "the source did not let go, so nothing can ever pick it up"
    );
    // `Unknown` and not `Stopped`: this node stopped nothing, it simply cannot
    // see the guest any more.
    assert_eq!(instance.status.state, InstanceState::Unknown);
    assert!(instance.status.vmm_pid.is_none());
    assert!(instance.status.addresses.is_empty());

    // And the source wrote nothing at all on the migration. It is not its
    // object: two agents on one status is the failure this whole design
    // removes, and a source that reported progress would be the second one.
    let after = read_migration(&cell.store, M1).await;
    assert_eq!(
        after.meta.revision, before,
        "the source wrote the migration's status"
    );
    assert_eq!(after.status.node.as_deref(), Some(DESTINATION));

    // Nothing has claimed the instance yet, and the source does not take it
    // back on the next pass even though the spec still names it.
    let again = cell.source.resync().await;
    assert_eq!(
        again,
        Pass::default(),
        "the source took the guest back: {again:?}"
    );
    assert!(!cell.source_vmm.is_running(I1));
}

#[tokio::test]
async fn a_migration_that_has_happened_costs_nothing_to_reconcile() {
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    cell.source.resync().await;

    // The controller's one spec write, at the one moment the model allows it:
    // the source has reported that it no longer has the guest.
    let released = read_instance(&cell.store, I1).await;
    assert!(released.status.node.is_none());
    reassign_instance(&cell.store, I1, DESTINATION).await;

    cell.destination.resync().await; // claim
    cell.destination.resync().await; // report it running here

    let instance = read_instance(&cell.store, I1).await;
    assert_eq!(instance.status.node.as_deref(), Some(DESTINATION));
    assert_eq!(instance.status.state, InstanceState::Running);
    assert_eq!(instance.status.addresses, vec!["10.0.0.5/24".to_string()]);
    assert_eq!(
        condition(&instance.status.conditions, "Ready").status,
        ConditionStatus::True
    );
    // Nothing is left listening on the destination: the receiver was what took
    // delivery, and once it has, it is the guest's VMM and not a receiver.
    assert!(!cell.destination_vmm.is_receiving(I1));
    let migration = read_migration(&cell.store, M1).await;
    assert!(!migration.status.receiver_ready);
    assert!(migration.status.receiver_url.is_none());

    // The property that makes the resync interval a matter of taste, on both
    // sides of a finished migration.
    for round in 0..2 {
        let source = cell.source.resync().await;
        let destination = cell.destination.resync().await;
        assert_eq!(source, Pass::default(), "source, round {round}: {source:?}");
        assert_eq!(
            destination,
            Pass::default(),
            "destination, round {round}: {destination:?}"
        );
    }
    assert_eq!(
        cell.source_vmm.count(Fault::Send, I1),
        1,
        "the guest was sent more than once"
    );
    assert_eq!(cell.destination_vmm.count(Fault::Start, I1), 0);
}

#[tokio::test]
async fn a_receiver_that_outlived_its_transfer_is_taken_down() {
    // A receiver left listening holds this node's memory for a guest it is not
    // running. The trigger is not "the transfer ended" — nothing here would
    // know — it is that the instance is now running on this node, which is a
    // fact anybody can see.
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", DESTINATION).await;
    create_instance(&store, I1, Some(DESTINATION), Some(DESTINATION), &[PORT_A]).await;
    create_migration(
        &store,
        M1,
        I1,
        SOURCE,
        DESTINATION,
        MigrationStatus {
            node: Some(DESTINATION.to_string()),
            receiver_url: Some("tcp:node-b:4901".to_string()),
            receiver_ready: true,
            ..Default::default()
        },
    )
    .await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), DESTINATION, &vmm, &datapath);
    vmm.cache_image(IMAGE);
    vmm.create_disk(
        I1,
        20,
        "projects/p1/images/sha256-abc",
        velstra_cloud_model::resources::ImageFormat::Raw,
    )
    .await
    .unwrap();
    let request = VmRequest {
        devices: Vec::new(),
        instance: I1.to_string(),
        vcpus: 2,
        memory_mib: 2048,
        image: IMAGE.to_string(),
        root_disk_gib: 20,
        nics: vec![],
        cpu_baseline: None,
    };
    // A receive process that did not exit when its transfer did: the guest is
    // here and something is still listening for it.
    vmm.prepare_receiver(&request, MigrationMode::Live)
        .await
        .unwrap();
    vmm.start(&request).await.unwrap();
    assert!(vmm.is_receiving(I1) && vmm.is_running(I1));

    agent.resync().await;

    assert!(
        !vmm.is_receiving(I1),
        "the receiver is still holding memory"
    );
    assert_eq!(vmm.count(Fault::TearDownReceiver, I1), 1);
    assert!(
        vmm.is_running(I1),
        "tearing down the receiver killed the guest"
    );
    let migration = read_migration(&store, M1).await;
    assert!(!migration.status.receiver_ready);
    assert!(migration.status.receiver_url.is_none());

    // …and a second pass over the same finished migration does nothing at all.
    assert_eq!(agent.resync().await, Pass::default());
}

#[tokio::test]
async fn a_transfer_that_is_under_way_is_not_started_a_second_time() {
    // `reconcile_source` asks for a send on every pass until the guest is gone,
    // and that is right — the ask has not changed. Obeying it twice would put a
    // second transfer of one guest on the wire while the first is still copying.
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    cell.source_vmm.stall(I1);

    cell.source.resync().await;
    assert!(cell.source_vmm.is_sending(I1), "the transfer never started");
    assert!(
        cell.source_vmm.is_running(I1),
        "the guest stopped mid-transfer"
    );

    for _ in 0..3 {
        cell.source.resync().await;
    }
    assert_eq!(
        cell.source_vmm.count(Fault::Send, I1),
        1,
        "the source started the transfer again while it was still running"
    );

    // When it lands, the source notices on its own — nobody tells it.
    cell.source_vmm.finish_transfer(I1).unwrap();
    cell.source.resync().await;
    assert_eq!(read_instance(&cell.store, I1).await.status.node, None);
    assert!(cell.destination_vmm.is_running(I1));
}

#[tokio::test]
async fn abandoning_a_migration_keeps_the_guest_and_stops_the_receiver() {
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    cell.source_vmm.stall(I1);
    cell.source.resync().await;
    assert!(cell.source_vmm.is_sending(I1));

    request_delete_migration(&cell.store, M1).await;
    cell.source.resync().await;
    cell.destination.resync().await;

    assert!(
        !cell.source_vmm.is_sending(I1),
        "the transfer was not stopped"
    );
    assert!(
        cell.source_vmm.is_running(I1),
        "abandoning a pre-copy migration lost the guest"
    );
    assert_eq!(
        read_instance(&cell.store, I1).await.status.node.as_deref(),
        Some(SOURCE),
        "the source let go of a guest it still has"
    );
    assert!(
        !cell.destination_vmm.is_receiving(I1),
        "the destination is still holding memory for a guest that is not coming"
    );

    // Cancelling is not repeated on every pass afterwards: there is nothing
    // left in flight to cancel, and an abandoned migration must stop costing.
    let after = cell.source.resync().await;
    assert_eq!(after.actions, 0, "{after:?}");
    assert_eq!(cell.source_vmm.count(Fault::CancelSend, I1), 1);
}

#[tokio::test]
async fn a_destination_that_cannot_listen_says_so_and_the_source_stays_put() {
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_vmm
        .fail(Fault::PrepareReceiver, I1, "no hugepages left on this host");

    cell.destination_listens().await;

    let migration = read_migration(&cell.store, M1).await;
    assert!(
        !migration.status.receiver_ready,
        "a receiver that never bound was reported as listening"
    );
    assert!(migration.status.receiver_url.is_none());
    let host = condition(&migration.status.conditions, "HostActions");
    assert_eq!(host.status, ConditionStatus::False);
    assert!(host.message.contains("hugepages"), "{host:?}");

    // Which is what the source reads, and why it does nothing.
    let pass = cell.source.resync().await;
    assert_eq!(pass, Pass::default(), "{pass:?}");
    assert!(cell.source_vmm.is_running(I1));

    // The host recovers; nobody has to intervene.
    cell.destination_vmm.heal(Fault::PrepareReceiver, I1);
    cell.destination.resync().await;
    assert!(cell.destination_vmm.is_receiving(I1));
    assert!(read_migration(&cell.store, M1).await.status.receiver_ready);
}

#[tokio::test]
async fn an_agent_restarted_mid_migration_finds_its_own_receiver() {
    // The crate's whole recovery model, applied to the one thing that is
    // expensive to do twice. Nothing about the receiver is remembered in this
    // process — its URL and its liveness are read off the machine — so a
    // successor agent must find it rather than bind a second one, and a source
    // that was already told where to send must still be right.
    let cell = two_nodes(MigrationStatus::default()).await;
    cell.destination_listens().await;
    let published = read_migration(&cell.store, M1).await.status.receiver_url;

    let successor = node_agent(
        cell.store.clone(),
        DESTINATION,
        &cell.destination_vmm,
        &cell.destination_datapath,
    );
    let pass = successor.resync().await;

    assert_eq!(
        pass,
        Pass::default(),
        "the successor redid the work: {pass:?}"
    );
    assert_eq!(
        cell.destination_vmm.count(Fault::PrepareReceiver, I1),
        1,
        "a second receiver was started for one guest"
    );
    assert_eq!(
        read_migration(&cell.store, M1).await.status.receiver_url,
        published,
        "the URL the source was told to use changed underneath it"
    );

    // And the migration still completes, driven by the agent that did not start
    // it.
    cell.source.resync().await;
    assert!(cell.destination_vmm.is_running(I1));
}

#[tokio::test]
async fn a_migration_between_two_other_nodes_is_nobody_elses_business() {
    // A node listing a cell's migrations must act on the two that name it and
    // ignore the rest, or every node in the cell would be preparing receivers
    // for every guest that moves.
    let store = store();
    create_port(&store, PORT_A, "10.0.0.5/24", SOURCE).await;
    create_instance(&store, I1, Some(SOURCE), Some(SOURCE), &[PORT_A]).await;
    create_migration(
        &store,
        M1,
        I1,
        "node-c",
        "node-d",
        MigrationStatus::default(),
    )
    .await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let bystander = node_agent(store.clone(), DESTINATION, &vmm, &datapath);

    let pass = bystander.resync().await;

    assert_eq!(pass, Pass::default(), "{pass:?}");
    assert!(!vmm.is_receiving(I1));
    assert_eq!(read_migration(&store, M1).await.status.node, None);
}
