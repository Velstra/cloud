//! One suite, run against every backend.
//!
//! The point of the [`Store`] trait is that a caller cannot tell which backend
//! it has. That is not a claim you can make by reading two files side by side —
//! it is a claim you check by running the same assertions against both, which
//! is what `tests/memory.rs` and `tests/etcd.rs` do with what is in here.
//!
//! Every case takes a cell name and confines itself to that cell's keys, so the
//! suite is safe to run against a store other tests are also using, and safe to
//! run twice against an etcd that remembers the first time. Nothing asserts an
//! absolute revision for the same reason: in etcd the revision counts the whole
//! cluster's history, and only its ordering is contract.

use std::time::Duration;

use velstra_cloud_model::meta::Revision;
use velstra_cloud_store::{
    Entry, Event, Expect, Store, StoreError, WATCH_QUEUE, key_for, prefix_for,
};

/// Long enough that a real network round trip is not mistaken for a missing
/// event, short enough that a genuinely missing one fails the run rather than
/// hanging it.
const ARRIVES: Duration = Duration::from_secs(5);

/// How long silence has to last to count as silence. Only ever used after an
/// event has already been delivered on the same watch, so the stream is known
/// to be live and this is not a race dressed as an assertion.
const SILENCE: Duration = Duration::from_millis(250);

fn key(cell: &str, name: &str) -> String {
    key_for(cell, "instances", name)
}

async fn create(store: &impl Store, cell: &str, name: &str, value: &str) -> Revision {
    store
        .put(&key(cell, name), value.as_bytes().to_vec(), Expect::Absent)
        .await
        .expect("a create of a fresh key")
}

async fn next(rx: &mut tokio::sync::mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(ARRIVES, rx.recv())
        .await
        .expect("a watch delivered nothing before the deadline")
        .expect("the watch closed instead of delivering")
}

pub async fn a_revision_moves_forward_and_never_repeats(store: &impl Store, cell: &str) {
    let a = create(store, cell, "a", "1").await;
    let b = create(store, cell, "b", "1").await;
    assert!(b > a, "two writes shared a revision");
    let d = store.delete(&key(cell, "a"), Expect::Any).await.unwrap();
    assert!(d > b, "a delete did not advance the revision");
}

pub async fn an_object_round_trips_through_create_read_update_delete(
    store: &impl Store,
    cell: &str,
) {
    let created = create(store, cell, "a", "first").await;
    let read = store.get(&key(cell, "a")).await.unwrap().unwrap();
    assert_eq!(
        read,
        Entry {
            key: key(cell, "a"),
            value: b"first".to_vec(),
            revision: created,
        },
        "a read did not return what was written, at the revision it was written"
    );

    let updated = store
        .put(
            &key(cell, "a"),
            b"second".to_vec(),
            Expect::Revision(created),
        )
        .await
        .unwrap();
    assert!(updated > created);
    let read = store.get(&key(cell, "a")).await.unwrap().unwrap();
    assert_eq!(read.value, b"second");
    assert_eq!(
        read.revision, updated,
        "the object's revision is where it last changed"
    );

    store
        .delete(&key(cell, "a"), Expect::Revision(updated))
        .await
        .unwrap();
    assert!(
        store.get(&key(cell, "a")).await.unwrap().is_none(),
        "a deleted object came back"
    );
}

pub async fn a_stale_writer_is_refused_rather_than_winning(store: &impl Store, cell: &str) {
    // Two writers read the same object; one writes; the other still holds what
    // it read. This is the lost update the whole compare-and-swap discipline
    // exists to prevent, and the only interesting thing a second backend could
    // get wrong is which error it reports.
    let first = create(store, cell, "a", "1").await;
    let second = store
        .put(&key(cell, "a"), b"2".to_vec(), Expect::Revision(first))
        .await
        .unwrap();

    let err = store
        .put(&key(cell, "a"), b"3".to_vec(), Expect::Revision(first))
        .await
        .unwrap_err();
    match err {
        StoreError::Conflict {
            key: k,
            expected,
            actual,
        } => {
            assert_eq!(k, key(cell, "a"));
            assert_eq!(expected, first, "the refusal forgot what the writer held");
            assert_eq!(actual, second, "the refusal did not say where to re-read");
        }
        other => panic!("a stale write was refused with {other:?}"),
    }
}

