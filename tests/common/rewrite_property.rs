// The check behind the seeded loop in `tests/rewrite.rs` and the `rewrite` fuzz target, which both include this file, so a crash found by one
// reproduces in the other.

use lopdf::{BytesSource, IndexedReader, RewriteChanges, RewritePolicy};

/// Objects read back for one input, so one hostile file can't take long.
const MAX_COMPARED_OBJECTS: usize = 256;

/// Whatever `data` is: nothing panics, and if a file comes out, it's a good one. It opens without a rescan as one section, and removing a page takes
/// one page off. Returns whether a file came out, so a test can tell that its inputs reach the writer at all.
pub fn check_rewrite_on_any_bytes(data: &[u8]) -> bool {
    let Ok(reader) = IndexedReader::open(BytesSource::from(data.to_vec())) else {
        return false;
    };
    let policy = RewritePolicy::new().remove_signatures();
    let pages = reader.page_count().ok();

    let mut output = Vec::new();
    let written = match RewriteChanges::new(&reader) {
        Ok(changes) => changes.write_unverified_to(policy, &mut output).ok(),
        Err(_) => None,
    };
    let produced = written.is_some();
    if let Some(written) = written {
        // The same check a save makes, which has to pass on whatever the writer finished.
        let source: std::sync::Arc<dyn lopdf::RandomAccessSource> = std::sync::Arc::new(BytesSource::from(output.clone()));
        let (_, after) = written.verify(&reader, source).expect("a finished rewrite fails its own check");
        assert!(!after.index_stats().unwrap().recovered(), "a finished rewrite needs a rescan");
        assert_eq!(after.revisions().unwrap().revisions.len(), 1, "a finished rewrite has more than one revision");
        if let (Some(pages), Ok(now)) = (pages, after.page_count()) {
            assert_eq!(now, pages, "a rewrite with no changes changed the page count");
        }
        // What went in has to come back out. A limit can stop it, on a hostile file, and not what was written being unreadable.
        for id in after.object_ids().into_iter().take(MAX_COMPARED_OBJECTS) {
            match after.resolve_object(id) {
                Ok(_) | Err(lopdf::IndexedReaderError::StreamLimitExceeded { .. }) => {}
                Err(error) => panic!("object {id:?} doesn't read back: {error}"),
            }
        }
    }

    if let Some(pages) = pages.filter(|pages| *pages > 1) {
        let mut output = Vec::new();
        if let Ok(mut changes) = RewriteChanges::new(&reader) {
            changes.remove_page(pages / 2);
            if let Ok(written) = changes.write_unverified_to(policy, &mut output) {
                let source: std::sync::Arc<dyn lopdf::RandomAccessSource> = std::sync::Arc::new(BytesSource::from(output));
                let (_, after) = written.verify(&reader, source).expect("a finished removal fails its own check");
                assert!(!after.index_stats().unwrap().recovered(), "a finished removal needs a rescan");
                if let Ok(now) = after.page_count() {
                    assert!(now < pages, "removing a page left {now} of {pages}");
                }
            }
        }
    }
    produced
}
