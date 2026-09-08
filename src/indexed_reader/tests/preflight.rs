//! Indexed-reader tests: preflight.

use super::*;

#[test]
fn bounded_scalar_holds_one_allowance_through_parse_and_measurement() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Type /Catalog /Value [1 /Name (text)] >>",
    }]);
    let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
    let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap();
    assert!(matches!(scalar.as_object(), Object::Dictionary(_)));
    assert!(scalar.retained_bytes() > 0);
    assert!(scalar.peak_bytes() <= permit.limit_bytes());
    assert_eq!(permit.stats().current_bytes, scalar.retained_bytes());
    drop(scalar);
    assert_eq!(permit.close().unwrap().current_bytes, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn bounded_scalar_classifies_stream_framing_from_complete_fixture_dictionaries_without_reading_payloads() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 4 /Kind /DirectBad >>\nstream\nvalue\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 3 0 R /Kind /IndirectBad >>\nstream\nvalue\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"4",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 1000000 /Kind /PastSource >>\nstream\nshort",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Kind /Valid >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Kind /Missing >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 7,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length (bad) /Kind /InvalidLength >>\nstream\nignored\nendstream",
        },
    ]);
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();

    for (id, expected) in [
        ((1, 0), dictionary! { "Length" => 4, "Kind" => "DirectBad" }),
        ((2, 0), dictionary! { "Length" => (3, 0), "Kind" => "IndirectBad" }),
        ((4, 0), dictionary! { "Length" => 1_000_000, "Kind" => "PastSource" }),
    ] {
        source.requests.lock().unwrap().clear();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit(id, &permit).unwrap();
        // Upstream #568 now rejects these malformed eager objects while the
        // indexed scalar route retains its pre-existing dictionary fallback.
        // Keep this fixture contract explicit instead of following a moving
        // eager baseline.
        assert_eq!(scalar.as_object(), &Object::Dictionary(expected));
        assert!(
            source
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, bytes)| *bytes <= 64 * 1024)
        );
        drop(scalar);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }

    for id in [(5, 0), (6, 0), (7, 0)] {
        source.requests.lock().unwrap().clear();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(matches!(
            reader.resolve_scalar_with_permit(id, &permit),
            Err(IndexedReaderError::NotScalarObject { id: actual }) if actual == id
        ));
        assert!(
            source
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, bytes)| *bytes <= 64 * 1024)
        );
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn bounded_scalar_stream_tail_refusal_is_retryable_and_preserves_fatal_limits() {
    let mut bad_tail = b"<< /Length 5 /Kind /BadTail >>\nstream\nhello\nnot-endstream".to_vec();
    bad_tail.resize(bad_tail.len() + 16 * 1024, b'x');
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &bad_tail,
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 >>\nstream\nhello\nendst",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length -1 >>\nstream\n\nendstream",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 20 >>\nstream\n01234567890123456789\nendstream",
        },
    ]);
    let reader = open_reader(
        &pdf,
        ResolverLimits {
            max_endstream_tail_bytes: 8 * 1024,
            ..ResolverLimits::default()
        },
    );
    let generous = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &generous).unwrap();
    let peak = scalar.peak_bytes();
    drop(scalar);
    generous.close().unwrap();

    let refused = crate::ScalarResolutionPermit::new(peak - 1);
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &refused),
        Err(IndexedReaderError::ScalarResourceLimit {
            phase: "scalar-stream-end-marker",
            ..
        })
    ));
    assert_eq!(refused.stats().current_bytes, 0);
    refused.close().unwrap();

    let retry = crate::ScalarResolutionPermit::new(peak);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &retry).unwrap();
    drop(scalar);
    assert_eq!(retry.close().unwrap().current_bytes, 0);

    let tail_limited = open_reader(
        &pdf,
        ResolverLimits {
            max_endstream_tail_bytes: 4,
            ..ResolverLimits::default()
        },
    );
    let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    assert!(matches!(
        tail_limited.resolve_scalar_with_permit((2, 0), &permit),
        Err(IndexedReaderError::MissingEndstream { id: (2, 0) })
    ));
    assert_eq!(permit.close().unwrap().current_bytes, 0);

    let size_limited = open_reader(
        &pdf,
        ResolverLimits {
            max_stream_bytes: 10,
            ..ResolverLimits::default()
        },
    );
    for (id, expected) in [((3, 0), "negative"), ((4, 0), "size")] {
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let error = size_limited.resolve_scalar_with_permit(id, &permit).unwrap_err();
        match expected {
            "negative" => assert!(matches!(
                error,
                IndexedReaderError::NegativeStreamLength { id: (3, 0), length: -1 }
            )),
            "size" => assert!(matches!(
                error,
                IndexedReaderError::StreamLimitExceeded {
                    id: (4, 0),
                    length: 20,
                    limit: 10
                }
            )),
            _ => unreachable!(),
        }
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn bounded_scalar_stream_tail_source_failure_releases_and_retries() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Length 5 /Kind /BadTail >>\nstream\nhello\nnot-endstream",
    }]);
    let encoded_start = pdf
        .windows(b"stream\n".len())
        .position(|window| window == b"stream\n")
        .unwrap()
        + b"stream\n".len();
    let encoded_end = u64::try_from(encoded_start + 5).unwrap();
    let source = Arc::new(FailingOffsetSource {
        bytes: pdf,
        fail_offset: AtomicU64::new(u64::MAX),
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();

    source.fail_offset.store(encoded_end, Ordering::SeqCst);
    source.requests.lock().unwrap().clear();
    let failed = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &failed),
        Err(IndexedReaderError::Source(SourceError::Io(_)))
    ));
    assert!(
        source
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, bytes)| *bytes <= 64 * 1024)
    );
    assert_eq!(failed.close().unwrap().current_bytes, 0);

    source.fail_offset.store(u64::MAX, Ordering::SeqCst);
    let retry = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &retry).unwrap();
    assert!(matches!(scalar.as_object(), Object::Dictionary(_)));
    drop(scalar);
    assert_eq!(retry.close().unwrap().current_bytes, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn bounded_scalar_stream_overflow_and_permit_lifecycle_errors_stay_fatal() {
    assert!(matches!(
        checked_stream_end((7, 0), u64::MAX - 1, 2),
        Err(IndexedReaderError::InvalidIndirectObject {
            id: (7, 0),
            offset
        }) if offset == u64::MAX - 1
    ));

    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Length 5 >>\nstream\nhello\nnot-endstream",
    }]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let cancelled = crate::ScalarResolutionPermit::new(1024 * 1024);
    cancelled.cancel();
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &cancelled),
        Err(IndexedReaderError::ScalarResolutionCancelled { .. })
    ));
    assert_eq!(cancelled.close().unwrap().current_bytes, 0);

    let closed = crate::ScalarResolutionPermit::new(1024 * 1024);
    closed.close().unwrap();
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &closed),
        Err(IndexedReaderError::ScalarResolutionClosed { .. })
    ));
    assert_eq!(closed.stats().current_bytes, 0);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn bounded_scalar_decrypts_a_malformed_stream_dictionary() {
    for revision in 2..=6 {
        let mut pdf = encrypted_malformed_dictionary_pdf(revision, "owner", "user");
        let object_start = pdf
            .windows(b"3 0 obj".len())
            .position(|window| window == b"3 0 obj")
            .unwrap();
        let marker = object_start
            + pdf[object_start..]
                .windows(b"endstream".len())
                .position(|window| window == b"endstream")
                .unwrap();
        pdf[marker..marker + b"endstream".len()].copy_from_slice(b"badstream");
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((3, 0), &permit).unwrap();
        // The indexed route intentionally preserves its malformed-stream
        // dictionary fallback even though upstream #568 changed eager parsing
        // to reject this shape before decryption.
        assert_eq!(
            scalar
                .as_object()
                .as_dict()
                .unwrap()
                .get(b"Sentinel")
                .unwrap()
                .as_str()
                .unwrap(),
            b"malformed dictionary secret"
        );
        drop(scalar);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }
}

