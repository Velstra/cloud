//! The metadata service, on the node that runs the guest.
//!
//! A guest asks `169.254.169.254` who it is. The answer comes from the agent on
//! the same machine, which means its availability is that machine's
//! availability: there is no central metadata service whose outage stops every
//! guest in the region from booting. A node that is up can always answer for
//! the guests it is running, because it is the thing running them.
//!
//! **Identity is the source address, and nothing else.** No token, no header, no
//! query parameter — all of which a guest could forge or a neighbour could
//! replay. The address on the packet is the one thing the node itself assigned,
//! through the port it programmed, so it is the only claim in the request that
//! the answerer already knows to be true. An address the agent has not
//! programmed on this node gets a 404, and one guest asking about another's
//! user-data is not a request this service can express.
//!
//! ## Which shape, and why both
//!
//! The paths are **EC2 IMDS** — `/latest/meta-data/…` and `/latest/user-data`.
//! That is the one surface an unmodified cloud image finds by itself: cloud-init
//! probes `169.254.169.254` on every boot with no kernel command line, no seed
//! ISO and no configuration from the operator, and an image that has never
//! heard of this platform comes up with its hostname, its keys and its
//! user-data.
//!
//! The same document is also served at the three flat **NoCloud** paths
//! (`/meta-data`, `/user-data`, `/network-config`), and the reason is one
//! specific gap rather than a wish to support everything: the EC2 surface has
//! no key for a gateway and no key for a resolver — an AWS guest learns both
//! from DHCP and there is nowhere in that shape to put them. A guest that wants
//! to render its own networking needs them, so it needs the NoCloud
//! `network-config`, which is netplan and can say so.
//!
//! Both renderings are computed from one [`crate::guests::GuestView`] on every
//! request. There is no second copy of the truth to go stale, and no request
//! anywhere in this file whose answer depends on anything but which guest
//! asked.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Router,
    extract::{ConnectInfo, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use velstra_cloud_model::network::format_mac;

use crate::guests::{GuestRegistry, GuestView, Interface};

/// Serve until the returned handle is dropped or aborted.
///
/// The listener is bound before returning and its real address handed back, so
/// a caller (a test, or a node whose link-local address is not up yet) knows
/// exactly where it ended up instead of guessing.
pub async fn serve(
    listen: SocketAddr,
    registry: GuestRegistry,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    let app = router(registry);
    let handle = tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, service).await {
            tracing::error!(error = %e, "the metadata service stopped");
        }
    });
    Ok((bound, handle))
}

fn router(registry: GuestRegistry) -> Router {
    Router::new()
        // EC2 IMDS: what an unmodified image asks for on its own.
        .route("/latest/meta-data", get(index))
        .route("/latest/meta-data/", get(index))
        .route("/latest/meta-data/instance-id", get(instance_id))
        .route("/latest/meta-data/hostname", get(hostname))
        .route("/latest/meta-data/local-hostname", get(hostname))
        .route("/latest/meta-data/local-ipv4", get(local_ipv4))
        .route("/latest/meta-data/mac", get(primary_mac))
        .route("/latest/meta-data/public-keys", get(public_keys))
        .route("/latest/meta-data/public-keys/", get(public_keys))
        .route(
            "/latest/meta-data/public-keys/:index/openssh-key",
            get(openssh_key),
        )
        .route("/latest/meta-data/network/interfaces/macs", get(macs))
        .route("/latest/meta-data/network/interfaces/macs/", get(macs))
        .route(
            "/latest/meta-data/network/interfaces/macs/:mac/:field",
            get(nic_field),
        )
        .route("/latest/user-data", get(user_data))
        // NoCloud: the flat trio, for an image told `ds=nocloud-net`.
        .route("/meta-data", get(nocloud_meta_data))
        .route("/user-data", get(user_data))
        .route("/network-config", get(network_config))
        .fallback(unknown_path)
        .with_state(registry)
}

/// The one place a caller becomes a guest. Everything else in this file goes
/// through it, so there is no handler that can accidentally answer for someone.
fn caller(registry: &GuestRegistry, peer: SocketAddr) -> Option<Arc<GuestView>> {
    registry.at_address(peer.ip()).map(|(view, _)| view)
}

/// Deliberately the same answer for a stranger and for a path that does not
/// exist: nobody learns from here whether an address is in use on this node.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

fn text(body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

async fn index(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(_) => {
            text("instance-id\nhostname\nlocal-hostname\nlocal-ipv4\nmac\nnetwork/\npublic-keys\n")
        }
        None => not_found(),
    }
}

async fn instance_id(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(me) => text(me.instance_id.clone()),
        None => not_found(),
    }
}

async fn hostname(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(me) => text(me.hostname.clone()),
        None => not_found(),
    }
}

