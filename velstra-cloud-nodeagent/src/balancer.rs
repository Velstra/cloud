//! A load balancer for the datapath that has none.
//!
//! **Why this exists.** A `LoadBalancer` is programmed into the Velstra fabric
//! and nowhere else. On a cell whose datapath is the local bridge — the one box
//! that is the whole cell, the shape the quickstart produces — a balancer got
//! an address, said `NoDataPlane` on itself, and did nothing at all. The object
//! was in the model, on the console, in the API and in the contract, and there
//! was no arrangement of a cell without a fabric in which it worked.
//!
//! So this node balances, in userspace: it holds the VIP on its bridge, accepts
//! connections on the listener's port, and splices each one to a member that
//! answers.
//!
//! ## What it does not do, on purpose
//!
//! **It does not terminate TLS.** The stream is passed through byte for byte,
//! and whichever guest receives it presents the certificate. That is a decision
//! rather than a shortcut: terminating means holding a tenant's private key,
//! and this platform has nowhere safe to hold one — it refuses an
//! `encryptionKey` for exactly that reason and would be contradicting itself
//! here. Passing through is also what a network load balancer *is*; the guest
//! keeps its own certificate and this node never sees the plaintext.
//!
//! **It balances only across members on this node.** Without a fabric there is
//! no path from this machine to a guest on another, so a member elsewhere is
//! one this node could not reach if it tried. A single-box cell — which is what
//! this datapath is for — has all its guests here anyway. On a cell of two such
//! nodes each holds the VIP for its own guests, which is honest and is what the
//! addressing already implies.
//!
//! **It is not a place to put clever routing.** No paths, no headers, no
//! rewriting: it moves bytes. Everything above that needs the connection's
//! contents, and reading those is exactly what passing the stream through
//! avoids.

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use velstra_cloud_model::{
    loadbalancer::{LoadBalancer, Protocol, serving},
    resources::Port,
};

/// One socket this node should be listening on, and where its connections go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// The balancer this serves, by resource name — the identity a status is
    /// reported under and the key that says whether a running task is still
    /// wanted.
    pub balancer: String,
    /// What to bind: the VIP and the listener's port.
    pub at: SocketAddr,
    /// Where a connection may go, in a fixed order so two passes over an
    /// unchanged world plan the same thing.
    pub members: Vec<SocketAddr>,
    /// Send a client back to the member it reached last time.
    ///
    /// The fabric does this by hashing the client's address alone instead of
    /// address-and-port; this does the same, for the same reason — so a service
    /// behaves the same on either datapath and an operator learns one rule.
    pub affinity: bool,
}