#[test]
fn scalar_ast_preflight_bounds_exhaustive_token_and_object_corpus() {
    let corpus: &[&[u8]] = &[
        b"null",
        b"true",
        b"false",
        b"0",
        b"-9223372036854775808",
        b"+17",
        b".5",
        b"-12.75",
        b"1 0 R",
        b"/",
        b"/Name",
        b"/A#20B#23C",
        b"()",
        b"(plain text)",
        b"(line\\nreturn\\r tab\\t parens\\(\\))",
        b"(octal \\0 \\12 \\377)",
        b"(continued\\\r\nline)",
        b"(nested (one (two) tail) end)",
        b"<>",
        b"<0>",
        b"<01 23 4a BC d>",
        b"[]",
        b"[null true false 1 .5 /Name (text) <01> 2 0 R]",
        b"[1% between\r\n2% again\n3]",
        b"[[] [1] [[2]] << /K /V >>]",
        b"<<>>",
        b"<< /Type /Catalog >>",
        b"<< /A 1 /B [2 3] /C << /D (four) >> >>",
        b"<< /Dup (old) /Dup (replacement) >>",
        b"<<% lead\n/A#20B <012345> /Ref 99 65535 R >>",
    ];
    for input in corpus {
        assert_scalar_preflight_bounds_retained(input);
    }
}

