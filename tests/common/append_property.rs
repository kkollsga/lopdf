// The check behind the seeded loop in `tests/append_writer.rs` and the `append` fuzz target, which
// both include this file, so a crash found by one reproduces in the other.

use lopdf::{AppendError, AppendWriter, BytesSource, IndexedReader, Object, RandomAccessSource, dictionary};
use std::sync::Arc;

/// Objects compared across the append for one input, so one hostile file can't take long.
const MAX_COMPARED_OBJECTS: usize = 64;

/// Whatever `data` is: nothing panics, and if an update comes out, it's a good one. The original
/// bytes are a prefix of it, it opens without a rescan, the new object reads back, and every old
/// object that could be read still reads the same. Returns whether an update came out, so a test can
/// tell that its inputs reach the writer at all.
pub fn check_append_on_any_bytes(data: &[u8]) -> bool {
    let Ok(reader) = IndexedReader::open(BytesSource::from(data.to_vec())) else {
        return false;
    };
    let _ = reader.revisions();
    let mut output = Vec::new();
    let written = (|| {
        let mut writer = AppendWriter::new(&reader, &mut output)?;
        let added = writer.add_object(&Object::Dictionary(dictionary! { "Fuzz" => 1 }))?;
        let summary = writer.finish()?;
        Ok::<_, AppendError>((added, summary))
    })();
    let Ok((added, summary)) = written else {
        return false;
    };
    assert!(output.starts_with(data), "the original bytes changed");
    assert_eq!(summary.new_len, output.len() as u64);

    let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::from(output.clone()));
    let after = match reader.verify_appended(Arc::clone(&source), &summary) {
        Ok(after) => after,
        // One more section can tip a hostile file over a bound on the whole walk. The update is
        // refused for that by whoever verifies it, so it's a limit and not a wrong file.
        Err(AppendError::Reader(lopdf::IndexedReaderError::StructureLimitExceeded { .. }))
        | Err(AppendError::Reader(lopdf::IndexedReaderError::EntryLimitExceeded { .. })) => return false,
        Err(error) => panic!("the update failed its own check: {error}"),
    };
    assert!(
        matches!(after.resolve_object(added.id), Ok(Object::Dictionary(_))),
        "the new object doesn't read back"
    );
    for id in reader.object_ids().into_iter().take(MAX_COMPARED_OBJECTS) {
        if let Ok(before) = reader.resolve_object(id) {
            let now = after.resolve_object(id).expect("an old object stopped reading");
            assert_eq!(format!("{before:?}"), format!("{now:?}"), "old object {id:?} changed");
        }
    }
    true
}
