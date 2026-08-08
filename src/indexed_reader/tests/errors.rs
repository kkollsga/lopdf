//! Indexed-reader tests: errors.

use super::*;

#[test]
fn object_limit_provenance_does_not_change_the_stable_display() {
    for provenance in [
        ObjectLimitProvenance::FrameNeedMoreAtMaximum,
        ObjectLimitProvenance::SourceExhaustedAtMaximum,
        ObjectLimitProvenance::ArithmeticInvariant,
    ] {
        assert_eq!(
            IndexedReaderError::ObjectLimitExceeded {
                id: (7, 0),
                limit: 64,
                provenance,
            }
            .to_string(),
            "object (7, 0) exceeds the 64-byte parser limit"
        );
    }

    for limit in [4 * 1024 * 1024, 64 * 1024 * 1024] {
        assert!(matches!(
            object_frame_need_more((7, 0), 91, limit, limit + 1),
            IndexedReaderError::ObjectLimitExceeded {
                id: (7, 0),
                limit: actual,
                provenance: ObjectLimitProvenance::FrameNeedMoreAtMaximum,
            } if actual == limit
        ));
        assert!(matches!(
            object_frame_need_more((7, 0), 91, limit, limit),
            IndexedReaderError::IncompleteObject { id: (7, 0), offset: 91 }
        ));
    }
    assert!(matches!(
        object_limit_arithmetic((8, 0), 64 * 1024 * 1024),
        IndexedReaderError::ObjectLimitExceeded {
            id: (8, 0),
            limit: 67_108_864,
            provenance: ObjectLimitProvenance::ArithmeticInvariant,
        }
    ));
}

#[test]
fn bounded_scalar_source_exhaustion_preserves_object_limit_with_typed_provenance() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"(",
    }]);
    let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let permit = crate::ScalarResolutionPermit::new(u64::try_from(pdf.len()).unwrap() + 4096);
    let error = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap_err();
    assert!(matches!(
        error,
        IndexedReaderError::ObjectLimitExceeded {
            id: (1, 0),
            limit,
            provenance: ObjectLimitProvenance::SourceExhaustedAtMaximum,
        } if limit < u64::try_from(pdf.len()).unwrap()
    ));
    assert_eq!(permit.close().unwrap().current_bytes, 0);
}