/// Which member a client starts at.
///
/// Round robin is the default and is the whole of the algorithm. With affinity
/// the start is a function of the client's address instead of a counter, so the
/// same client lands on the same member for as long as the member list holds
/// still — which is exactly as long as the fabric's hashing holds a client, and
/// exactly as long as anybody should rely on either.
///
/// FNV-1a over the address' octets: the fabric hashes the same bytes the same
/// way, so a service that moves between datapaths moves its clients once rather
/// than reshuffling them on every pass.
fn start_at(service: &Service, from: IpAddr, next: usize) -> usize {
    if !service.affinity {
        return next % service.members.len();
    }
    let mut hash: u32 = 0x811c_9dc5;
    let octets: Vec<u8> = match from {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    for byte in octets {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as usize % service.members.len()
}

/// What this node should serve, given the cell's balancers and the ports it
/// carries.
///
/// **Every input is already known to the pass that calls this.** The balancers
/// come from the cell, the ports from this node's own view, and `answering`
/// from the probe that runs beside it — so the plan is a pure function of a
/// pass, and a test can state exactly what a cell produces.
///
/// A member is included when this node carries its port *and* the port has an
/// address. A member that answers nothing is left out — unless nothing answers
/// at all, in which case every member is kept: a health check that takes the
/// last member out has turned a degraded service into no service, which is the
/// one thing a balancer must never do on its own initiative.
pub fn plan(
    balancers: &[LoadBalancer],
    ports: &BTreeMap<String, Port>,
    mine: &dyn Fn(&Port) -> bool,
    answering: &BTreeMap<String, Vec<u32>>,
) -> Vec<Service> {
    let mut out = Vec::new();
    for balancer in balancers {
        let Some(vip) = balancer
            .spec
            .vip
            .as_ref()
            .and_then(|v| v.parse::<IpAddr>().ok())
        else {
            continue;
        };
        if balancer.meta.deleted_at.is_some() {
            continue;
        }
        for listener in &balancer.spec.listeners {
            // **TCP only, and said out loud.** This balancer accepts a
            // connection and splices it; there is no such thing for UDP, which
            // has no connection to accept. The protocol was not read at all, so
            // a `Udp` listener quietly became a TCP one — and a balancer with
            // both TCP/53 and UDP/53 produced two identical services that the
            // `dedup` below folded into one, so half the ask vanished without a
            // word. A listener this datapath cannot serve is left out; the
            // balancer's condition then names the nodes serving it and does not
            // claim this one.
            if listener.protocol != Protocol::Tcp {
                continue;
            }
            // Zero means "the port the client asked for", which for a
            // passthrough balancer is the listener's own.
            let member_port = if listener.member_port == 0 {
                listener.port
            } else {
                listener.member_port
            };
            // Which members answer on that port, in the vocabulary `serving`
            // speaks: a name and whether it is up.
            let health: Vec<(String, bool)> = balancer
                .spec
                .members
                .iter()
                // Taken out of service on purpose, so not a candidate for a new
                // connection. Unlike a member that fails its health check this
                // is an instruction, not an observation, and `serving`'s rule
                // about never removing the last member does not apply to it: an
                // operator draining every member is asking for the service to
                // stop, and gets that.
                .filter(|name| !balancer.spec.draining.contains(name))
                .map(|name| {
                    let up = answering
                        .get(name)
                        .is_some_and(|ports| ports.contains(&u32::from(member_port)));
                    (name.clone(), up)
                })
                .collect();
            let usable = serving(&health);
            let members: Vec<SocketAddr> = usable
                .iter()
                .filter_map(|name| ports.get(name))
                .filter(|port| mine(port))
                .filter_map(|port| port.spec.address.as_ref()?.parse::<IpAddr>().ok())
                .map(|address| SocketAddr::new(address, member_port))
                .collect();
            if members.is_empty() {
                continue;
            }
            out.push(Service {
                balancer: balancer.meta.name.to_string(),
                at: SocketAddr::new(vip, listener.port),
                members,
                affinity: balancer.spec.session_affinity,
            });
        }
    }
    out.sort_by(|a, b| (a.at, &a.balancer).cmp(&(b.at, &b.balancer)));
    out.dedup();
    out
}

/// A running listener, and the way to stop it.
pub struct Running {
    /// What it was started for. A plan that no longer holds this exact service
    /// stops the task rather than reconfiguring it: a listener is cheap to
    /// replace and a half-updated one is a bug nobody can see.
    pub service: Service,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    pub fn stop(self) {
        self.task.abort();
    }
}

/// Accept on `service.at` and splice each connection to a member.
///
/// **Binds before it returns**, and answers `None` when it could not. The first
/// shape of this bound inside the spawned task, so a listener that could not
/// take its address — the address not on the bridge yet, or a port something
/// else already holds — still counted as running, and the node reported the
/// balancer as served while nothing was listening. A status that claims a
/// service nobody can reach is worse than one that admits there is none.
///
/// Round robin over the members, which is the whole of the algorithm and is
/// stated as such: weights and least-connections are real things and neither is
/// worth pretending to have. The order is the plan's order, so it is stable
/// across passes.
pub async fn start(service: Service) -> Option<Running> {
    let listener = match tokio::net::TcpListener::bind(service.at).await {
        Ok(listener) => listener,
        Err(e) => {
            // Not fatal to the pass: the address may not be on the bridge yet,
            // and the next pass tries again.
            tracing::warn!(at = %service.at, error = %e, "could not hold this balancer's address");
            return None;
        }
    };
    let listening = service.clone();
    let task = tokio::spawn(async move {
        tracing::info!(
            balancer = %listening.balancer, at = %listening.at,
            members = listening.members.len(), "balancing"
        );
        let members = Arc::new(listening.members.clone());
        let mut next = 0usize;
        loop {
            let Ok((client, from)) = listener.accept().await else {
                continue;
            };
            let first = start_at(&listening, from.ip(), next);
            next = next.wrapping_add(1);
            let balancer = listening.balancer.clone();
            let members = members.clone();
            tokio::spawn(async move {
                match dial(&members, first).await {
                    Some((mut backend, _member)) => {
                        let mut client = client;
                        // Bytes, both ways, until either end stops. Whatever
                        // this carries — TLS records included — crosses
                        // untouched.
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
                    }
                    None => {
                        tracing::debug!(%balancer, %from, members = members.len(),
                                        "no member took the connection");
                    }
                }
            });
        }
    });
    Some(Running { service, task })
}

/// Connect to the member at `start_at`, and to each one after it in turn until
/// one takes the connection.
///
/// The first shape gave the client whatever the round robin picked and dropped
/// the connection when that member refused. A pool exists so that one member
/// being down is not an outage; handing a caller a closed socket while a
/// healthy member sits next in the ring is the one thing a balancer must not
/// do. Health checking still belongs elsewhere — this is the connect that
/// happens anyway, and it costs nothing to notice its answer.
async fn dial(
    members: &[SocketAddr],
    start_at: usize,
) -> Option<(tokio::net::TcpStream, SocketAddr)> {
    for step in 0..members.len() {
        let member = members[(start_at + step) % members.len()];
        match tokio::net::TcpStream::connect(member).await {
            Ok(backend) => return Some((backend, member)),
            Err(e) => {
                tracing::debug!(%member, error = %e, "a member did not take the connection");
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        loadbalancer::{Listener, LoadBalancerSpec, LoadBalancerStatus, Protocol},
        meta::{Meta, Placement},
        resources::{PortSpec, PortStatus, Resource},
    };

    use super::*;

    fn port(name: &str, address: &str) -> (String, Port) {
        (
            name.to_string(),
            Resource::new(
                Meta::new(name.parse().unwrap(), Placement::new("eu", "cell-1")),
                PortSpec {
                    address: Some(address.to_string()),
                    ..Default::default()
                },
                PortStatus::default(),
            ),
        )
    }

    fn balancer(listeners: Vec<Listener>, members: Vec<&str>) -> LoadBalancer {
        Resource::new(
            Meta::new(
                "projects/p1/load-balancers/web".parse().unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            LoadBalancerSpec {
                network: "projects/p1/networks/n1".into(),
                subnet: "projects/p1/subnets/s1".into(),
                vip: Some("10.19.136.9".into()),
                listeners,
                members: members.into_iter().map(str::to_string).collect(),
                session_affinity: false,
                draining: Default::default(),
            },
            LoadBalancerStatus::default(),
        )
    }

    fn listener(port: u16, member_port: u16) -> Listener {
        Listener {
            protocol: Protocol::Tcp,
            port,
            member_port,
        }
    }

    fn everything(_: &Port) -> bool {
        true
    }

    /// The ordinary case: one listener, two members, both answering.
    #[test]
    fn a_listener_becomes_one_socket_and_its_members() {
        let ports = BTreeMap::from([
            port("projects/p1/ports/a", "10.19.136.2"),
            port("projects/p1/ports/b", "10.19.136.3"),
        ]);
        let answering = BTreeMap::from([
            ("projects/p1/ports/a".to_string(), vec![8080u32]),
            ("projects/p1/ports/b".to_string(), vec![8080u32]),
        ]);
        let plan = plan(
            &[balancer(
                vec![listener(443, 8080)],
                vec!["projects/p1/ports/a", "projects/p1/ports/b"],
            )],
            &ports,
            &everything,
            &answering,
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].at.to_string(), "10.19.136.9:443");
        assert_eq!(
            plan[0]
                .members
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["10.19.136.2:8080", "10.19.136.3:8080"]
        );
    }

    /// A listener this datapath cannot serve is left out, not served wrongly.
    ///
    /// The protocol was never read. `start` binds a `TcpListener`, so a `Udp`
    /// listener quietly became a TCP one — and TCP/53 beside UDP/53 produced
    /// two identical `Service` values that `dedup` folded into one, so half of
    /// what was asked for disappeared without a word.
    #[test]
    fn a_udp_listener_is_left_out_rather_than_bound_as_tcp() {
        let ports = BTreeMap::from([port("projects/p1/ports/a", "10.19.136.2")]);
        let answering = BTreeMap::from([("projects/p1/ports/a".to_string(), vec![53u32])]);
        let mut both = balancer(
            vec![listener(53, 53), listener(53, 53)],
            vec!["projects/p1/ports/a"],
        );
        both.spec.listeners[1].protocol = Protocol::Udp;

        let served = plan(&[both], &ports, &everything, &answering);
        assert_eq!(
            served.len(),
            1,
            "the UDP listener was either bound as TCP or folded into it: {served:?}"
        );
        assert_eq!(served[0].at.to_string(), "10.19.136.9:53");

        // A balancer that is *only* UDP serves nothing here, rather than
        // serving TCP and reporting itself as done.
        let mut udp_only = balancer(vec![listener(53, 53)], vec!["projects/p1/ports/a"]);
        udp_only.spec.listeners[0].protocol = Protocol::Udp;
        assert!(
            plan(&[udp_only], &ports, &everything, &answering).is_empty(),
            "a UDP-only balancer was served over TCP"
        );
    }

    /// A draining member gets no new connections — and unlike a failed health
    /// check, draining *can* empty the pool: it is an instruction, and an
    /// operator draining everything is asking the service to stop.
    #[test]
    fn draining_takes_a_member_out_of_the_rotation() {
        let ports = BTreeMap::from([
            port("projects/p1/ports/a", "10.19.136.2"),
            port("projects/p1/ports/b", "10.19.136.3"),
        ]);
        let both_up = BTreeMap::from([
            ("projects/p1/ports/a".to_string(), vec![8080u32]),
            ("projects/p1/ports/b".to_string(), vec![8080u32]),
        ]);
        let members = vec!["projects/p1/ports/a", "projects/p1/ports/b"];

        let mut one_draining = balancer(vec![listener(443, 8080)], members.clone());
        one_draining.spec.draining = vec!["projects/p1/ports/a".into()];
        let plan_one = plan(&[one_draining], &ports, &everything, &both_up);
        assert_eq!(
            plan_one[0]
                .members
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["10.19.136.3:8080"],
            "a draining member was still handed new connections"
        );

        let mut all_draining = balancer(vec![listener(443, 8080)], members);
        all_draining.spec.draining =
            vec!["projects/p1/ports/a".into(), "projects/p1/ports/b".into()];
        assert!(
            plan(&[all_draining], &ports, &everything, &both_up).is_empty(),
            "draining every member left the service listening"
        );
    }

    /// Affinity sends a client back where it was. Two connections from one
    /// address pick the same member; the counter that round robin turns is not
    /// consulted.
    #[test]
    fn affinity_pins_a_client_to_one_member() {
        let service = Service {
            balancer: "projects/p1/load-balancers/b".into(),
            at: "10.19.136.9:443".parse().unwrap(),
            members: vec![
                "10.19.136.2:8080".parse().unwrap(),
                "10.19.136.3:8080".parse().unwrap(),
            ],
            affinity: true,
        };
        let client: IpAddr = "10.19.136.50".parse().unwrap();
        let first = start_at(&service, client, 0);
        assert_eq!(
            first,
            start_at(&service, client, 7),
            "the same client was sent to a different member on its next connection"
        );

        // And it is a real spread, not everybody on one member: some address
        // has to land on the other one or affinity is just "member zero".
        let elsewhere = (1u8..=64)
            .map(|i| IpAddr::from([10, 19, 136, i]))
            .any(|a| start_at(&service, a, 0) != first);
        assert!(elsewhere, "every client hashed to the same member");

        let round_robin = Service {
            affinity: false,
            ..service
        };
        assert_eq!(start_at(&round_robin, client, 0), 0);
        assert_eq!(start_at(&round_robin, client, 1), 1);
    }

    /// A member that answers nothing is left out — and when *nothing* answers,
    /// every member is kept. A health check that takes the last member away has
    /// turned a degraded service into no service.
    #[test]
    fn health_narrows_the_pool_but_never_empties_it() {
        let ports = BTreeMap::from([
            port("projects/p1/ports/a", "10.19.136.2"),
            port("projects/p1/ports/b", "10.19.136.3"),
        ]);
        let one_up = BTreeMap::from([("projects/p1/ports/a".to_string(), vec![8080u32])]);
        let plan_one = plan(
            &[balancer(
                vec![listener(443, 8080)],
                vec!["projects/p1/ports/a", "projects/p1/ports/b"],
            )],
            &ports,
            &everything,
            &one_up,
        );
        assert_eq!(
            plan_one[0]
                .members
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["10.19.136.2:8080"],
            "a member that answers nothing was still sent traffic"
        );

        let none_up = BTreeMap::new();
        let plan_none = plan(
            &[balancer(
                vec![listener(443, 8080)],
                vec!["projects/p1/ports/a", "projects/p1/ports/b"],
            )],
            &ports,
            &everything,
            &none_up,
        );
        assert_eq!(
            plan_none[0].members.len(),
            2,
            "the health check emptied the pool"
        );
    }

    /// `memberPort: 0` is "the port the client asked for", which for a
    /// passthrough balancer is the listener's own.
    #[test]
    fn a_member_port_of_zero_is_the_listeners_own() {
        let ports = BTreeMap::from([port("projects/p1/ports/a", "10.19.136.2")]);
        let answering = BTreeMap::from([("projects/p1/ports/a".to_string(), vec![443u32])]);
        let plan = plan(
            &[balancer(
                vec![listener(443, 0)],
                vec!["projects/p1/ports/a"],
            )],
            &ports,
            &everything,
            &answering,
        );
        assert_eq!(plan[0].members[0].to_string(), "10.19.136.2:443");
    }

    /// A member this node does not carry is not a member this node can reach:
    /// without a fabric there is no path from here to a guest on another
    /// machine, and pretending otherwise is a listener that accepts and then
    /// fails every connection.
    #[test]
    fn a_member_on_another_node_is_left_out() {
        let ports = BTreeMap::from([
            port("projects/p1/ports/here", "10.19.136.2"),
            port("projects/p1/ports/elsewhere", "10.19.136.3"),
        ]);
        let answering = BTreeMap::from([
            ("projects/p1/ports/here".to_string(), vec![8080u32]),
            ("projects/p1/ports/elsewhere".to_string(), vec![8080u32]),
        ]);
        let only_here = |p: &Port| p.meta.name.to_string().ends_with("/here");
        let plan = plan(
            &[balancer(
                vec![listener(443, 8080)],
                vec!["projects/p1/ports/here", "projects/p1/ports/elsewhere"],
            )],
            &ports,
            &only_here,
            &answering,
        );
        assert_eq!(
            plan[0]
                .members
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["10.19.136.2:8080"]
        );
    }

    /// A balancer with no reachable member is not a socket to open. Binding one
    /// would accept connections and refuse every one of them, which reads to a
    /// client as a service that is up and broken rather than one that is not
    /// here.
    #[test]
    fn a_balancer_with_nothing_behind_it_is_not_bound() {
        let plan = plan(
            &[balancer(
                vec![listener(443, 8080)],
                vec!["projects/p1/ports/gone"],
            )],
            &BTreeMap::new(),
            &everything,
            &BTreeMap::new(),
        );
        assert!(plan.is_empty());
    }

    /// One being deleted is not one to serve.
    #[test]
    fn a_balancer_on_its_way_out_is_not_served() {
        let ports = BTreeMap::from([port("projects/p1/ports/a", "10.19.136.2")]);
        let mut going = balancer(vec![listener(443, 8080)], vec!["projects/p1/ports/a"]);
        going.meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp(1));
        assert!(plan(&[going], &ports, &everything, &BTreeMap::new()).is_empty());
    }

    /// A member that refuses is skipped, not handed to the caller as an outage.
    #[tokio::test]
    async fn a_refusing_member_passes_the_connection_on() {
        let up = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback has ports");
        let alive = up.local_addr().expect("a bound listener has an address");
        // A port nothing holds: bind one, read its address, drop it.
        let dead = {
            let taken = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the loopback has ports");
            taken.local_addr().expect("a bound listener has an address")
        };
        let members = vec![dead, alive];
        let (_stream, took) = dial(&members, 0).await.expect("one member is up");
        assert_eq!(took, alive);
    }

    /// Every member refusing is the one case where there is nothing to hand
    /// back — and it says so rather than hanging.
    #[tokio::test]
    async fn a_pool_that_is_all_down_answers_nothing() {
        let dead = {
            let taken = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the loopback has ports");
            taken.local_addr().expect("a bound listener has an address")
        };
        assert!(dial(&[dead], 0).await.is_none());
    }

    #[tokio::test]
    async fn an_address_something_else_holds_starts_nothing() {
        // The first shape of `start` bound inside the spawned task, so a
        // listener that lost the race for its address still counted as
        // running and the node reported the balancer as served. Nothing was
        // listening. The bind now happens before `start` answers.
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback has ports");
        let at = taken.local_addr().expect("a bound listener has an address");
        let service = Service {
            balancer: "projects/p/load-balancers/b".into(),
            at,
            members: vec![at],
            affinity: false,
        };
        assert!(start(service).await.is_none());
    }

    /// Two passes over an unchanged world plan the same thing, in the same
    /// order — which is what lets the agent tell "nothing to do" from
    /// "something moved".
    #[test]
    fn the_plan_is_stable() {
        let ports = BTreeMap::from([
            port("projects/p1/ports/a", "10.19.136.2"),
            port("projects/p1/ports/b", "10.19.136.3"),
        ]);
        let answering = BTreeMap::from([
            ("projects/p1/ports/a".to_string(), vec![8080u32]),
            ("projects/p1/ports/b".to_string(), vec![8080u32]),
        ]);
        let make = || {
            plan(
                &[balancer(
                    vec![listener(443, 8080), listener(80, 8080)],
                    vec!["projects/p1/ports/b", "projects/p1/ports/a"],
                )],
                &ports,
                &everything,
                &answering,
            )
        };
        assert_eq!(make(), make());
        // Sorted by socket, so the list reads the way somebody would say it.
        assert_eq!(make()[0].at.port(), 80);
    }
}
