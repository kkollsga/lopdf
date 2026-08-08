//! Indexed-reader tests: stats.

use super::*;

#[test]
fn page_map_with_stats_matches_normal_legacy_results_and_walks_once() {
    let reader = open_reader(&generated_page_tree_pdf(3, 999), ResolverLimits::default());
    let legacy_page_map = reader.page_map().unwrap();
    let legacy_stats = reader.index_stats().unwrap();

    PAGE_TREE_WALK_CALLS.with(|calls| calls.set(0));
    let (page_map, stats) = reader.page_map_with_stats().unwrap();
    assert_eq!(PAGE_TREE_WALK_CALLS.with(Cell::get), 1);
    assert_eq!(page_map, legacy_page_map);
    assert_eq!(stats, legacy_stats);

    PAGE_TREE_WALK_CALLS.with(|calls| calls.set(0));
    let _ = reader.page_map().unwrap();
    let _ = reader.index_stats().unwrap();
    assert_eq!(PAGE_TREE_WALK_CALLS.with(Cell::get), 2);
}

#[test]
fn page_map_with_stats_matches_large_and_encrypted_legacy_results() {
    for page_count in [5_000, 10_000] {
        let reader = open_reader(&generated_page_tree_pdf(page_count, -1), ResolverLimits::default());
        assert_page_map_snapshot_matches_legacy(&reader);
    }

    let pdf = encrypted_page_tree_pdf();
    let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
    assert_page_map_snapshot_matches_legacy(&reader);
}

#[test]
fn page_map_with_stats_matches_cycle_depth_and_limit_legacy_results() {
    let reader = open_reader(&cyclic_page_tree_pdf(), ResolverLimits::default());
    assert_page_map_snapshot_matches_legacy(&reader);

    // Over-deep is now a refusal, so the three entry points must agree on the *error*
    // rather than on a truncated page map.
    let reader = open_reader(
        &generated_deep_page_tree_pdf(DEFAULT_PAGE_TREE_DEPTH_LIMIT + 1),
        ResolverLimits::default(),
    );
    assert_page_map_snapshot_error_matches_legacy(&reader);

    let reader = IndexedReader::open_with_options(
        BytesSource::from(generated_page_tree_pdf(2, 2)),
        IndexedReaderOptions {
            max_pages: 1,
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    assert_page_map_snapshot_error_matches_legacy(&reader);
}
