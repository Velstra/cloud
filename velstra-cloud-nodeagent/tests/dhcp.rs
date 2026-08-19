//! DHCP, from the objects a cell actually holds.
//!
//! The unit tests in `src/dhcp.rs` drive [`velstra_cloud_nodeagent::dhcp::answer`]
//! against a registry built by hand. These drive it against a registry built by
//! the agent's own pass over a real store, which is what makes them worth
//! having separately: the thing most likely to be wrong is not the packet
//! encoding but whether the guest a node is running is the guest the responder
//! can find.
//!
//! The packets are bytes, not a live network — there is no tap and no guest
//! here. What *is* real is everything above the socket: the objects, the agent
//! pass, the derivation and the answer.

mod common;

use common::*;
use velstra_cloud_nodeagent::{
    FakeDatapath, FakeVmm, GuestRegistry,
    dhcp::{self, Destination, Ignored, Server},
    metadata,
};

const I1: &str = "projects/p1/instances/i1";
const I2: &str = "projects/p1/instances/i2";
const PORT_A: &str = "projects/p1/ports/port-a";
const PORT_B: &str = "projects/p1/ports/port-b";
/// What [`FakeDatapath`] names the tap for `PORT_A`.
const TAP_A: &str = "vt-port-a";
const TAP_B: &str = "vt-port-b";
const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// A DISCOVER, as a client sends one.
fn discover(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0u8; 240];
    packet[0] = 1; // BOOTREQUEST
    packet[1] = 1; // ethernet
    packet[2] = 6;
    packet[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
    packet[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // cannot unicast yet
    packet[28..34].copy_from_slice(&mac);
    packet[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    packet.extend_from_slice(&[53, 1, 1, 255]); // DHCPDISCOVER, end
    packet
}

/// The value of a DHCP option in a reply.
fn option(reply: &[u8], want: u8) -> Option<Vec<u8>> {
    let mut rest = &reply[240..];
    while let Some((&code, tail)) = rest.split_first() {
        match code {
            0 => rest = tail,
            255 => return None,
            _ => {
                let (&len, tail) = tail.split_first()?;
                let len = len as usize;
                if code == want {
                    return Some(tail[..len].to_vec());
                }
                rest = &tail[len..];
            }
        }
    }
    None
}

fn yiaddr(reply: &[u8]) -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::from(<[u8; 4]>::try_from(&reply[16..20]).unwrap())
}

/// A node running one guest on a real subnet, with the agent's registry built.
async fn a_running_guest() -> (
    std::sync::Arc<dyn velstra_cloud_store::Store>,
    velstra_cloud_nodeagent::Agent,
    GuestRegistry,
) {
    let store = store();
    create_network(&store, "10.20.0.0/24", "10.20.0.1").await;
    create_port(&store, PORT_A, "10.20.0.10", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;
    let guests = agent.guests();
    (store, agent, guests)
}

#[tokio::test]
async fn a_guest_is_offered_exactly_what_its_port_and_subnet_say() {
    let (_store, _agent, guests) = a_running_guest().await;

    let reply = dhcp::answer(TAP_A, &discover(MAC), &guests, &Server::default())
        .expect("the node did not answer the guest it is running");
    assert_eq!(
        yiaddr(&reply.bytes),
        "10.20.0.10".parse::<std::net::Ipv4Addr>().unwrap()
    );
    assert_eq!(option(&reply.bytes, 1).unwrap(), vec![255, 255, 255, 0]);
    assert_eq!(option(&reply.bytes, 3).unwrap(), vec![10, 20, 0, 1]);
    assert_eq!(option(&reply.bytes, 6).unwrap(), vec![10, 20, 0, 1]);
    assert_eq!(option(&reply.bytes, 12).unwrap(), b"i1".to_vec());
    // The MTU comes from the network, which is the only object that knows what
    // the encapsulation costs.
    assert_eq!(
        option(&reply.bytes, 26).unwrap(),
        1450u16.to_be_bytes().to_vec()
    );
    assert_eq!(reply.to, Destination::Broadcast);
}

#[tokio::test]
async fn what_dhcp_hands_out_is_what_the_metadata_service_answers_for() {
    // The property that made both services read one registry. If they derived
    // separately, this is the test that would catch the day they disagreed —
    // and a guest leased an address the metadata service does not recognise is
    // a guest that boots and then cannot ask who it is.
    let (_store, _agent, guests) = a_running_guest().await;

    let reply = dhcp::answer(TAP_A, &discover(MAC), &guests, &Server::default()).unwrap();
    let leased = yiaddr(&reply.bytes);

    let (view, _) = guests
        .at_address(leased.into())
        .expect("the metadata service does not know the address DHCP just handed out");
    assert_eq!(view.instance_id, I1);
    // …and the document it would be sent describes that same address.
    let document = metadata::render_network_config(&view).unwrap();
    assert!(
        document.contains(&format!("- \"{leased}/24\"")),
        "{document}"
    );
}

#[tokio::test]
async fn a_guest_that_is_gone_is_offered_nothing() {
    // The lease is not a record that outlives the port: it is re-derived from
    // the objects on every answer, so a guest on its way out stops being
    // answered on the pass that noticed, with nothing to expire or clean up.
    let (store, agent, guests) = a_running_guest().await;
    assert!(dhcp::answer(TAP_A, &discover(MAC), &guests, &Server::default()).is_ok());

    request_delete_instance(&store, I1).await;
    agent.resync().await;

    assert!(matches!(
        dhcp::answer(TAP_A, &discover(MAC), &guests, &Server::default()),
        Err(Ignored::Unknown { .. })
    ));
}

#[tokio::test]
async fn a_guest_cannot_lease_its_neighbours_address_by_wearing_its_mac() {
    // Two guests on one node, and one of them lying about who it is. The tap it
    // sends from is this node's own doing, so the lie has nowhere to land.
    let store = store();
    create_network(&store, "10.20.0.0/24", "10.20.0.1").await;
    create_port(&store, PORT_A, "10.20.0.10", "node-a").await;
    create_port(&store, PORT_B, "10.20.0.11", "node-a").await;
    create_instance(&store, I1, Some("node-a"), Some("node-a"), &[PORT_A]).await;
    create_instance(&store, I2, Some("node-a"), Some("node-a"), &[PORT_B]).await;

    let vmm = FakeVmm::new();
    let datapath = FakeDatapath::new();
    let agent = node_agent(store.clone(), "node-a", &vmm, &datapath);
    agent.resync().await;
    let guests = agent.guests();

    // Both ports carry the same MAC in this fixture, which is the hardest
    // version of the question: even then, each tap answers only for its own.
    let a = dhcp::answer(TAP_A, &discover(MAC), &guests, &Server::default()).unwrap();
    let b = dhcp::answer(TAP_B, &discover(MAC), &guests, &Server::default()).unwrap();
    assert_eq!(
        yiaddr(&a.bytes),
        "10.20.0.10".parse::<std::net::Ipv4Addr>().unwrap()
    );
    assert_eq!(
        yiaddr(&b.bytes),
        "10.20.0.11".parse::<std::net::Ipv4Addr>().unwrap()
    );

    // A tap this node does not carry is nobody, whatever the packet says.
    assert!(matches!(
        dhcp::answer("vt-elsewhere", &discover(MAC), &guests, &Server::default()),
        Err(Ignored::Unknown { .. })
    ));
}

#[tokio::test]
async fn a_node_answers_only_on_the_taps_of_the_guests_it_runs() {
    let (_store, _agent, guests) = a_running_guest().await;
    assert_eq!(
        guests.taps(),
        std::collections::BTreeSet::from([TAP_A.to_string()])
    );
}