/// The address the guest is asking from, which is by construction one of its
/// own. Answered from the request rather than from the first interface: a
/// guest with two NICs asking down one of them means that one.
async fn local_ipv4(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match registry.at_address(peer.ip()) {
        Some((me, n)) => match me.interfaces[n].address() {
            Some(address) => text(address.to_string()),
            None => not_found(),
        },
        None => not_found(),
    }
}

async fn primary_mac(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match registry.at_address(peer.ip()) {
        Some((me, n)) => match me.interfaces[n].mac {
            Some(mac) => text(format_mac(&mac)),
            None => not_found(),
        },
        None => not_found(),
    }
}

async fn public_keys(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        // The index format cloud-init expects: one `n=name` line per key.
        Some(me) => text(
            me.ssh_keys
                .iter()
                .enumerate()
                .map(|(i, _)| format!("{i}=velstra\n"))
                .collect::<String>(),
        ),
        None => not_found(),
    }
}

async fn openssh_key(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(index): Path<usize>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    match me.ssh_keys.get(index) {
        Some(key) => text(format!("{key}\n")),
        None => not_found(),
    }
}

/// The index of this guest's NICs, keyed the way EC2 keys them.
async fn macs(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    text(
        me.interfaces
            .iter()
            .filter_map(|nic| nic.mac)
            .map(|mac| format!("{}/\n", format_mac(&mac)))
            .collect::<String>(),
    )
}

