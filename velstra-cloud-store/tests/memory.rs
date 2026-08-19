//! The conformance suite against the in-process store.
//!
//! It runs here so that a failure against etcd is unambiguous: if the same case
//! passes here and fails there, the difference is in the backend and not in the
//! assertion.

mod conformance;

use velstra_cloud_store::MemoryStore;

/// Each case gets its own store as well as its own cell — the same shape the
/// etcd file uses, where the store is shared and the cell is what separates
/// them.
macro_rules! cases {
    ($($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                conformance::$case(&MemoryStore::new(), stringify!($case)).await;
            }
        )*
    };
}

cases! {
    a_revision_moves_forward_and_never_repeats,
    an_object_round_trips_through_create_read_update_delete,
    a_stale_writer_is_refused_rather_than_winning,
    creating_twice_is_refused,
    writing_to_something_that_is_gone_is_a_conflict,
    deleting_something_that_is_not_there_says_so,
    a_stale_delete_is_refused,
    a_list_is_prefix_scoped_and_ordered,
    a_list_returns_the_whole_collection_however_big_it_is,
    reading_what_is_not_there_is_not_an_error,
    a_watch_sees_changes_under_its_prefix_and_no_others,
    a_watch_from_a_past_revision_gets_what_it_missed,
    a_delete_tells_watchers_which_key_went,
    a_watcher_that_stops_reading_is_dropped_rather_than_queued,
    a_dropped_watcher_does_not_wedge_the_store,
    asking_where_the_store_is_does_not_move_it,
    paging_walks_the_whole_collection_exactly_once,
    a_collection_that_ends_on_a_page_boundary_says_it_is_done,
    a_page_resumes_strictly_after_the_key_it_is_given,
    a_page_stays_inside_its_prefix,
    paging_an_empty_collection_is_one_empty_page,
}