#[test]
fn scalar_ast_preflight_property_matrix_bounds_parser_capacities() {
    for length in 0_usize..=257 {
        let mut name = Vec::with_capacity(length.saturating_mul(3).saturating_add(1));
        name.push(b'/');
        for index in 0..length {
            if index % 3 == 0 {
                name.extend_from_slice(b"#41");
            } else {
                name.push(b'a' + u8::try_from(index % 26).unwrap());
            }
        }
        assert_scalar_preflight_bounds_retained(&name);

        let mut literal = Vec::with_capacity(length.saturating_mul(4).saturating_add(2));
        literal.push(b'(');
        for index in 0..length {
            match index % 5 {
                0 => literal.extend_from_slice(b"x"),
                1 => literal.extend_from_slice(b"\\n"),
                2 => literal.extend_from_slice(b"\\053"),
                3 => literal.extend_from_slice(b"(z)"),
                _ => literal.extend_from_slice(b"\\\n"),
            }
        }
        literal.push(b')');
        assert_scalar_preflight_bounds_retained(&literal);

        let mut hex = Vec::with_capacity(length.saturating_mul(3).saturating_add(2));
        hex.push(b'<');
        for index in 0..length {
            hex.extend_from_slice(if index % 2 == 0 { b"a " } else { b"0f " });
        }
        hex.push(b'>');
        assert_scalar_preflight_bounds_retained(&hex);
    }

    for count in 0..=128 {
        let mut array = Vec::from(b"[".as_slice());
        let mut dictionary = Vec::from(b"<<".as_slice());
        for index in 0..count {
            array.extend_from_slice(match index % 4 {
                0 => b" 0".as_slice(),
                1 => b" /N".as_slice(),
                2 => b" (s)".as_slice(),
                _ => b" [1 2]".as_slice(),
            });
            dictionary.extend_from_slice(format!(" /K{index} [{index} /V (text)]").as_bytes());
        }
        array.extend_from_slice(b" ]");
        dictionary.extend_from_slice(b" >>");
        assert_scalar_preflight_bounds_retained(&array);
        assert_scalar_preflight_bounds_retained(&dictionary);
    }
}