/// One field of one of this guest's NICs.
///
/// A MAC in the path selects only among **this guest's own** interfaces, so it
/// is a way of saying "the other one of mine" and never a way of asking about
/// somebody else's NIC. The identity is still the source address.
async fn nic_field(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((mac, field)): Path<(String, String)>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    let Some(nic) = me
        .interfaces
        .iter()
        .find(|nic| nic.mac.map(|m| format_mac(&m)) == Some(mac.to_lowercase()))
    else {
        return not_found();
    };
    // Each of these keys is about one address family, so a v6 NIC asked for
    // `local-ipv4s` has nothing to say and says nothing — answering with its v6
    // address under a v4 key is how a guest ends up writing one into a field
    // that cannot hold it.
    let Some(cidr) = nic.cidr else {
        return not_found();
    };
    let family = match cidr.address {
        IpAddr::V4(_) => Family::V4,
        IpAddr::V6(_) => Family::V6,
    };
    match (field.as_str(), family) {
        ("mac", _) => text(mac),
        ("local-ipv4s", Family::V4) | ("ipv6s", Family::V6) => text(format!("{}\n", cidr.address)),
        ("subnet-ipv4-cidr-block", Family::V4) | ("subnet-ipv6-cidr-blocks", Family::V6) => {
            text(format!("{}/{}\n", cidr.network(), cidr.prefix_len))
        }
        _ => not_found(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

async fn user_data(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    match &me.user_data {
        Some(data) => text(data.clone()),
        // An instance with no user-data gets the same 404 cloud-init expects,
        // not an empty 200 it would then try to run.
        None => not_found(),
    }
}

async fn nocloud_meta_data(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(me) => text(render_meta_data(&me)),
        None => not_found(),
    }
}

async fn network_config(
    State(registry): State<GuestRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    match render_network_config(&me) {
        // Nothing this node knows about the guest's NICs is worth rendering —
        // no MAC, no address. Better a 404 than a document that would leave
        // cloud-init configuring an interface into nothing.
        None => not_found(),
        Some(document) => text(document),
    }
}

async fn unknown_path() -> Response {
    not_found()
}

// ---- the documents -------------------------------------------------------

/// The NoCloud `meta-data` document.
///
/// Pure, so what a guest is told is a unit test rather than a live boot.
pub fn render_meta_data(view: &GuestView) -> String {
    let mut out = format!(
        "instance-id: {}\nlocal-hostname: {}\nhostname: {}\n",
        view.instance_id, view.hostname, view.hostname
    );
    if !view.ssh_keys.is_empty() {
        out.push_str("public-keys:\n");
        for key in &view.ssh_keys {
            // Quoted, because a key ends in a comment that may contain
            // anything, including a colon.
            out.push_str(&format!("  - \"{}\"\n", key.replace('"', "\\\"")));
        }
    }
    out
}

/// The NoCloud `network-config` document: netplan v2, one entry per NIC.
///
/// **Static addresses rather than `dhcp4: true`**, even though this platform
/// does run DHCP for these guests. The two cannot disagree — both are rendered
/// from the same [`GuestView`] — and stating the addresses means a guest whose
/// DHCP client is slow, disabled or replaced still comes up on the network it
/// was given. The DHCP responder exists for the guest's *first* moments, before
/// it has read any of this.
///
/// Interfaces are matched by MAC rather than by name, because a NIC's name
/// depends on the guest's own udev and its bus order, and being wrong about
/// that means configuring somebody else's interface.
pub fn render_network_config(view: &GuestView) -> Option<String> {
    let usable: Vec<&Interface> = view
        .interfaces
        .iter()
        .filter(|nic| nic.mac.is_some() && nic.cidr.is_some())
        .collect();
    if usable.is_empty() {
        return None;
    }
    let mut out = String::from("version: 2\nethernets:\n");
    // A guest holding a public address defaults out through it. The other way
    // round sends replies from the public address out of a door they cannot
    // return through — the asymmetric-routing bug that looks like a firewall
    // problem for a day. See `velstra_cloud_model::public`.
    let public_default = usable
        .iter()
        .flat_map(|nic| nic.public.iter())
        .next()
        .cloned();
    for (n, nic) in usable.iter().enumerate() {
        let mac = format_mac(&nic.mac.expect("filtered"));
        let cidr = nic.cidr.expect("filtered");
        out.push_str(&format!("  velstra{n}:\n"));
        out.push_str(&format!("    match:\n      macaddress: \"{mac}\"\n"));
        out.push_str("    dhcp4: false\n    dhcp6: false\n");
        out.push_str(&format!(
            "    addresses:\n      - \"{}/{}\"\n",
            cidr.address, cidr.prefix_len
        ));
        // The public addresses this NIC holds, as host routes. A `/32` belongs
        // to no broadcast domain, which is exactly what makes the address
        // correct on whichever machine the guest is running on today.
        for route in &nic.public {
            out.push_str(&format!(
                "      - \"{}/{}\"\n",
                route.address, route.prefix_len
            ));
        }
        if let Some(mtu) = nic.mtu {
            out.push_str(&format!("    mtu: {mtu}\n"));
        }
        // Only the first NIC gets a default route. Two default routes with no
        // metric between them is a guest whose egress depends on which one the
        // kernel happened to install second.
        if n == 0 {
            match (&public_default, nic.gateway) {
                // Out through the public address, and the next hop is on-link
                // and in no subnet — answered by the host itself. That is what
                // frees the address from any L2 segment: nothing has to move a
                // VLAN when the guest migrates.
                (Some(route), _) => {
                    let destination = if route.address.is_ipv4() {
                        "0.0.0.0/0"
                    } else {
                        "::/0"
                    };
                    out.push_str(&format!(
                        "    routes:\n      - to: \"{destination}\"\n        via: \"{}\"\n",
                        route.via
                    ));
                    if route.on_link {
                        out.push_str("        on-link: true\n");
                    }
                }
                (None, Some(gateway)) => {
                    let destination = if gateway.is_ipv4() {
                        "0.0.0.0/0"
                    } else {
                        "::/0"
                    };
                    out.push_str(&format!(
                        "    routes:\n      - to: \"{destination}\"\n        via: \"{gateway}\"\n"
                    ));
                }
                (None, None) => {}
            }
            if !nic.dns.is_empty() {
                out.push_str("    nameservers:\n      addresses:\n");
                for resolver in &nic.dns {
                    out.push_str(&format!("        - \"{resolver}\"\n"));
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::network::{Cidr, parse_mac};

    use super::*;

    fn guest() -> GuestView {
        GuestView {
            instance_id: "projects/p1/instances/i1".into(),
            hostname: "i1".into(),
            ssh_keys: vec!["ssh-ed25519 AAAA user@host".into()],
            user_data: Some("#cloud-config\n".into()),
            interfaces: vec![Interface {
                port: "projects/p1/ports/port-a".into(),
                mac: parse_mac("52:54:00:12:34:56"),
                cidr: Some(Cidr::parse("10.20.0.10/24").unwrap()),
                gateway: Some("10.20.0.1".parse().unwrap()),
                dns: vec!["10.20.0.1".parse().unwrap()],
                mtu: Some(1450),
                tap: Some("vt-port-a".into()),
                public: Vec::new(),
            }],
        }
    }

    #[test]
    fn the_meta_data_document_says_who_the_guest_is() {
        let document = render_meta_data(&guest());
        assert!(
            document.contains("instance-id: projects/p1/instances/i1"),
            "{document}"
        );
        assert!(document.contains("local-hostname: i1"), "{document}");
        assert!(
            document.contains("\"ssh-ed25519 AAAA user@host\""),
            "{document}"
        );
    }

    #[test]
    fn the_network_config_is_netplan_matched_by_mac() {
        let document = render_network_config(&guest()).unwrap();
        assert_eq!(
            document,
            "version: 2\n\
             ethernets:\n  \
               velstra0:\n    \
                 match:\n      \
                   macaddress: \"52:54:00:12:34:56\"\n    \
                 dhcp4: false\n    \
                 dhcp6: false\n    \
                 addresses:\n      \
                   - \"10.20.0.10/24\"\n    \
                 mtu: 1450\n    \
                 routes:\n      \
                   - to: \"0.0.0.0/0\"\n        \
                     via: \"10.20.0.1\"\n    \
                 nameservers:\n      \
                   addresses:\n        \
                     - \"10.20.0.1\"\n"
        );
    }

    #[test]
    fn only_the_first_interface_carries_the_default_route() {
        // Two of them, and the guest's egress becomes a race between whichever
        // the kernel installed last.
        let mut two = guest();
        let mut second = two.interfaces[0].clone();
        second.mac = parse_mac("52:54:00:aa:bb:cc");
        second.cidr = Some(Cidr::parse("10.21.0.10/24").unwrap());
        two.interfaces.push(second);
        let document = render_network_config(&two).unwrap();
        assert_eq!(
            document.matches("to: \"0.0.0.0/0\"").count(),
            1,
            "{document}"
        );
        assert_eq!(document.matches("macaddress").count(), 2, "{document}");
    }

    #[test]
    fn a_v6_only_guest_gets_a_v6_default_route() {
        let mut v6 = guest();
        v6.interfaces[0].cidr = Some(Cidr::parse("fd00:1::10/64").unwrap());
        v6.interfaces[0].gateway = Some("fd00:1::1".parse().unwrap());
        let document = render_network_config(&v6).unwrap();
        assert!(document.contains("to: \"::/0\""), "{document}");
        assert!(document.contains("- \"fd00:1::10/64\""), "{document}");
    }

    #[test]
    fn a_nic_the_platform_cannot_describe_is_left_out_rather_than_half_written() {
        let mut unknown = guest();
        unknown.interfaces[0].cidr = None;
        assert_eq!(render_network_config(&unknown), None);
    }

    /// The whole point of a routed address: the guest is told to configure it.
    ///
    /// Not "the platform knows about it" — the guest's own netplan carries the
    /// address as a host route and defaults out through a next hop that is in
    /// no subnet. Everything else about this feature is bookkeeping around this
    /// document.
    #[test]
    fn a_guest_holding_a_public_address_is_told_to_configure_it() {
        let mut view = guest();
        view.interfaces[0].public = vec![velstra_cloud_model::public::guest_route(
            "203.0.113.7".parse().unwrap(),
        )];
        let rendered = render_network_config(&view).expect("a guest with a NIC gets a document");

        // Both addresses: the tenant one it already had, and the public one it
        // now holds. A `/32` belongs to no broadcast domain, which is what
        // makes it correct on whichever machine is running the guest today.
        assert!(rendered.contains("- \"10.20.0.10/24\""), "{rendered}");
        assert!(rendered.contains("- \"203.0.113.7/32\""), "{rendered}");

        // And it defaults out through the public next hop rather than through
        // the tenant gateway — the other way round is the asymmetric-routing
        // bug that looks like a firewall problem for a day.
        assert!(rendered.contains("via: \"169.254.1.1\""), "{rendered}");
        assert!(rendered.contains("on-link: true"), "{rendered}");
        assert!(
            !rendered.contains("via: \"10.20.0.1\""),
            "the guest still defaults through the tenant gateway: {rendered}"
        );
    }

    /// A guest with no public address is unchanged — the tenant gateway is
    /// still its way out.
    #[test]
    fn a_guest_without_one_still_defaults_through_its_tenant_gateway() {
        let rendered = render_network_config(&guest()).expect("a document");
        assert!(rendered.contains("via: \"10.20.0.1\""), "{rendered}");
        assert!(!rendered.contains("169.254.1.1"), "{rendered}");
    }

    /// A translated address must **not** be configured by the guest: it would
    /// answer ARP for something the edge is also answering for, and the two
    /// would take turns.
    #[test]
    fn a_translated_address_is_never_put_in_the_guest() {
        use velstra_cloud_model::{
            meta::{Meta, Placement, ResourceName},
            public::Delivery,
            resources::{FloatingIp, FloatingIpSpec, FloatingIpStatus, Resource},
        };
        let fip = |id: &str, delivery: Delivery| -> FloatingIp {
            Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("projects/p1/floatingips/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                FloatingIpSpec {
                    subnet: "projects/p1/subnets/public".into(),
                    address: Some("203.0.113.7".into()),
                    port: "projects/p1/ports/port-a".into(),
                    delivery,
                    announce: None,
                },
                FloatingIpStatus::default(),
            )
        };
        let held = crate::guests::public_addresses(&[fip("nat", Delivery::Nat)]);
        assert!(held.is_empty(), "a translated address was handed to the guest");

        let routed = crate::guests::public_addresses(&[fip("routed", Delivery::Routed)]);
        assert_eq!(routed["projects/p1/ports/port-a"].len(), 1);
    }
}
