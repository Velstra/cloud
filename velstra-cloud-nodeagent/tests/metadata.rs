//! The metadata service, asked the way a guest asks it.
//!
//! These tests speak HTTP over a real socket from a real source address rather
//! than calling the handlers, because the property under test *is* the source
//! address: a handler test would prove the code branches, not that a guest
//! cannot reach another guest's user-data. The whole of `127.0.0.0/8` is local
//! on Linux, so each "guest" here binds its own loopback address and the server
//! sees exactly what it would see on a node.

mod common;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use velstra_cloud_nodeagent::{FakeDatapath, FakeVmm, metadata};

const I1: &str = "projects/p1/instances/i1";
const I2: &str = "projects/p1/instances/i2";
const PORT_A: &str = "projects/p1/ports/port-a";
const PORT_B: &str = "projects/p1/ports/port-b";

/// One guest asking about itself, from its own address.
async fn ask(server: SocketAddr, from: &str, path: &str) -> (u16, String) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .bind(SocketAddr::new(from.parse::<IpAddr>().unwrap(), 0))
        .unwrap();
    let mut stream = socket.connect(server).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body.to_string())
}

/// A node running two guests on two addresses, with the service up.
async fn two_guests() -> (SocketAddr, Arc<dyn velstra_cloud_store::Store>) {
    let store = store();
    // The subnet these loopback addresses are on, so the guests can be told
    // the shape of their own link rather than only their address.
    create_network(&store, "127.0.0.0/8", "127.0.0.1").await;
    create_port(&store, PORT_A, "127.0.0.2/8", "node-a").await;
    create_port(&store, PORT_B, "127.0.0.3/8", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;
    create_instance(&store, I2, Some("node-a"), Some("node-a"), &[PORT_B]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    // Dropping the handle detaches the task rather than stopping it, which is
    // what these tests want: the service stays up, and the agent's registry
    // with it.
    let (bound, _server) = metadata::serve("127.0.0.1:0".parse().unwrap(), agent.guests())
        .await
        .unwrap();
    (bound, store)
}

#[tokio::test]
async fn a_guest_is_told_who_it_is() {
    let (server, _store) = two_guests().await;

    let (status, body) = ask(server, "127.0.0.2", "/latest/meta-data/instance-id").await;
    assert_eq!(status, 200);
    assert_eq!(body, I1);

    let (_, hostname) = ask(server, "127.0.0.2", "/latest/meta-data/hostname").await;
    assert_eq!(hostname, "i1");

    let (_, key) = ask(
        server,
        "127.0.0.2",
        "/latest/meta-data/public-keys/0/openssh-key",
    )
    .await;
    assert!(key.contains("ssh-ed25519"), "{key:?}");
}

#[tokio::test]
async fn a_guest_is_told_enough_to_bring_its_network_up() {
    // The three things a guest cannot work out for itself and that DHCP would
    // otherwise be the only source of: the size of its link, its gateway and
    // its resolvers.
    let (server, _store) = two_guests().await;

    let (status, address) = ask(server, "127.0.0.2", "/latest/meta-data/local-ipv4").await;
    assert_eq!(status, 200);
    assert_eq!(address, "127.0.0.2");

    let (_, mac) = ask(server, "127.0.0.2", "/latest/meta-data/mac").await;
    assert_eq!(mac, "52:54:00:12:34:56");

    let (status, network) = ask(server, "127.0.0.2", "/network-config").await;
    assert_eq!(status, 200);
    assert!(
        network.contains("macaddress: \"52:54:00:12:34:56\""),
        "{network}"
    );
    assert!(network.contains("- \"127.0.0.2/8\""), "{network}");
    assert!(network.contains("via: \"127.0.0.1\""), "{network}");
    assert!(network.contains("mtu: 1450"), "{network}");

    // …and the same facts through the EC2 shape, which is what an unmodified
    // image asks for without being told to.
    let (_, macs) = ask(
        server,
        "127.0.0.2",
        "/latest/meta-data/network/interfaces/macs",
    )
    .await;
    assert_eq!(macs, "52:54:00:12:34:56/\n");
    let (_, addresses) = ask(
        server,
        "127.0.0.2",
        "/latest/meta-data/network/interfaces/macs/52:54:00:12:34:56/local-ipv4s",
    )
    .await;
    assert_eq!(addresses, "127.0.0.2\n");
    let (_, block) = ask(
        server,
        "127.0.0.2",
        "/latest/meta-data/network/interfaces/macs/52:54:00:12:34:56/subnet-ipv4-cidr-block",
    )
    .await;
    assert_eq!(block, "127.0.0.0/8\n");
}

#[tokio::test]
async fn the_nocloud_and_ec2_answers_are_the_same_facts() {
    // Two shapes, one document underneath. They are served together because
    // the EC2 surface has nowhere to put a gateway, not because there are two
    // sources of truth.
    let (server, _store) = two_guests().await;

    let (status, meta_data) = ask(server, "127.0.0.2", "/meta-data").await;
    assert_eq!(status, 200);
    assert!(
        meta_data.contains(&format!("instance-id: {I1}")),
        "{meta_data}"
    );
    assert!(meta_data.contains("local-hostname: i1"), "{meta_data}");

    let (_, flat) = ask(server, "127.0.0.2", "/user-data").await;
    let (_, ec2) = ask(server, "127.0.0.2", "/latest/user-data").await;
    assert_eq!(flat, ec2);
}

#[tokio::test]
async fn an_instance_can_never_read_another_instances_user_data() {
    // The one that matters. If identity came from anything in the request —
    // a header, a token, a parameter — a guest could ask as its neighbour.
    let (server, _store) = two_guests().await;

    let (status, mine) = ask(server, "127.0.0.2", "/latest/user-data").await;
    assert_eq!(status, 200);
    assert!(mine.contains("i1"), "{mine:?}");

    let (status, theirs) = ask(server, "127.0.0.3", "/latest/user-data").await;
    assert_eq!(status, 200);
    assert!(theirs.contains("i2"), "{theirs:?}");
    assert_ne!(mine, theirs);

    // There is no request a guest can make that asks about the other one: the
    // only thing that selects an answer is the address it came from.
    let (status, _) = ask(server, "127.0.0.4", "/latest/user-data").await;
    assert_eq!(
        status, 404,
        "an address this node does not run got an answer"
    );
    let (status, _) = ask(server, "127.0.0.4", "/latest/meta-data/instance-id").await;
    assert_eq!(status, 404);
    // Nor through the flat paths, which are the same identity check.
    for path in ["/meta-data", "/user-data", "/network-config"] {
        let (status, _) = ask(server, "127.0.0.4", path).await;
        assert_eq!(status, 404, "{path} answered a stranger");
    }
}

#[tokio::test]
async fn naming_a_neighbours_nic_does_not_reach_it() {
    // The MAC in the path selects among the caller's *own* interfaces. Asking
    // with somebody else's is a 404, not a way around the source address.
    let (server, _store) = two_guests().await;
    let (status, _) = ask(
        server,
        "127.0.0.2",
        "/latest/meta-data/network/interfaces/macs/52:54:00:99:99:99/local-ipv4s",
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_guest_that_is_no_longer_here_stops_being_answered_for() {
    // An address is re-used by the next tenant, so an entry that outlives its
    // guest hands somebody else's keys to a stranger.
    let store = store();
    create_network(&store, "127.0.0.0/8", "127.0.0.1").await;
    create_port(&store, PORT_A, "127.0.0.5/8", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    let (server, handle) = metadata::serve("127.0.0.1:0".parse().unwrap(), agent.guests())
        .await
        .unwrap();
    let (status, _) = ask(server, "127.0.0.5", "/latest/user-data").await;
    assert_eq!(status, 200);

    request_delete_instance(&store, I1).await;
    agent.resync().await;

    let (status, _) = ask(server, "127.0.0.5", "/latest/user-data").await;
    assert_eq!(status, 404, "a guest that is gone was still answered for");
    handle.abort();
}

#[tokio::test]
async fn two_guests_on_one_address_are_answered_for_neither() {
    // A datapath that gave two guests one address would otherwise let whichever
    // instance the agent happened to list second inherit the other's identity.
    // The safe answer to an ambiguous address is no answer.
    let store = store();
    create_network(&store, "127.0.0.0/8", "127.0.0.1").await;
    create_port(&store, PORT_A, "127.0.0.7/8", "node-a").await;
    create_port(&store, PORT_B, "127.0.0.7/8", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;
    create_instance(&store, I2, Some("node-a"), Some("node-a"), &[PORT_B]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;

    let (server, handle) = metadata::serve("127.0.0.1:0".parse().unwrap(), agent.guests())
        .await
        .unwrap();
    let (status, _) = ask(server, "127.0.0.7", "/latest/user-data").await;
    assert_eq!(
        status, 404,
        "an ambiguous address was given somebody's user-data"
    );
    handle.abort();
}

#[tokio::test]
async fn the_service_is_the_nodes_own_and_needs_nothing_else_to_be_up() {
    // No store, no controllers, no cell. A node that is up can answer for the
    // guests it is running, which is the whole reason this is not central.
    let registry = velstra_cloud_nodeagent::GuestRegistry::new();
    registry.replace(vec![velstra_cloud_nodeagent::GuestView {
        instance_id: "projects/p1/instances/lonely".into(),
        hostname: "lonely".into(),
        interfaces: vec![velstra_cloud_nodeagent::Interface {
            cidr: Some("127.0.0.6/8".parse().unwrap()),
            ..Default::default()
        }],
        ..Default::default()
    }]);
    let (server, handle) = metadata::serve("127.0.0.1:0".parse().unwrap(), registry)
        .await
        .unwrap();

    let (status, body) = ask(server, "127.0.0.6", "/latest/meta-data/instance-id").await;
    assert_eq!(status, 200);
    assert_eq!(body, "projects/p1/instances/lonely");

    // No user-data is a 404, not an empty file cloud-init would then run.
    let (status, _) = ask(server, "127.0.0.6", "/latest/user-data").await;
    assert_eq!(status, 404);

    handle.abort();
}
