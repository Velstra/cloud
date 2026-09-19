//! Force two API replicas to validate the same inventory before either commits.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Barrier, mpsc};
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier};
use velstra_cloud_model::meta::Revision;
use velstra_cloud_store::{Admission, Entry, Event, Expect, MemoryStore, Page, Result, Store};

struct Racing {
    inner: MemoryStore,
    armed: AtomicBool,
    barrier: Barrier,
}
#[async_trait]
impl Store for Racing {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.get(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        self.inner.list(prefix).await
    }
    async fn list_page(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Page> {
        self.inner.list_page(prefix, after, limit).await
    }
    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision> {
        self.inner.put(key, value, expect).await
    }
    async fn put_admitted(
        &self,
        key: &str,
        value: Vec<u8>,
        expect: Expect,
        admission: &Admission,
    ) -> Result<Revision> {
        if self.armed.load(Ordering::SeqCst) {
            self.barrier.wait().await;
        }
        self.inner.put_admitted(key, value, expect, admission).await
    }
    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision> {
        self.inner.delete(key, expect).await
    }
    fn watch(&self, prefix: &str, from: Option<Revision>) -> mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }
    async fn revision(&self) -> Result<Revision> {
        self.inner.revision().await
    }
}

#[tokio::test]
async fn two_api_replicas_cannot_both_spend_the_last_volume_of_quota() {
    let store = Arc::new(Racing {
        inner: MemoryStore::new(),
        armed: AtomicBool::new(false),
        barrier: Barrier::new(2),
    });
    let api = || {
        Api::new(
            store.clone(),
            "eu",
            "cell-1",
            Arc::new(StaticTokenVerifier::single("t")),
        )
        .with_cell_admins(vec!["ops".into()])
    };
    let (a, b) = (api(), api());
    let who = Identity::new("ops");
    a.create(
        "",
        "projects",
        &json!({"id":"p1", "spec":{"quota":{"volumes":1}}}),
        &who,
    )
    .await
    .unwrap();
    a.create(
        "",
        "pools",
        &json!({"id":"pool-a", "spec":{"accepting":true}}),
        &who,
    )
    .await
    .unwrap();
    store.armed.store(true, Ordering::SeqCst);
    let one = json!({"id":"one", "spec":{"pool":"pool-a", "size_gib":1}});
    let two = json!({"id":"two", "spec":{"pool":"pool-a", "size_gib":1}});
    let (one, two) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            a.create("projects/p1", "volumes", &one, &who),
            b.create("projects/p1", "volumes", &two, &who)
        )
    })
    .await
    .expect("both requests must reach the atomic commit");
    assert_ne!(one.is_ok(), two.is_ok());
    assert_eq!(store.list("/cell-1/volumes/").await.unwrap().len(), 1);
    store.armed.store(false, Ordering::SeqCst);
    let refused = a
        .create(
            "projects/p1",
            "volumes",
            &json!({"id":"retry", "spec":{"pool":"pool-a", "size_gib":1}}),
            &who,
        )
        .await
        .err()
        .expect("quota must reject the retry");
    assert_eq!(
        refused.code,
        velstra_cloud_api::error::Code::ResourceExhausted
    );
}