pub async fn creating_twice_is_refused(store: &impl Store, cell: &str) {
    create(store, cell, "a", "1").await;
    let err = store
        .put(&key(cell, "a"), b"2".to_vec(), Expect::Absent)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::Exists { key: k } if *k == key(cell, "a")),
        "a second create was refused with {err:?}"
    );
    assert_eq!(
        store.get(&key(cell, "a")).await.unwrap().unwrap().value,
        b"1",
        "a refused create still overwrote the object"
    );
}

pub async fn writing_to_something_that_is_gone_is_a_conflict(store: &impl Store, cell: &str) {
    // A writer holding a copy of an object that has since been deleted is in
    // the same position as one holding an old copy: what it has is not what is
    // there. Reporting that as a conflict against revision zero, rather than as
    // its own error, keeps the caller's recovery a single path.
    let revision = create(store, cell, "a", "1").await;
    store.delete(&key(cell, "a"), Expect::Any).await.unwrap();
    let err = store
        .put(&key(cell, "a"), b"2".to_vec(), Expect::Revision(revision))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { actual, .. } if actual.0 == 0),
        "a write against a deleted object gave {err:?}"
    );
}

pub async fn deleting_something_that_is_not_there_says_so(store: &impl Store, cell: &str) {
    let err = store
        .delete(&key(cell, "ghost"), Expect::Any)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Missing { .. }),
        "deleting a missing key gave {err:?}"
    );
    // And a revision the caller happens to be holding does not change the
    // answer: gone is gone, and telling it its revision is stale would send it
    // to re-read something that is not there.
    let err = store
        .delete(&key(cell, "ghost"), Expect::Revision(Revision(7)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Missing { .. }),
        "deleting a missing key with a revision gave {err:?}"
    );
}

pub async fn a_stale_delete_is_refused(store: &impl Store, cell: &str) {
    let first = create(store, cell, "a", "1").await;
    store
        .put(&key(cell, "a"), b"2".to_vec(), Expect::Revision(first))
        .await
        .unwrap();
    let err = store
        .delete(&key(cell, "a"), Expect::Revision(first))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict { .. }),
        "a delete of a copy the caller had not seen gave {err:?}"
    );
    assert!(
        store.get(&key(cell, "a")).await.unwrap().is_some(),
        "a refused delete removed the object anyway"
    );
}

