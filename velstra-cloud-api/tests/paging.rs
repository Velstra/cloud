//! Answering a collection a page at a time.
//!
//! An unpaged list is what decides how large a cell can get before the API stops
//! being able to answer: `GET .../ports` on a cell of ten thousand builds ten
//! thousand objects, computes every derived field on each, and serialises the
//! lot — to fill a console showing twenty rows.
//!
//! The two things worth testing are not "does it return a page" but:
//!
//! * a walk of the pages sees **every object exactly once**, and
//! * a page is **actually cheaper**, which is measured here rather than argued,
//!   in the style of `scaling.rs` next door.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::json;
use velstra_cloud_api::{
    Api, Filter, Identity, StaticTokenVerifier, TokenVerifier,
    paging::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, PageToken, Paging},
};
use velstra_cloud_model::meta::Revision;
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Page, Store, StoreError};

struct Counting {
    inner: Arc<MemoryStore>,
    entries: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryStore::new()),
            entries: AtomicUsize::new(0),
        })
    }
    fn reset(&self) {
        self.entries.store(0, Ordering::SeqCst);
    }
    fn read(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Store for Counting {
    async fn get(&self, key: &str) -> Result<Option<Entry>, StoreError> {
        let out = self.inner.get(key).await?;
        self.entries
            .fetch_add(out.is_some() as usize, Ordering::SeqCst);
        Ok(out)
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StoreError> {
        let out = self.inner.list(prefix).await?;
        self.entries.fetch_add(out.len(), Ordering::SeqCst);
        Ok(out)
    }
    async fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page, StoreError> {
        let out = self.inner.list_page(prefix, after, limit).await?;
        self.entries.fetch_add(out.entries.len(), Ordering::SeqCst);
        Ok(out)
    }
    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision, StoreError> {
        self.inner.put(key, value, expect).await
    }
    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision, StoreError> {
        self.inner.delete(key, expect).await
    }
    fn watch(&self, prefix: &str, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }
    async fn revision(&self) -> Result<Revision, StoreError> {
        self.inner.revision().await
    }
}

fn who() -> Identity {
    Identity::new("paging-test")
}

/// A document's resource name as one string.
///
/// Stored names are a list of segments rather than a path, so a test that reads
/// `meta.name` as a string sees `null` and only finds out at the `unwrap`.
fn name_of(document: &serde_json::Value) -> String {
    velstra_cloud_wire::joined(&document["meta"]["name"]).expect("a stored object has a name")
}

/// A project with `n` instances in it.
async fn cell_of(n: usize) -> (Arc<Counting>, Api) {
    let counting = Counting::new();
    let store: Arc<dyn Store> = counting.clone();
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api = Api::new(store, "eu-central", "cell-1", verifier)
        .with_cell_admins(vec!["paging-test".into()]);

    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who(),
    )
    .await
    .unwrap();
    for i in 0..n {
        api.create(
            "projects/p1",
            "instances",
            // Zero-padded so name order and creation order agree, which is what
            // makes "exactly once, in order" a statement about paging rather
            // than about how the ids happen to sort.
            &json!({"id": format!("i{i:04}"), "spec": {"vcpus": 1, "memory_mib": 512}}),
            &who(),
        )
        .await
        .unwrap();
    }
    (counting, api)
}

/// Walk the whole collection and collect the names, one page at a time.
async fn walk(api: &Api, size: usize) -> (Vec<String>, usize) {
    let mut names = Vec::new();
    let mut token = None;
    let mut pages = 0;
    loop {
        let paging = Paging {
            size: Some(size),
            token,
        };
        let listing = api
            .list_page_for("projects/p1", "instances", &Filter::none(), &paging, &who())
            .await
            .unwrap();
        pages += 1;
        assert!(pages < 100, "the walk did not terminate");
        assert!(
            listing.items.len() <= size,
            "a page came back bigger than asked for"
        );
        names.extend(listing.items.iter().map(name_of));
        match listing.next_page_token {
            Some(raw) => token = Some(PageToken::decode(&raw).unwrap()),
            None => break,
        }
    }
    (names, pages)
}

#[tokio::test]
async fn a_walk_sees_every_object_exactly_once_and_in_order() {
    let (_, api) = cell_of(25).await;
    let (names, pages) = walk(&api, 10).await;

    assert_eq!(names.len(), 25, "the walk lost or repeated objects");
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 25, "an object came back on two pages");
    assert_eq!(names, sorted, "the walk came back out of order");
    assert_eq!(pages, 3, "25 objects at 10 a page is three pages");
}

#[tokio::test]
async fn a_collection_that_ends_on_a_page_boundary_does_not_promise_another() {
    // 20 at 10 a page. A server that answers "is there more" by comparing the
    // page against the limit says yes here, and every client makes one pointless
    // round trip for every collection whose size happens to divide.
    let (_, api) = cell_of(20).await;
    let (names, pages) = walk(&api, 10).await;
    assert_eq!(names.len(), 20);
    assert_eq!(
        pages, 2,
        "the walk asked for a third page that cannot exist"
    );
}

#[tokio::test]
async fn asking_for_no_page_still_answers_the_whole_collection() {
    // Every caller written before paging existed, and every controller since:
    // an unpaged list must not quietly start returning the first hundred.
    let (_, api) = cell_of(150).await;
    let listing = api
        .list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging::unpaged(),
            &who(),
        )
        .await
        .unwrap();
    assert_eq!(listing.items.len(), 150);
    assert!(
        listing.next_page_token.is_none(),
        "an unpaged answer offered a page token"
    );
}

