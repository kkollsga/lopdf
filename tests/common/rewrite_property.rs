// The check behind the seeded loop in `tests/rewrite.rs` and the `rewrite` fuzz target, which both include this file, so a crash found by one
// reproduces in the other.

use lopdf::{BytesSource, IndexedReader, RewriteChanges, RewritePolicy};

/// Whatever `data` is: nothing panics, and if a file comes out, it's a good one. It opens without a rescan as one section, and removing a page takes
/// one page off. Returns whether a file came out, so a test can tell that its inputs reach the writer at all.
pub fn check_rewrite_on_any_bytes(data: &[u8]) -> bool {
    let Ok(reader) = IndexedReader::open(BytesSource::from(data.to_vec())) else {
        return false;
    };
    let policy = RewritePolicy::new().allow_breaking_signatures();
    let pages = reader.page_count().ok();

    let mut output = Vec::new();
    let produced = match RewriteChanges::new(&reader) {
        Ok(changes) => changes.write_to(policy, &mut output).is_ok(),
        Err(_) => false,
    };
    if produced {
        let after = IndexedReader::open(BytesSource::from(output.clone())).expect("a finished rewrite doesn't open");
        assert!(!after.index_stats().unwrap().recovered(), "a finished rewrite needs a rescan");
        assert_eq!(after.revisions().unwrap().revisions.len(), 1, "a finished rewrite has more than one revision");
        if let (Some(pages), Ok(now)) = (pages, after.page_count()) {
            assert_eq!(now, pages, "a rewrite with no changes changed the page count");
        }
    }

    if let Some(pages) = pages.filter(|pages| *pages > 1) {
        let mut output = Vec::new();
        if let Ok(mut changes) = RewriteChanges::new(&reader) {
            changes.remove_page(pages / 2);
            if changes.write_to(policy, &mut output).is_ok() {
                let after = IndexedReader::open(BytesSource::from(output)).expect("a finished removal doesn't open");
                assert!(!after.index_stats().unwrap().recovered(), "a finished removal needs a rescan");
                if let Ok(now) = after.page_count() {
                    assert!(now < pages, "removing a page left {now} of {pages}");
                }
            }
        }
    }
    produced
}
