//! Indexed-reader tests: encrypted documents.

use super::*;

#[test]
fn bounded_scalar_decrypts_normal_strings_with_proven_overlap() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");
        let reader = IndexedReader::open_with_options(
            BytesSource::from(pdf),
            IndexedReaderOptions {
                password: Some(b"user".to_vec()),
                ..IndexedReaderOptions::default()
            },
        )
        .unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap();
        assert_eq!(scalar.as_object().as_str().unwrap(), b"encrypted string");
        assert!(scalar.peak_bytes() <= permit.limit_bytes());
        drop(scalar);
        permit.close().unwrap();
    }
}

#[test]
fn bounded_stream_decrypts_normal_streams_across_revisions_two_through_six() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");
        let reader = IndexedReader::open_with_options(
            BytesSource::from(pdf),
            IndexedReaderOptions {
                password: Some(b"user".to_vec()),
                ..IndexedReaderOptions::default()
            },
        )
        .unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let stream = reader.resolve_stream_with_permit((2, 0), &permit).unwrap();
        assert_eq!(stream.as_stream().content, b"encrypted stream");
        assert!(stream.peak_bytes() <= permit.limit_bytes());
        let content = stream.into_content();
        assert_eq!(content.as_slice(), b"encrypted stream");
        assert!(content.peak_bytes() <= permit.limit_bytes());
        drop(content);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }
}

#[test]
fn oversized_encrypted_materialization_refuses_before_payload_and_releases_permit() {
    let pdf = encrypted_pdf_with_stream(4, "owner", "user", &vec![b'x'; 128 * 1_024]);
    let object_start = pdf
        .windows(b"2 0 obj\n".len())
        .position(|window| window == b"2 0 obj\n")
        .unwrap();
    let stream_prefix = pdf[object_start..]
        .windows(b"stream\n".len())
        .position(|window| window == b"stream\n")
        .unwrap();
    let encoded_start = u64::try_from(object_start + stream_prefix + b"stream\n".len()).unwrap();
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(
        erased,
        IndexedReaderOptions {
            stream_bytes: 4,
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();

    let descriptor = reader.resolve_stream_descriptor((2, 0)).unwrap();
    assert_eq!(descriptor.protection(), EncodedStreamProtection::DocumentEncrypted);
    let encoded_end = encoded_start.checked_add(descriptor.encoded_len().unwrap()).unwrap();
    assert!(matches!(
        descriptor.open_plain_encoded(),
        Err(IndexedStreamReadError::Protected {
            protection: EncodedStreamProtection::DocumentEncrypted,
            ..
        })
    ));

    source.requests.lock().unwrap().clear();
    let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    assert!(matches!(
        reader.resolve_stream_with_permit((2, 0), &permit),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (2, 0),
            limit: 4,
            ..
        })
    ));
    assert!(
        !source
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(offset, _)| *offset >= encoded_start && *offset < encoded_end),
        "encrypted materialization refusal issued a payload read"
    );
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
}

#[test]
fn bounded_encrypted_object_stream_matches_eager_and_releases_every_charge() {
    for revision in 2..=6 {
        for flate in [false, true] {
            let (pdf, _, _) = encrypted_object_stream_pdf(revision, flate, 0);
            let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
            let expected = reader.resolve_object((10, 0)).unwrap();

            let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
            let scalar = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap();
            assert_eq!(scalar.as_object(), &expected);
            assert_eq!(
                scalar
                    .as_object()
                    .as_dict()
                    .unwrap()
                    .get(b"Text")
                    .unwrap()
                    .as_str()
                    .unwrap(),
                b"member secret"
            );
            let observed_peak = scalar.peak_bytes();
            assert!(observed_peak <= permit.limit_bytes());
            drop(scalar);
            assert_eq!(permit.close().unwrap().current_bytes, 0);

            let refused = crate::ScalarResolutionPermit::new(observed_peak - 1);
            assert!(matches!(
                reader.resolve_scalar_with_permit((10, 0), &refused),
                Err(IndexedReaderError::ScalarResourceLimit { .. })
            ));
            assert_eq!(refused.stats().current_bytes, 0);
            refused.close().unwrap();
        }
    }
}