pub async fn a_list_is_prefix_scoped_and_ordered(store: &impl Store, cell: &str) {
    create(store, cell, "b", "1").await;
    create(store, cell, "a", "1").await;
    // The sibling collection is the whole point: without the separator in the
    // prefix, `instances` would swallow `instances-archive` and a controller
    // would reconcile objects that are not its own.
    store
        .put(
            &key_for(cell, "instances-archive", "z"),
            b"1".to_vec(),
            Expect::Absent,
        )
        .await
        .unwrap();

    let got: Vec<String> = store
        .list(&prefix_for(cell, "instances"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(got, vec![key(cell, "a"), key(cell, "b")]);
}

pub async fn a_list_returns_the_whole_collection_however_big_it_is(store: &impl Store, cell: &str) {
    // A backend that answers a list in one message has a size at which it stops
    // being able to, and the collection that reaches it is the one nobody
    // tested with. This is more objects than any one answer is asked to carry,
    // so a backend that pages has to page correctly — in order, once each.
    let count = 1100;
    for i in 0..count {
        create(store, cell, &format!("i{i:05}"), "1").await;
    }
    let listed = store.list(&prefix_for(cell, "instances")).await.unwrap();
    assert_eq!(
        listed.len(),
        count,
        "the list stopped short of the collection"
    );
    let keys: Vec<&str> = listed.iter().map(|e| e.key.as_str()).collect();
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "the list came back out of order, or repeated an object across a page"
    );
    assert_eq!(keys.first(), Some(&key(cell, "i00000").as_str()));
    assert_eq!(keys.last(), Some(&key(cell, "i01099").as_str()));
}

pub async fn reading_what_is_not_there_is_not_an_error(store: &impl Store, cell: &str) {
    assert!(store.get(&key(cell, "ghost")).await.unwrap().is_none());
    assert!(
        store
            .list(&prefix_for(cell, "nothing"))
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn a_watch_sees_changes_under_its_prefix_and_no_others(store: &impl Store, cell: &str) {
    let mut rx = store.watch(&prefix_for(cell, "instances"), None);
    create(store, cell, "a", "1").await;
    store
        .put(
            &key_for(cell, "volumes", "v"),
            b"1".to_vec(),
            Expect::Absent,
        )
        .await
        .unwrap();

    let event = next(&mut rx).await;
    assert_eq!(event.key(), key(cell, "a"));
    assert!(
        tokio::time::timeout(SILENCE, rx.recv()).await.is_err(),
        "a watcher was woken for another collection's object"
    );
}

pub async fn a_watch_from_a_past_revision_gets_what_it_missed(store: &impl Store, cell: &str) {
    // This is what makes list-then-watch race-free, and it is the property the
    // whole design rests on: the caller lists at revision R, watches from R,
    // and nothing that happened in between is lost.
    let listed_at = create(store, cell, "a", "1").await;
    create(store, cell, "b", "1").await;

    let mut rx = store.watch(&prefix_for(cell, "instances"), Some(listed_at));
    let replayed = next(&mut rx).await;
    assert_eq!(
        replayed.key(),
        key(cell, "b"),
        "the write the caller missed was not replayed"
    );
    assert!(
        replayed.revision() > listed_at,
        "a revision the caller had already seen was replayed to it"
    );

    // And then it keeps up: replay is not a snapshot, it is the same stream
    // rewound.
    create(store, cell, "c", "1").await;
    assert_eq!(next(&mut rx).await.key(), key(cell, "c"));
}

pub async fn a_delete_tells_watchers_which_key_went(store: &impl Store, cell: &str) {
    let created = create(store, cell, "a", "1").await;
    let mut rx = store.watch(&prefix_for(cell, "instances"), Some(created));
    let deleted_at = store.delete(&key(cell, "a"), Expect::Any).await.unwrap();
    match next(&mut rx).await {
        Event::Delete { key: k, revision } => {
            assert_eq!(k, key(cell, "a"));
            assert_eq!(
                revision, deleted_at,
                "the delete event named the wrong revision"
            );
        }
        other => panic!("a delete arrived as {other:?}"),
    }
}

pub async fn a_watcher_that_stops_reading_is_dropped_rather_than_queued(
    store: &impl Store,
    cell: &str,
) {
    // Unbounded memory in the one process that holds all the state is worse
    // than a controller that has to re-list, so a watcher that falls too far
    // behind is disconnected. The observable form of "disconnected" is that the
    // channel closes: the reader drains what was queued and then gets `None`,
    // rather than the rest of the events arriving late.
    let overflow = WATCH_QUEUE + 8;
    let mut rx = store.watch(&prefix_for(cell, "instances"), None);
    for i in 0..overflow {
        create(store, cell, &format!("k{i:05}"), "1").await;
    }

    let mut delivered = 0;
    while let Ok(Some(_)) = tokio::time::timeout(ARRIVES, rx.recv()).await {
        delivered += 1;
        assert!(
            delivered <= WATCH_QUEUE,
            "a watcher that never read was queued past the bound"
        );
    }
    assert!(
        delivered < overflow,
        "the store held every event for a watcher that had stopped reading"
    );
}

pub async fn a_dropped_watcher_does_not_wedge_the_store(store: &impl Store, cell: &str) {
    {
        let _rx = store.watch(&prefix_for(cell, "instances"), None);
    }
    // The receiver is gone. A store that still tried to hand it events would
    // block here, or grow a queue for a listener that will never read again.
    create(store, cell, "a", "1").await;
    let mut rx = store.watch(&prefix_for(cell, "instances"), None);
    create(store, cell, "b", "1").await;
    assert_eq!(next(&mut rx).await.key(), key(cell, "b"));
}

pub async fn asking_where_the_store_is_does_not_move_it(store: &impl Store, cell: &str) {
    let written = create(store, cell, "a", "1").await;
    let first = store.revision().await.unwrap();
    assert!(
        first >= written,
        "the store reported a revision older than a write it had acknowledged"
    );
    let second = store.revision().await.unwrap();
    assert_eq!(first, second, "reading the revision advanced it");
}

pub async fn paging_walks_the_whole_collection_exactly_once(store: &impl Store, cell: &str) {
    // 250 objects at 40 a page: seven pages, the last one short. The count is
    // deliberately not a multiple of the page size — the multiple is its own
    // case below, because that is where the off-by-one lives.
    let count = 250;
    let size = 40;
    for i in 0..count {
        create(store, cell, &format!("i{i:05}"), "1").await;
    }

    let prefix = prefix_for(cell, "instances");
    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = store
            .list_page(&prefix, after.as_deref(), size)
            .await
            .unwrap();
        pages += 1;
        assert!(
            page.entries.len() <= size,
            "a page came back bigger than the limit asked for"
        );
        assert!(pages <= 20, "paging did not terminate");
        after = page.entries.last().map(|e| e.key.clone());
        seen.extend(page.entries.into_iter().map(|e| e.key));
        if !page.more {
            break;
        }
    }

    assert_eq!(seen.len(), count, "paging lost or repeated objects");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "paging came back out of order, or handed the same object out twice"
    );
    assert_eq!(
        seen.first().map(String::as_str),
        Some(key(cell, "i00000").as_str())
    );
    assert_eq!(
        seen.last().map(String::as_str),
        Some(key(cell, "i00249").as_str())
    );
}

pub async fn a_collection_that_ends_on_a_page_boundary_says_it_is_done(
    store: &impl Store,
    cell: &str,
) {
    // Exactly two pages' worth. A backend that answers `more` by comparing the
    // page against the limit reports a third page here, and the caller makes a
    // round trip for nothing — every single time, for every collection whose
    // size happens to divide.
    for i in 0..20 {
        create(store, cell, &format!("i{i:05}"), "1").await;
    }
    let prefix = prefix_for(cell, "instances");

    let first = store.list_page(&prefix, None, 10).await.unwrap();
    assert_eq!(first.entries.len(), 10);
    assert!(first.more, "there is a second page and the store denied it");

    let last_key = first.entries.last().unwrap().key.clone();
    let second = store.list_page(&prefix, Some(&last_key), 10).await.unwrap();
    assert_eq!(second.entries.len(), 10);
    assert!(
        !second.more,
        "the collection ended exactly on the boundary and the store claimed more"
    );
}

pub async fn a_page_resumes_strictly_after_the_key_it_is_given(store: &impl Store, cell: &str) {
    // Inclusive-vs-exclusive is the bug that hands the same object out on two
    // consecutive pages, and a caller building a map never notices — it just
    // overwrites. A caller counting does.
    for i in 0..5 {
        create(store, cell, &format!("i{i:05}"), "1").await;
    }
    let prefix = prefix_for(cell, "instances");
    let resume = key(cell, "i00002");

    let page = store.list_page(&prefix, Some(&resume), 10).await.unwrap();
    let keys: Vec<&str> = page.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![key(cell, "i00003").as_str(), key(cell, "i00004").as_str()],
        "the resume key was handed out again, or one past it was skipped"
    );
}

