//! Load balancers through the API: what is accepted, what is refused, and in
//! which words.
//!
//! The same two-tenant cell as `authz.rs`, because the questions worth asking
//! about a new kind are the ones that decide whether the platform stayed
//! multi-tenant when it grew: may a tenant front another tenant's ports, does
//! the quota hold at the limit, and does a refusal land on the field a person
//! can act on.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Code, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::meta::ResourceName;
use velstra_cloud_store::{MemoryStore, Store};

const OPERATOR: &str = "ops";
const ADA: &str = "ada";
const BOB: &str = "bob";

fn who(subject: &str) -> Identity {
    Identity::new(subject)
}

fn name(text: &str) -> ResourceName {
    ResourceName::parse(text).unwrap()
}

/// Two tenants: ada admins `p1`, bob admins `p2`, and neither is an operator.
async fn cell() -> Api {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec![OPERATOR.to_string()]);
    for (project, admin) in [("p1", ADA), ("p2", BOB)] {
        api.create(
            "",
            "projects",
            &json!({
                "id": project,
                "spec": {
                    "quota": {},
                    "bindings": [{"role": "admin", "members": [admin]}]
                }
            }),
            &who(OPERATOR),
        )
        .await
        .expect("an operator creates a project");
    }
    api
}

/// A network, a subnet and a port in `project`, so a load balancer has
/// something real to name.
async fn plumbing(api: &Api, project: &str, admin: &str) {
    let parent = format!("projects/{project}");
    api.create(
        &parent,
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(admin),
    )
    .await
    .expect("a network");
    api.create(
        &parent,
        "subnets",
        &json!({"id": "s1", "spec": {
            "network": format!("projects/{project}/networks/n1"),
            "cidr": "10.20.0.0/24",
            "gateway": "10.20.0.1"
        }}),
        &who(admin),
    )
    .await
    .expect("a subnet");
    api.create(
        &parent,
        "ports",
        &json!({"id": "web", "spec": {
            "network": format!("projects/{project}/networks/n1"),
            "subnet": format!("projects/{project}/subnets/s1")
        }}),
        &who(admin),
    )
    .await
    .expect("a port");
}

fn lb_spec(project: &str) -> serde_json::Value {
    json!({
        "network": format!("projects/{project}/networks/n1"),
        "subnet": format!("projects/{project}/subnets/s1"),
        "listeners": [{"protocol": "Tcp", "port": 443, "member_port": 8080}],
        "members": [format!("projects/{project}/ports/web")]
    })
}

#[tokio::test]
async fn a_load_balancer_is_created_changed_and_deleted_by_its_own_tenant() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;

    let created = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": lb_spec("p1")}),
            &who(ADA),
        )
        .await
        .expect("ada creates a load balancer in her own project");
    assert_eq!(created.target, "projects/p1/load-balancers/web");

    let read = api
        .get(&name("projects/p1/load-balancers/web"), &who(ADA))
        .await
        .unwrap();
    assert_eq!(read["spec"]["listeners"][0]["port"], 443);
    assert!(
        read["spec"].get("vip").is_none(),
        "no controller runs in this test, so nothing has decided a VIP yet — \
         the API must not have invented one"
    );

    // A change to the pool is an ordinary spec write, and the generation moves
    // with it.
    let before = read["meta"]["generation"].as_u64().unwrap();
    let changed = api
        .patch(
            &name("projects/p1/load-balancers/web"),
            &json!({"spec": {"members": []}}),
            None,
            &who(ADA),
        )
        .await
        .expect("draining the pool is a legitimate change");
    assert_eq!(changed["meta"]["generation"].as_u64().unwrap(), before + 1);

    api.delete(&name("projects/p1/load-balancers/web"), None, &who(ADA))
        .await
        .expect("ada deletes her own load balancer");
}

#[tokio::test]
async fn a_listener_that_cannot_mean_what_it_says_is_refused_on_its_index() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;

    let mut spec = lb_spec("p1");
    spec["listeners"] = json!([{"protocol": "Tcp", "port": 0}]);
    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("a listener on port zero was stored");
    assert_eq!(refused.field.as_deref(), Some("spec.listeners[0]"));

    let mut spec = lb_spec("p1");
    spec["listeners"] = json!([
        {"protocol": "Tcp", "port": 443},
        {"protocol": "Tcp", "port": 443, "member_port": 9}
    ]);
    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("two listeners on one protocol/port were stored");
    assert_eq!(
        refused.field.as_deref(),
        Some("spec.listeners[1]"),
        "the second claim is the one a person would remove: {}",
        refused.message
    );
    assert!(refused.message.contains("tcp/443"), "{}", refused.message);
}

