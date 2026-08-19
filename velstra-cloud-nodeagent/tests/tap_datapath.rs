//! [`TapDatapath`] against a real kernel.
//!
//! Creating a tap needs `CAP_NET_ADMIN`, which is why this had no test until
//! there was a way to have it without being root: a user namespace with its own
//! network namespace grants it, over interfaces nothing else can see.
//!
//!     unshare -Urn cargo test -p velstra-cloud-nodeagent --test tap_datapath
//!
//! Without that, every test here **skips** rather than failing. A test that goes
//! red on a developer's laptop for a reason that has nothing to do with the code
//! is a test people learn to ignore, and this one has real work to do: it is the
//! only place the port ↔ tap mapping is exercised against the thing that actually
//! holds it.
//!
//! It also answers the one question the unit tests cannot, and the one the whole
//! design rests on: **can the DHCP responder answer on a tap that has no address
//! of its own?** The responder binds one socket per tap with `SO_BINDTODEVICE`
//! and replies to `255.255.255.255`, and a kernel with no source address on that
//! interface would be entitled to refuse the send. If it does, a guest gets a
//! wire and no address, and the platform's own IPAM never reaches it.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use velstra_cloud_model::resources::{NetworkSpec, PortSpec};
use velstra_cloud_nodeagent::{Datapath, datapath::TapDatapath};

/// What `ip` says about one interface, or `None` if there is no such interface.
fn link(tap: &str) -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["-o", "link", "show", "dev", tap])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Whether this process may make an interface. Asked by trying, because the
/// answer depends on the namespace and not on the uid.
fn may_create() -> bool {
    let probe = "vtprobe0";
    let made = std::process::Command::new("ip")
        .args(["tuntap", "add", "dev", probe, "mode", "tap"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if made {
        let _ = std::process::Command::new("ip")
            .args(["tuntap", "del", "dev", probe, "mode", "tap"])
            .status();
    }
    made
}

macro_rules! needs_cap {
    () => {
        if !may_create() {
            eprintln!(
                "skipped: no CAP_NET_ADMIN here — run under `unshare -Urn` to exercise this"
            );
            return;
        }
    };
}

#[tokio::test]
async fn a_port_gets_a_tap_that_is_up_and_says_what_it_carries() {
    needs_cap!();
    let dp = TapDatapath::new("vt", None);
    let port = "projects/p1/ports/web";

    let tap = dp
        .program(port, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .expect("programming a plain port");

    // The three things the VMM and the DHCP responder each need, and each of
    // them a separate way for this to be quietly broken.
    let shown = link(&tap).unwrap_or_else(|| panic!("{tap} was reported and `ip` cannot see it"));
    // The administrative flag, not `state`: a tap with nothing attached to its
    // file descriptor has no carrier, so a correctly configured one reads `state
    // DOWN` right up until the VMM opens it. Asserting on `state` would have
    // failed on a working tap — and passed on one nobody had brought up.
    assert!(
        shown.split('>').next().unwrap_or("").contains("UP"),
        "{tap} was never brought up, so it carries nothing: {shown}"
    );
    assert!(
        shown.contains(&format!("alias {port}")),
        "the tap does not say which port it carries, so nothing can map it back: {shown}"
    );

    // And the mapping comes back from the kernel, not from this process: a
    // freshly built datapath that has done nothing still finds it.
    let fresh = TapDatapath::new("vt", None);
    let seen = fresh.observe().await.unwrap();
    assert_eq!(
        seen.get(port).map(|p| p.tap.as_str()),
        Some(tap.as_str()),
        "a new datapath did not recognise a tap this machine is carrying: {seen:?}"
    );

    dp.unprogram(port).await.unwrap();
    assert!(link(&tap).is_none(), "{tap} outlived the port");
    assert!(
        TapDatapath::new("vt", None)
            .observe()
            .await
            .unwrap()
            .is_empty(),
        "a removed port is still reported as carried"
    );
}

#[tokio::test]
async fn programming_twice_is_programming_once() {
    needs_cap!();
    // The agent takes a desired map, not a delta, so it asks on every pass. If
    // the second ask failed on "device exists", every port on the node would go
    // to `programmed: false` one pass after it came up.
    let dp = TapDatapath::new("vu", None);
    let port = "projects/p1/ports/twice";
    let first = dp
        .program(port, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .unwrap();
    let second = dp
        .program(port, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .unwrap();
    assert_eq!(first, second);
    dp.unprogram(port).await.unwrap();
    // And unprogramming twice, for the same reason on the other side.
    dp.unprogram(port).await.unwrap();
}

#[tokio::test]
async fn two_ports_on_one_node_get_two_taps() {
    needs_cap!();
    let dp = TapDatapath::new("vw", None);
    let a = "projects/p1/ports/one";
    let b = "projects/p2/ports/two";
    let ta = dp
        .program(a, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .unwrap();
    let tb = dp
        .program(b, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .unwrap();
    assert_ne!(ta, tb);
    let seen = dp.observe().await.unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[a].tap, ta);
    assert_eq!(seen[b].tap, tb);
    dp.unprogram(a).await.unwrap();
    dp.unprogram(b).await.unwrap();
}

/// The question the whole design rests on: a tap with no address of its own, and
/// a broadcast reply that has to leave through it anyway.
///
/// This deliberately builds the socket the way [`velstra_cloud_nodeagent::dhcp`]
/// does rather than calling into it, because what is being asked about is the
/// kernel's answer to that exact combination — bound to a device, broadcast
/// enabled, no source address anywhere on the link.
#[tokio::test]
async fn a_reply_can_leave_a_tap_that_has_no_address() {
    needs_cap!();
    let dp = TapDatapath::new("vx", None);
    let port = "projects/p1/ports/dhcp";
    let tap = dp
        .program(port, &PortSpec::default(), &NetworkSpec::default(), &[])
        .await
        .unwrap();

    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_broadcast(true).unwrap();
    let bound_device = socket.bind_device(Some(tap.as_bytes()));
    let bound = socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 6767)).into());

    let sent = if bound_device.is_ok() && bound.is_ok() {
        socket.send_to(
            b"probe",
            &SocketAddrV4::new(Ipv4Addr::BROADCAST, 6868).into(),
        )
    } else {
        Ok(0)
    };

    dp.unprogram(port).await.unwrap();

    // Reported rather than asserted away: each of these failing means something
    // different, and a plain `unwrap` would name none of them.
    bound_device.expect(
        "SO_BINDTODEVICE was refused — it needs CAP_NET_RAW, which the DHCP responder \
         therefore also needs",
    );
    bound.expect("binding the DHCP port failed");
    let n = sent.expect(
        "a broadcast reply cannot leave a tap with no address on it. The DHCP responder \
         answers 255.255.255.255 on exactly such a tap, so no guest would ever get the \
         address the platform allocated for it",
    );
    assert_eq!(n, 5);
}