pub async fn a_page_stays_inside_its_prefix(store: &impl Store, cell: &str) {
    // The page walks in key order from wherever it is told to start, so nothing
    // but the prefix check stops it walking straight into the next collection.
    // A limit larger than the collection is what exposes that.
    for i in 0..3 {
        create(store, cell, &format!("i{i:05}"), "1").await;
    }
    for i in 0..3 {
        let key = key_for(cell, "volumes", &format!("v{i:05}"));
        store
            .put(&key, b"1".to_vec(), Expect::Absent)
            .await
            .unwrap();
    }

    let page = store
        .list_page(&prefix_for(cell, "instances"), None, 100)
        .await
        .unwrap();
    assert_eq!(
        page.entries.len(),
        3,
        "the page ran past its own collection"
    );
    assert!(!page.more);
    assert!(
        page.entries.iter().all(|e| e.key.contains("/instances/")),
        "a page carried something from another collection: {:?}",
        page.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );
}

pub async fn paging_an_empty_collection_is_one_empty_page(store: &impl Store, cell: &str) {
    let page = store
        .list_page(&prefix_for(cell, "instances"), None, 20)
        .await
        .unwrap();
    assert!(page.entries.is_empty());
    assert!(
        !page.more,
        "an empty collection promised a page that cannot exist"
    );
}