#[tokio::test]
async fn the_quota_holds_at_the_limit_and_names_the_dimension() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;
    api.patch(
        &name("projects/p1"),
        &json!({"spec": {"quota": {"load_balancers": 1}}}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    api.create(
        "projects/p1",
        "load-balancers",
        &json!({"id": "one", "spec": lb_spec("p1")}),
        &who(ADA),
    )
    .await
    .expect("the first load balancer is within the limit");
    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "two", "spec": lb_spec("p1")}),
            &who(ADA),
        )
        .await
        .err()
        .expect("a second load balancer past the limit was admitted");
    assert_eq!(refused.code, Code::ResourceExhausted, "{}", refused.message);
    assert!(
        refused.message.contains("load balancers"),
        "{}",
        refused.message
    );
}

#[tokio::test]
async fn a_tenant_may_not_front_another_tenants_objects() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;
    plumbing(&api, "p2", BOB).await;

    // ada's load balancer, bob's port in the pool. The refusal must be the
    // same one she gets for a port that is not there, so it is not an oracle
    // for what bob has.
    let mut spec = lb_spec("p1");
    spec["members"] = json!(["projects/p2/ports/web"]);
    let real = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("ada balanced across bob's port");
    assert_eq!(real.code, Code::PermissionDenied);

    let mut spec = lb_spec("p1");
    spec["members"] = json!(["projects/p2/ports/no-such-port"]);
    let absent = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("ada named a port that does not exist in bob's project");
    assert_eq!(
        real.message, absent.message,
        "the two refusals can be told apart, which enumerates the other tenant"
    );

    // The same wall for the network and the subnet the VIP would come from.
    let mut spec = lb_spec("p1");
    spec["subnet"] = json!("projects/p2/subnets/s1");
    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("ada drew an address out of bob's subnet");
    assert_eq!(refused.code, Code::PermissionDenied);

    // And bob may not read, change or delete ada's load balancer at all.
    plumbing_free_create(&api).await;
    let read = api
        .get(&name("projects/p1/load-balancers/web"), &who(BOB))
        .await
        .expect_err("bob read ada's load balancer");
    assert_eq!(read.code, Code::PermissionDenied);
    let write = api
        .patch(
            &name("projects/p1/load-balancers/web"),
            &json!({"spec": {"members": []}}),
            None,
            &who(BOB),
        )
        .await
        .expect_err("bob changed ada's load balancer");
    assert_eq!(write.code, Code::PermissionDenied);
    let delete = api
        .delete(&name("projects/p1/load-balancers/web"), None, &who(BOB))
        .await
        .err()
        .expect("bob deleted ada's load balancer");
    assert_eq!(delete.code, Code::PermissionDenied);
}

/// The load balancer the cross-tenant reads above are aimed at.
async fn plumbing_free_create(api: &Api) {
    api.create(
        "projects/p1",
        "load-balancers",
        &json!({"id": "web", "spec": lb_spec("p1")}),
        &who(ADA),
    )
    .await
    .expect("ada creates a load balancer in her own project");
}

#[tokio::test]
async fn status_is_not_a_clients_to_write() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;

    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({
                "id": "web",
                "spec": lb_spec("p1"),
                // A client claiming the fabric already serves the address.
                "status": {"vip": "10.20.0.20"}
            }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a create carried a status and was stored");
    assert_eq!(refused.field.as_deref(), Some("status"));
}

#[tokio::test]
async fn a_port_a_pool_still_names_cannot_be_deleted_out_from_under_it() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;
    plumbing_free_create(&api).await;

    let refused = api
        .delete(&name("projects/p1/ports/web"), None, &who(ADA))
        .await
        .err()
        .expect("a port in a load balancer's pool was deleted");
    assert_eq!(refused.code, Code::FailedPrecondition);
    assert!(
        refused.message.contains("projects/p1/load-balancers/web"),
        "the refusal does not say who holds the port: {}",
        refused.message
    );

    // Take it out of the pool and the port may go.
    api.patch(
        &name("projects/p1/load-balancers/web"),
        &json!({"spec": {"members": []}}),
        None,
        &who(ADA),
    )
    .await
    .unwrap();
    api.delete(&name("projects/p1/ports/web"), None, &who(ADA))
        .await
        .expect("a port nothing names any more may be deleted");
}

#[tokio::test]
async fn a_member_is_a_full_resource_name_because_something_has_to_follow_it() {
    let api = cell().await;
    plumbing(&api, "p1", ADA).await;

    let mut spec = lb_spec("p1");
    spec["members"] = json!(["web"]);
    let refused = api
        .create(
            "projects/p1",
            "load-balancers",
            &json!({"id": "web", "spec": spec}),
            &who(ADA),
        )
        .await
        .err()
        .expect("a bare id was accepted where a resource name belongs");
    assert_eq!(refused.field.as_deref(), Some("spec.members[0]"));
}