#[tokio::test]
async fn a_page_is_actually_cheaper_and_the_saving_does_not_shrink_with_the_cell() {
    // The point of the whole exercise, measured. An unpaged list reads the
    // collection; a page reads a page — and crucially the page's cost stays flat
    // as the cell grows, which is the difference between a cell bounded by its
    // store and one bounded by how much the API can serialise.
    //
    // Measured 2026-08-18, as (cell size, objects read for a whole list, objects
    // read for one page of 20):
    //
    //     (40, 40, 20)   (160, 160, 20)   (640, 640, 20)
    //
    // The middle column is the cell; the right one does not move.
    let mut measured = Vec::new();
    for n in [40usize, 160, 640] {
        let (counting, api) = cell_of(n).await;

        counting.reset();
        api.list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging::unpaged(),
            &who(),
        )
        .await
        .unwrap();
        let whole = counting.read();

        counting.reset();
        api.list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging::of(20),
            &who(),
        )
        .await
        .unwrap();
        let page = counting.read();

        measured.push((n, whole, page));
    }

    for (n, whole, page) in &measured {
        assert!(
            whole >= n,
            "a whole listing of {n} read {whole} — the fixture is not doing what it says"
        );
        assert!(
            *page <= 40,
            "a page of 20 in a cell of {n} read {page} objects; it is still reading the cell"
        );
    }
    let first_page_cost = measured[0].2;
    let last_page_cost = measured[2].2;
    assert_eq!(
        first_page_cost, last_page_cost,
        "the cost of one page grew with the cell: {measured:?}"
    );
}

#[tokio::test]
async fn a_token_from_another_collection_is_refused_rather_than_answered() {
    // A token is a position inside one particular walk. Honoured against another
    // collection it does not mean "start there" — it means two walks have been
    // confused, and answering hands back objects nobody asked for in an order
    // that looks deliberate.
    let (_, api) = cell_of(5).await;
    let stolen = PageToken {
        kind: "volumes".into(),
        parent: "projects/p1".into(),
        after: "projects/p1/volumes/v1".into(),
        revision: 1,
    };
    let error = api
        .list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging {
                size: Some(2),
                token: Some(stolen),
            },
            &who(),
        )
        .await
        .expect_err("a token for another collection was honoured");
    assert!(
        error.message.contains("volumes"),
        "the refusal did not say which walk the token belonged to: {}",
        error.message
    );
}

#[tokio::test]
async fn a_token_that_this_api_did_not_issue_is_refused() {
    assert!(PageToken::decode("not-base64-!!").is_err());
    assert!(
        PageToken::decode("a_g_vsb_g8").is_err(),
        "a decodable but meaningless token was accepted"
    );
}

#[tokio::test]
async fn a_token_survives_the_wire_unchanged() {
    let token = PageToken {
        kind: "instances".into(),
        parent: "projects/p1".into(),
        after: "projects/p1/instances/i0042".into(),
        revision: 77,
    };
    let there_and_back = PageToken::decode(&token.encode()).unwrap();
    // Destructured: a field added here and not carried through `encode` would
    // otherwise resume a walk in the wrong place and look like data loss.
    let PageToken {
        kind,
        parent,
        after,
        revision,
    } = &there_and_back;
    assert_eq!(*kind, token.kind);
    assert_eq!(*parent, token.parent);
    assert_eq!(*after, token.after);
    assert_eq!(*revision, token.revision);
}

#[tokio::test]
async fn every_page_of_a_walk_reports_where_the_walk_started() {
    // This is what keeps list-then-watch correct across a paged list: the caller
    // pages to the end, then watches from the revision it was given, and the
    // watch replays everything that happened during the walk. Report each page's
    // own revision and the caller watches from the *end* and silently misses it.
    let (_, api) = cell_of(25).await;
    let first = api
        .list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging::of(10),
            &who(),
        )
        .await
        .unwrap();
    let started_at = first.revision;

    // A write lands mid-walk, exactly as it would in a live cell.
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": "i9999", "spec": {"vcpus": 1, "memory_mib": 512}}),
        &who(),
    )
    .await
    .unwrap();

    let second = api
        .list_page_for(
            "projects/p1",
            "instances",
            &Filter::none(),
            &Paging {
                size: Some(10),
                token: Some(PageToken::decode(&first.next_page_token.unwrap()).unwrap()),
            },
            &who(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.revision, started_at,
        "the second page reported its own revision, so a watch from here would \
         miss everything that happened during the walk"
    );
}

#[tokio::test]
async fn a_page_size_beyond_the_ceiling_is_capped_rather_than_refused() {
    // A caller asking for a million gets a thousand and a token, and their loop
    // still terminates. Refusing would break clients over a number they have no
    // way to know.
    assert_eq!(Paging::of(usize::MAX).resolved_size(), MAX_PAGE_SIZE);
    assert_eq!(Paging::of(0).resolved_size(), DEFAULT_PAGE_SIZE);
    assert_eq!(Paging::unpaged().resolved_size(), DEFAULT_PAGE_SIZE);
    assert_eq!(Paging::of(7).resolved_size(), 7);
}