#[test]
fn scalar_ast_preflight_charges_nested_literal_overlap() {
    let input = b"(prefix (middle (a long nested payload) suffix) tail)";
    let preflight = scalar_ast_preflight(input).unwrap();
    let object = crate::parser::direct_object(input).unwrap();
    assert!(preflight.transient_bytes > 0, "{preflight:?}");
    assert!(usize::try_from(preflight.reserved_bytes(false)).unwrap() >= scalar_object_retained_bytes(&object));
}

#[test]
fn retained_accounting_uses_spare_vector_capacities() {
    let mut literal = Vec::with_capacity(4096);
    literal.push(b'x');
    let object = Object::String(literal, crate::StringFormat::Literal);
    assert!(scalar_object_retained_bytes(&object) >= std::mem::size_of::<Object>() + 4096);

    let mut content = Vec::with_capacity(2048);
    content.push(0);
    let raw = PreparedObjectStream::Raw(Stream::new(Dictionary::new(), content));
    assert!(raw.retained_bytes() >= std::mem::size_of::<Stream>() + 2048);
}

#[test]
fn scalar_ast_preflight_keeps_multimegabyte_literal_near_raw_plus_decoded() {
    let payload_bytes = 2 * 1024 * 1024;
    let mut input = Vec::with_capacity(payload_bytes + 2);
    input.push(b'(');
    input.resize(payload_bytes + 1, b'x');
    input.push(b')');
    let preflight = scalar_ast_preflight(&input).unwrap();
    let ast = usize::try_from(preflight.reserved_bytes(false)).unwrap();
    assert!(
        input.len().saturating_add(ast) <= input.len().saturating_mul(2).saturating_add(4096),
        "raw={}, ast={ast}, preflight={preflight:?}",
        input.len()
    );
}

#[test]
fn bounded_scalar_one_byte_below_observed_peak_refuses_before_ast_parse() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Type /Catalog /Value [1 /Name (text)] >>",
    }]);
    let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let generous = crate::ScalarResolutionPermit::new(1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &generous).unwrap();
    let peak = scalar.peak_bytes();
    drop(scalar);
    generous.close().unwrap();

    let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
    let limited = crate::ScalarResolutionPermit::new(peak - 1);
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &limited),
        Err(IndexedReaderError::ScalarResourceLimit {
            phase: "scalar-ast-envelope",
            ..
        })
    ));
    assert_eq!(OBJECT_BODY_PARSE_CALLS.with(Cell::get), 0);
    assert_eq!(limited.stats().current_bytes, 0);
    limited.close().unwrap();
}

#[test]
fn bounded_scalar_refuses_a_scalar_larger_than_four_mib_and_releases_every_charge() {
    let mut body = Vec::with_capacity(4 * 1024 * 1024 + 3);
    body.push(b'(');
    body.resize(4 * 1024 * 1024 + 2, b'x');
    body.push(b')');
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: &body,
    }]);
    let reader = open_reader(
        &pdf,
        ResolverLimits {
            max_object_bytes: 4 * 1024 * 1024,
            ..ResolverLimits::default()
        },
    );
    let permit = crate::ScalarResolutionPermit::new(64 * 1024 * 1024);
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &permit),
        Err(IndexedReaderError::ObjectLimitExceeded {
            id: (1, 0),
            limit: 4_194_304,
            provenance: ObjectLimitProvenance::FrameNeedMoreAtMaximum,
        })
    ));
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
}

#[test]
fn bounded_scalar_cancelled_permit_never_allocates_and_closes_at_zero() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Type /Catalog >>",
    }]);
    let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
    let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    permit.cancel();
    assert!(matches!(
        reader.resolve_scalar_with_permit((1, 0), &permit),
        Err(IndexedReaderError::ScalarResolutionCancelled { .. })
    ));
    assert_eq!(permit.stats().peak_bytes, 0);
    assert_eq!(permit.close().unwrap().current_bytes, 0);
}

