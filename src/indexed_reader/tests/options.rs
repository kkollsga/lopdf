//! Indexed-reader tests: options.

use super::*;

#[test]
fn public_options_wire_every_exposed_resolver_and_page_limit() {
    let options = IndexedReaderOptions {
        object_bytes: 101,
        stream_bytes: 102,
        encoded_stream_bytes: Some(104),
        endstream_tail_bytes: 103,
        reference_depth: 7,
        page_tree_depth: 0,
        max_pages: 11,
        password: None,
    };
    let limits = ResolverLimits::from(&options);
    assert_eq!(limits.max_object_bytes, 101);
    assert_eq!(limits.max_stream_bytes, 102);
    assert_eq!(limits.max_encoded_stream_bytes, Some(104));
    assert_eq!(limits.max_endstream_tail_bytes, 103);
    assert_eq!(limits.max_length_depth, 7);

    let pdf = generated_deep_page_tree_pdf(1);
    let reader = IndexedReader::open_with_options(BytesSource::from(pdf), options).unwrap();
    assert!(matches!(
        reader.page_map(),
        Err(IndexedReaderError::PageTreeDepthLimitExceeded { limit: 0 })
    ));
}