#[test]
fn encrypted_object_stream_overlap_refusal_precedes_payload_read_and_retries_identically() {
    let (pdf, _, _) = encrypted_object_stream_pdf(4, false, 512 * 1024);
    let container = pdf
        .windows(b"5 0 obj\n".len())
        .position(|window| window == b"5 0 obj\n")
        .unwrap();
    let stream = pdf[container..]
        .windows(b"stream\n".len())
        .position(|window| window == b"stream\n")
        .unwrap();
    let encoded_start = u64::try_from(container + stream + b"stream\n".len()).unwrap();
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(
        erased,
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();

    let mut limit = 1_u64;
    let refusal_limit = loop {
        let permit = crate::ScalarResolutionPermit::new(limit);
        let error = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap_err();
        if let IndexedReaderError::StreamLimitExceeded { length, .. } = &error {
            assert!(*length > limit, "non-progressing stream admission");
            assert_eq!(permit.stats().current_bytes, 0);
            permit.close().unwrap();
            limit = *length;
            continue;
        }
        let IndexedReaderError::ScalarResourceLimit { requested, phase, .. } = error else {
            panic!("unexpected bounded refusal: {error:?}")
        };
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
        if phase == "object-stream-decryption-overlap" {
            break limit;
        }
        assert!(requested > limit, "non-progressing admission at {phase}");
        limit = requested;
    };

    let mut observed = None;
    for _ in 0..2 {
        source.requests.lock().unwrap().clear();
        let permit = crate::ScalarResolutionPermit::new(refusal_limit);
        let error = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap_err();
        let IndexedReaderError::ScalarResourceLimit {
            requested,
            limit,
            phase,
            ..
        } = error
        else {
            panic!("unexpected bounded refusal: {error:?}")
        };
        assert_eq!(phase, "object-stream-decryption-overlap");
        assert_eq!(
            observed.get_or_insert((requested, limit, phase)),
            &(requested, limit, phase)
        );
        assert!(
            source
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(offset, _)| *offset != encoded_start),
            "payload read was issued before decryption-overlap admission"
        );
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
    }
}