#[test]
fn bounded_compressed_scalar_accounts_encoded_decoded_and_ast_overlap() {
    let body = b"<< /Type /Catalog /Value [1 /Name (compressed)] >>";
    let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
        &encoder.finish().unwrap(),
        &[(10, 0)],
    );
    let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
    let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap();
    assert!(matches!(scalar.as_object(), Object::Dictionary(_)));
    assert!(scalar.peak_bytes() <= permit.limit_bytes());
    drop(scalar);
    assert_eq!(permit.close().unwrap().current_bytes, 0);
}

#[test]
fn bounded_compressed_scalar_reports_retryable_decompressed_growth() {
    let body = vec![b'x'; 300 * 1024];
    let mut literal = Vec::with_capacity(body.len() + 2);
    literal.push(b'(');
    literal.extend_from_slice(&body);
    literal.push(b')');
    let (first, plain) = object_stream_content(&[(10, literal.as_slice())]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
        &encoder.finish().unwrap(),
        &[(10, 0)],
    );
    let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();

    let first_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let error = reader.resolve_scalar_with_permit((10, 0), &first_permit).unwrap_err();
    let IndexedReaderError::ScalarResourceLimit {
        requested,
        limit,
        phase,
        ..
    } = error
    else {
        panic!("expected retryable scalar resource limit, got {error:?}");
    };
    assert_eq!(limit, 1024 * 1024);
    assert_eq!(requested, 2 * 1024 * 1024);
    assert_eq!(phase, "object-stream-decompressed-growth");
    assert_eq!(first_permit.stats().current_bytes, 0);
    first_permit.close().unwrap();

    let retry_permit = crate::ScalarResolutionPermit::new(requested);
    let scalar = reader.resolve_scalar_with_permit((10, 0), &retry_permit).unwrap();
    assert_eq!(scalar.as_object().as_str().unwrap(), body.as_slice());
    assert!(scalar.peak_bytes() <= requested);
    drop(scalar);
    assert_eq!(retry_permit.close().unwrap().current_bytes, 0);
}

#[test]
fn bounded_compressed_scalar_rejects_large_inflation_and_releases_to_zero() {
    let mut body = Vec::with_capacity(2 * 1024 * 1024 + 2);
    body.push(b'(');
    body.resize(2 * 1024 * 1024 + 1, b'x');
    body.push(b')');
    let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
        &encoder.finish().unwrap(),
        &[(10, 0)],
    );
    let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
    let ceiling = 4 * 1024 * 1024;
    let mut allowance = 1024 * 1024;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let permit = crate::ScalarResolutionPermit::new(allowance);
        let error = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap_err();
        assert!(permit.stats().peak_bytes <= permit.limit_bytes());
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
        let IndexedReaderError::ScalarResourceLimit {
            requested,
            limit,
            phase,
            ..
        } = error
        else {
            panic!("expected stable bounded-growth refusal, got {error:?}");
        };
        assert_eq!(limit, allowance);
        assert!(requested > allowance);
        assert_eq!(phase, "object-stream-decompressed-growth");
        if requested > ceiling {
            break;
        }
        allowance = requested;
    }
    assert_eq!(attempts, 3, "growth must terminate logarithmically at O");
}

#[test]
fn bounded_scalar_never_issues_a_physical_read_over_sixty_four_kib() {
    let mut body = Vec::with_capacity(128 * 1024 + 2);
    body.push(b'(');
    body.resize(128 * 1024 + 1, b'x');
    body.push(b')');
    let source = Arc::new(TracingBytesSource {
        bytes: object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &body,
        }]),
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();
    source.requests.lock().unwrap().clear();
    let permit = crate::ScalarResolutionPermit::new(64 * 1024 * 1024);
    let scalar = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap();
    assert_eq!(scalar.as_object().as_str().unwrap().len(), 128 * 1024);
    assert!(
        source
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, bytes)| *bytes <= 64 * 1024)
    );
    drop(scalar);
    permit.close().unwrap();
}
