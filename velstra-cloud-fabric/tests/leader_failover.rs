//! Cross-repository wire test: run with a built Fabric controller binary.

use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use velstra_cloud_fabric::{
    connect,
    pb::{
        Action, LeaderRequest, NetworkSpec, velstra_orchestrator_client::VelstraOrchestratorClient,
    },
};

struct Controller(Child);
impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_ports() -> [u16; 9] {
    let sockets: Vec<_> = (0..9)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports = std::array::from_fn(|i| sockets[i].local_addr().unwrap().port());
    drop(sockets);
    ports
}

async fn leader(admin: &[u16; 3], excluded: Option<usize>) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut leaders = Vec::new();
        for (i, port) in admin.iter().enumerate() {
            if excluded == Some(i) {
                continue;
            }
            let url = format!("http://127.0.0.1:{port}");
            if let Ok(mut client) = VelstraOrchestratorClient::connect(url).await
                && let Ok(answer) = client.get_leader(LeaderRequest {}).await
                && answer.get_ref().leader
            {
                leaders.push(i);
            }
        }
        if leaders.len() == 1 {
            return leaders[0];
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no single Fabric write leader became reachable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VELSTRA_FABRIC_CONTROLLER_BIN pointing to a built Fabric controller"]
async fn cloud_selects_the_new_leader_after_fabric_failover() {
    let binary = std::env::var("VELSTRA_FABRIC_CONTROLLER_BIN")
        .expect("set VELSTRA_FABRIC_CONTROLLER_BIN for this cross-repository test");
    let ports = free_ports();
    let raft = [ports[0], ports[1], ports[2]];
    let agent = [ports[3], ports[4], ports[5]];
    let admin = [ports[6], ports[7], ports[8]];
    let mut children = Vec::new();
    for i in [1, 2, 0] {
        let mut command = Command::new(&binary);
        command
            .args([
                "serve",
                "--node-id",
                &(i + 1).to_string(),
                "--raft-listen",
                &format!("127.0.0.1:{}", raft[i]),
                "--listen",
                &format!("127.0.0.1:{}", agent[i]),
                "--admin-listen",
                &format!("127.0.0.1:{}", admin[i]),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if i == 0 {
            command.arg("--bootstrap");
            for (n, port) in raft.iter().enumerate() {
                command.args(["--peer", &format!("{}=127.0.0.1:{port}", n + 1)]);
            }
        }
        children.push((
            i,
            Controller(command.spawn().expect("spawn Fabric controller")),
        ));
    }

    let first = leader(&admin, None).await;
    let other = (0..3).find(|i| *i != first).unwrap();
    let third = (0..3).find(|i| *i != first && *i != other).unwrap();
    let endpoints = format!(
        "http://127.0.0.1:{},http://127.0.0.1:{},http://127.0.0.1:{}",
        admin[other], admin[first], admin[third]
    );
    let mut cloud = connect(&endpoints).await.expect("discover initial leader");
    cloud
        .add_network(NetworkSpec {
            vni: 4101,
            name: "cloud-before-failover".into(),
            subnet: "10.41.0.0/24".into(),
            default_action: Action::Drop as i32,
            drop_icmp: false,
        })
        .await
        .expect("write through selected leader");

    let (_, mut failed) = children.remove(children.iter().position(|(i, _)| *i == first).unwrap());
    failed.0.kill().unwrap();
    failed.0.wait().unwrap();
    assert_ne!(leader(&admin, Some(first)).await, first);
    let mut cloud = connect(&endpoints)
        .await
        .expect("discover replacement leader");
    cloud
        .add_network(NetworkSpec {
            vni: 4102,
            name: "cloud-after-failover".into(),
            subnet: "10.42.0.0/24".into(),
            default_action: Action::Drop as i32,
            drop_icmp: false,
        })
        .await
        .expect("write through replacement leader");
}