#[test]
fn public_preparation_matches_eager_for_encrypted_object_streams_without_private_cache_use() {
    for revision in 2..=6 {
        for flate in [false, true] {
            for stream_eol in [b"\n".as_slice(), b"\r".as_slice(), b"\r\n".as_slice()] {
                let (pdf, _, _) = encrypted_object_stream_pdf_with_eol(revision, flate, 0, stream_eol);
                let eager = Document::load_mem_with_options(&pdf, crate::LoadOptions::with_password("user")).unwrap();
                let expected = eager.get_object((10, 0)).unwrap();
                let source = Arc::new(TracingBytesSource {
                    bytes: pdf,
                    requests: Mutex::new(Vec::new()),
                });
                let erased: Arc<dyn RandomAccessSource> = source.clone();
                let reader = IndexedReader::open_shared(
                    erased,
                    IndexedReaderOptions {
                        password: Some(b"user".to_vec()),
                        ..IndexedReaderOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
                assert_eq!(reader.object_cache_stats(), IndexedObjectCacheStats::default());
                assert_eq!(
                    reader.object_stream_cache_stats(),
                    IndexedObjectStreamCacheStats::default()
                );

                let IndexedObjectLocation::Compressed { container, index } = reader.object_location((10, 0)).unwrap()
                else {
                    panic!("encrypted fixture member was not declared compressed")
                };
                assert_eq!(container, (5, 0));
                assert_eq!(index, 0);

                let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
                let prepared = reader.prepare_object_stream_with_permit(container, &permit).unwrap();
                assert_eq!(prepared.container_id(), container);
                assert_eq!(permit.stats().current_bytes, prepared.retained_bytes());
                let reads_after_prepare = source.requests.lock().unwrap().len();

                let member = prepared.resolve_member((10, 0), index).unwrap();
                assert_eq!(member.as_object(), expected);
                assert_eq!(
                    member
                        .as_object()
                        .as_dict()
                        .unwrap()
                        .get(b"Text")
                        .unwrap()
                        .as_str()
                        .unwrap(),
                    b"member secret"
                );
                assert_eq!(source.requests.lock().unwrap().len(), reads_after_prepare);
                assert_eq!(
                    permit.stats().current_bytes,
                    prepared.retained_bytes() + member.retained_bytes()
                );
                assert!(permit.stats().peak_bytes <= permit.limit_bytes());
                assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
                assert_eq!(reader.object_cache_stats(), IndexedObjectCacheStats::default());
                assert_eq!(
                    reader.object_stream_cache_stats(),
                    IndexedObjectStreamCacheStats::default()
                );

                drop(member);
                assert_eq!(permit.stats().current_bytes, prepared.retained_bytes());
                drop(prepared);
                assert_eq!(permit.close().unwrap().current_bytes, 0);
                assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
            }
        }
    }
}

#[test]
fn public_preparation_encrypted_overlap_refusal_precedes_payload_read_and_retries_identically() {
    for (stream_eol, expected_span) in [
        (b"\n".as_slice(), (66, 524_450)),
        (b"\r".as_slice(), (66, 524_450)),
        (b"\r\n".as_slice(), (67, 524_451)),
    ] {
        assert_public_preparation_encrypted_overlap_refusal(stream_eol, expected_span);
    }
}

#[test]
fn encrypted_cr_admission_preserves_tail_stats_and_security_precedence() {
    for stream_eol in [b"\n".as_slice(), b"\r".as_slice(), b"\r\n".as_slice()] {
        let (mut pdf, _, _) = encrypted_object_stream_pdf_with_eol(4, false, 0, stream_eol);
        let container = pdf
            .windows(b"5 0 obj\n".len())
            .position(|window| window == b"5 0 obj\n")
            .unwrap();
        let endstream = container
            + pdf[container..]
                .windows(b"\nendstream".len())
                .position(|window| window == b"\nendstream")
                .unwrap();
        pdf[endstream + b"\nendstrea".len()] = b'X';
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
        assert!(matches!(
            reader.prepare_object_stream_with_permit((5, 0), &permit),
            Err(IndexedReaderError::MissingEndstream { id: (5, 0) })
        ));
        let stats = permit.stats();
        assert_eq!(stats.current_bytes, 0);
        assert!(stats.peak_bytes <= 4 * 1024);
        assert_eq!(stats.reservations, 4);
        permit.close().unwrap();
    }

    let (mut pdf, _, _) = encrypted_object_stream_pdf_with_eol(4, false, 512 * 1024, b"\r");
    let container = pdf
        .windows(b"5 0 obj\n".len())
        .position(|window| window == b"5 0 obj\n")
        .unwrap();
    let stream = container
        + pdf[container..]
            .windows(b"stream\r".len())
            .position(|window| window == b"stream\r")
            .unwrap();
    let encoded_start = u64::try_from(stream + b"stream\r".len()).unwrap();
    let encoded_end = encoded_start + 524_384;
    let endstream = usize::try_from(encoded_end).unwrap();
    assert_eq!(&pdf[endstream..endstream + b"\nendstream".len()], b"\nendstream");
    pdf[endstream + b"\nendstrea".len()] = b'X';
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(
        erased,
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    let mut limit = 1_u64;
    let refusal_limit = loop {
        let permit = crate::ScalarResolutionPermit::new(limit);
        let error = reader.prepare_object_stream_with_permit((5, 0), &permit).unwrap_err();
        let next = match error {
            IndexedReaderError::StreamLimitExceeded { length, .. } => length,
            IndexedReaderError::ScalarResourceLimit {
                phase: "object-stream-decryption-overlap",
                ..
            } => {
                assert_eq!(permit.stats().current_bytes, 0);
                permit.close().unwrap();
                break limit;
            }
            IndexedReaderError::ScalarResourceLimit { requested, .. } => requested,
            error => panic!("unexpected bounded refusal: {error:?}"),
        };
        assert!(next > limit);
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
        limit = next;
    };
    source.requests.lock().unwrap().clear();
    let permit = crate::ScalarResolutionPermit::new(refusal_limit);
    assert!(matches!(
        reader.prepare_object_stream_with_permit((5, 0), &permit),
        Err(IndexedReaderError::ScalarResourceLimit {
            phase: "object-stream-decryption-overlap",
            ..
        })
    ));
    assert!(source.requests.lock().unwrap().iter().all(|(offset, length)| {
        let request_end = offset.saturating_add(u64::try_from(*length).unwrap_or(u64::MAX));
        request_end <= encoded_start || *offset >= encoded_end
    }));
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
}

#[test]
fn bounded_encrypted_stream_over_o_refuses_before_content_allocation() {
    let payload = vec![0x5a; 2 * 1024 * 1024];
    let pdf = encrypted_pdf_with_stream(6, "owner", "user", &payload);
    let reader = IndexedReader::open_with_options(
        BytesSource::from(pdf),
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    assert!(matches!(
        reader.resolve_stream_with_permit((2, 0), &permit),
        Err(IndexedReaderError::StreamLimitExceeded { .. })
            | Err(IndexedReaderError::ScalarResourceLimit { .. })
            | Err(IndexedReaderError::ObjectLimitExceeded { .. })
    ));
    assert!(permit.stats().peak_bytes <= permit.limit_bytes());
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
}

#[test]
fn public_preparation_checks_encoded_end_before_first_and_n_after_required_cr_admission() {
    for dictionary in [
        "/Type /ObjStm /N 1 /First -1",
        "/Type /ObjStm /N -1 /First 5",
        "/Type /ObjStm /N -1 /First -1",
    ] {
        let fixture = object_stream_fixture_with_declared_length(dictionary, b"10 0 (ten)", &[(10, 0)], 1_000_000);
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
        assert!(matches!(
            reader.prepare_object_stream_with_permit((5, 0), &permit),
            Err(IndexedReaderError::StreamLimitExceeded {
                id: (5, 0),
                length: 1_000_000,
                ..
            })
        ));
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
    }

    for (declared_n, declared_first) in [(1, -1), (-1, 5), (-1, -1)] {
        let (pdf, _, _) = encrypted_object_stream_pdf_with_options(
            4,
            false,
            0,
            b"\r",
            0,
            Some((declared_n, declared_first, 1_000_000)),
        );
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
        assert!(matches!(
            reader.prepare_object_stream_with_permit((5, 0), &permit),
            Err(IndexedReaderError::StreamLimitExceeded {
                id: (5, 0),
                length: 1_000_000,
                ..
            })
        ));
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
    }
}

#[test]
fn encrypted_public_preparation_bounds_large_prefix_source_requests() {
    const PREFIX_PADDING: usize = 4 * 1024;
    const REQUEST_LIMIT: usize = PREFIX_PADDING + 512;

    let (pdf, _, _) = encrypted_object_stream_pdf_with_eol_and_prefix(4, false, 0, b"\n", PREFIX_PADDING);
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(
        erased,
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    source.requests.lock().unwrap().clear();
    let permit = crate::ScalarResolutionPermit::new(8 * 1024 * 1024);
    let prepared = reader.prepare_object_stream_with_permit((5, 0), &permit).unwrap();
    let requests = source.requests.lock().unwrap().len();
    assert!(
        (PREFIX_PADDING..=REQUEST_LIMIT).contains(&requests),
        "large encrypted ObjStm prefix used {requests} source requests; limit is {REQUEST_LIMIT}"
    );
    drop(prepared);
    assert_eq!(permit.close().unwrap().current_bytes, 0);
}

#[test]
fn encrypted_public_metadata_and_unified_bounded_apis_preserve_decryption() {
    let pdf = encrypted_pdf_with_stream(6, "owner", "user", b"encrypted payload");
    let reader = IndexedReader::open_with_options(
        BytesSource::from(pdf),
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();

    let root = reader.trailer_entry_owned(b"Root").unwrap().unwrap();
    assert!(root.as_dict().unwrap().has_type(b"Catalog"));
    let stats = reader.index_stats().unwrap();
    assert!(stats.object_count() >= 3);
    assert_eq!(stats.page_count(), 0);

    let scalar_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let scalar = reader.resolve_object_with_permit((1, 0), &scalar_permit).unwrap();
    assert_eq!(scalar.as_object().as_str().unwrap(), b"encrypted string");
    drop(scalar);
    assert_eq!(scalar_permit.stats().current_bytes, 0);

    let stream_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let stream = reader.resolve_object_with_permit((2, 0), &stream_permit).unwrap();
    let Object::Stream(stream_object) = stream.as_object() else {
        panic!("encrypted stream did not resolve as a stream")
    };
    assert_eq!(stream_object.content, b"encrypted payload");
    drop(stream);
    assert_eq!(stream_permit.stats().current_bytes, 0);
}
