//! Indexed-reader tests: resolve.

use super::*;

#[test]
fn normal_and_nested_objects_match_complete_fixture_values() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Nested << /Values [1 (two) << /Flag true >>] >> >>",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"[1 2 (three) << /Name /owned >>]",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    assert_eq!(
        reader.resolve_object((1, 0)).unwrap(),
        Object::Dictionary(dictionary! {
            "Nested" => Object::Dictionary(dictionary! { "Values" => vec![Object::Integer(1), Object::string_literal("two"), Object::Dictionary(dictionary! { "Flag" => true })] })
        })
    );
    assert_eq!(
        reader.resolve_object((2, 0)).unwrap(),
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(2),
            Object::string_literal("three"),
            Object::Dictionary(dictionary! { "Name" => "owned" }),
        ])
    );
}

#[test]
fn shared_batch_deduplicates_and_restores_first_occurrence_order_with_errors() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"(one)",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"[2 (two)]",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let resolved = reader.resolve_many_shared(&[(2, 0), (99, 0), (1, 0), (2, 0), (1, 1)]);

    assert_eq!(
        resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [(2, 0), (99, 0), (1, 0), (1, 1)]
    );
    assert_eq!(resolved[0].1.as_ref().unwrap().as_array().unwrap().len(), 2);
    assert!(matches!(
        resolved[1].1.as_ref().unwrap_err().as_ref(),
        IndexedReaderError::MissingNormalObject { id: (99, 0) }
    ));
    assert_eq!(resolved[2].1.as_ref().unwrap().as_str().unwrap(), b"one");
    assert!(matches!(
        resolved[3].1.as_ref().unwrap_err().as_ref(),
        IndexedReaderError::MissingNormalObjectAtXref {
            id: (1, 1),
            reason: MissingNormalObjectReason::GenerationMismatch {
                requested: (1, 1),
                indexed: 0,
                actual: (1, 0)
            }
        }
    ));
    assert!(reader.resolve_many_shared(&[]).is_empty());
}

#[test]
fn shared_batch_groups_object_stream_reads_and_preserves_member_errors() {
    let members = [
        (10, b"(ten)".as_slice()),
        (11, b"(eleven)".as_slice()),
        (12, b"[12]".as_slice()),
    ];
    let (first, decoded) = object_stream_content(&members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let content = encoder.finish().unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 3 /First {first} /Filter /FlateDecode"),
        &content,
        &[(10, 0), (11, 1), (12, 2), (13, 1)],
    );
    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf,
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0), (13, 0), (11, 0), (10, 0)]);
    let batch_reads = source.requests.lock().unwrap().len();
    assert_eq!(
        resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [(12, 0), (10, 0), (13, 0), (11, 0)]
    );
    assert_eq!(
        resolved[0].1.as_ref().unwrap().as_array().unwrap()[0].as_i64().unwrap(),
        12
    );
    assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"ten");
    assert!(matches!(
        resolved[2].1.as_ref().unwrap_err().as_ref(),
        IndexedReaderError::ObjectStreamMember {
            id: (13, 0),
            container: (5, 0),
            index: 1,
            ..
        }
    ));
    assert_eq!(resolved[3].1.as_ref().unwrap().as_str().unwrap(), b"eleven");

    source.requests.lock().unwrap().clear();
    for id in [(12, 0), (10, 0), (13, 0), (11, 0)] {
        let _ = reader.resolve_object(id);
    }
    let scalar_reads = source.requests.lock().unwrap().len();
    assert!(batch_reads < scalar_reads, "batch={batch_reads}, scalar={scalar_reads}");

    let malformed_header = b"10 0 bad nope 12 6 ";
    let mut malformed_content = malformed_header.to_vec();
    malformed_content.extend_from_slice(b"(ten) (twelve)");
    let malformed = object_stream_fixture(
        &format!("/Type /ObjStm /N 3 /First {}", malformed_header.len()),
        &malformed_content,
        &[(10, 0), (12, 2)],
    );
    let reader = open_reader(&malformed.pdf, ResolverLimits::default());
    let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0)]);
    assert_eq!(resolved[0].1.as_ref().unwrap().as_str().unwrap(), b"twelve");
    assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"ten");

    let duplicate_header = b"10 0 10 6 ";
    let mut duplicate_content = duplicate_header.to_vec();
    duplicate_content.extend_from_slice(b"(one) (two)");
    let duplicate = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {}", duplicate_header.len()),
        &duplicate_content,
        &[(10, 1)],
    );
    let reader = open_reader(&duplicate.pdf, ResolverLimits::default());
    let resolved = reader.resolve_many_shared(&[(10, 0), (10, 0)]);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].1.as_ref().unwrap().as_str().unwrap(), b"two");
}

#[test]
fn shared_batch_propagates_one_container_read_failure_once_with_shared_identity() {
    let members = [
        (10, b"(ten)".as_slice()),
        (11, b"(eleven)".as_slice()),
        (12, b"(twelve)".as_slice()),
    ];
    let (first, content) = object_stream_content(&members);
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 3 /First {first}"),
        &content,
        &[(10, 0), (11, 1), (12, 2)],
    );
    let source = Arc::new(FailOnceSource {
        bytes: fixture.pdf,
        armed: AtomicBool::new(false),
        armed_reads: AtomicUsize::new(0),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.armed_reads.store(0, Ordering::SeqCst);
    source.armed.store(true, Ordering::SeqCst);

    let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0), (11, 0)]);
    let errors: Vec<_> = resolved
        .iter()
        .map(|(_, result)| result.as_ref().unwrap_err())
        .collect();
    assert!(matches!(errors[0].as_ref(), IndexedReaderError::Source(_)));
    assert!(Arc::ptr_eq(errors[0], errors[1]));
    assert!(Arc::ptr_eq(errors[0], errors[2]));
    assert_eq!(source.armed_reads.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_batch_preserves_encryption_for_revisions_two_through_six() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let resolved = reader.resolve_many_shared(&[(2, 0), (1, 0), (2, 0)]);
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved[0].1.as_ref().unwrap().as_stream().unwrap().content,
            b"encrypted stream"
        );
        assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"encrypted string");
    }

    let (pdf, image_plaintext, _) = encrypted_object_stream_pdf(4, false, 0);
    let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
    let resolved = reader.resolve_many_shared(&[(10, 0), (20, 0), (21, 0)]);
    assert_eq!(
        resolved[0]
            .1
            .as_ref()
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Text")
            .unwrap()
            .as_str()
            .unwrap(),
        b"member secret"
    );
    assert_eq!(
        resolved[1].1.as_ref().unwrap().as_stream().unwrap().content,
        image_plaintext
    );
    assert_eq!(resolved[2].1.as_ref().unwrap().as_str().unwrap(), b"normal secret");
}

#[test]
fn shared_batch_limit_errors_match_scalar_and_types_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<IndexedReader>();
    assert_send_sync::<Arc<Object>>();

    let large = format!("({})", "x".repeat(16 * 1_024));
    let (first, decoded) = object_stream_content(&[(10, large.as_bytes()), (11, b"(small)")]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let compressed = encoder.finish().unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {first} /Filter /FlateDecode"),
        &compressed,
        &[(10, 0), (11, 1)],
    );
    let reader = open_reader(
        &fixture.pdf,
        ResolverLimits {
            max_stream_bytes: 1_024,
            ..ResolverLimits::default()
        },
    );
    let batch = reader.resolve_many_shared(&[(11, 0), (10, 0)]);
    for (_, result) in batch {
        assert!(matches!(
            result.as_ref().unwrap_err().as_ref(),
            IndexedReaderError::ObjectStreamBatchSetup { .. }
        ));
    }
    assert!(matches!(
        reader.resolve_object((11, 0)),
        Err(IndexedReaderError::ObjectStreamMember { .. })
    ));
}

#[test]
fn declared_compressed_objects_match_complete_fixture_values() {
    let members = [
        (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
        (11, b"<< /Type /Pages /Count 0 /Kids [] >>".as_slice()),
        (12, b"[1 (two) << /Flag true >>]".as_slice()),
    ];
    let (first, plain) = object_stream_content(&members);
    for compressed in [false, true] {
        let (content, filter) = if compressed {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(&plain).unwrap();
            (encoder.finish().unwrap(), " /Filter /FlateDecode")
        } else {
            (plain.clone(), "")
        };
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N {} /First {first}{filter}", members.len()),
            &content,
            &[(10, 0), (11, 1), (12, 2)],
        );
        let reader = open_reader(&fixture.pdf, ResolverLimits::default());
        assert_eq!(
            reader.resolve_object((10, 0)).unwrap(),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => (11, 0) })
        );
        assert_eq!(
            reader.resolve_object((11, 0)).unwrap(),
            Object::Dictionary(dictionary! { "Type" => "Pages", "Count" => 0, "Kids" => Vec::<Object>::new() })
        );
        assert_eq!(
            reader.resolve_object((12, 0)).unwrap(),
            Object::Array(vec![
                Object::Integer(1),
                Object::string_literal("two"),
                Object::Dictionary(dictionary! { "Flag" => true })
            ])
        );
        assert!(reader.resolve_object((999, 0)).is_err());
    }
}

#[test]
fn compressed_member_enforces_container_index_id_generation_and_shape() {
    let (first, content) = object_stream_content(&[(10, b"(ten)"), (11, b"(eleven)")]);
    let wrong_index = object_stream_fixture(&format!("/Type /ObjStm /N 2 /First {first}"), &content, &[(10, 1)]);
    let reader = open_reader(&wrong_index.pdf, ResolverLimits::default());
    assert!(matches!(
        reader.resolve_object((10, 0)),
        Err(IndexedReaderError::ObjectStreamMember {
            id: (10, 0),
            container: (5, 0),
            index: 1,
            ..
        })
    ));
    assert!(matches!(
        reader.resolve_object((10, 1)),
        Err(IndexedReaderError::GenerationMismatch {
            id: (10, 1),
            indexed: 0
        })
    ));

    let ordinary = object_pdf(&[ObjectDef {
        id: 5,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Not /AStream >>",
    }]);
    let mut reader = open_reader(&ordinary, ResolverLimits::default());
    Arc::get_mut(&mut reader.index)
        .unwrap()
        .locations
        .insert(10, ObjectLocation64::Compressed { container: 5, index: 0 });
    assert!(matches!(
        reader.resolve_object((10, 0)),
        Err(IndexedReaderError::ObjectStreamContainerNotStream {
            id: (10, 0),
            container: (5, 0)
        })
    ));
}

#[test]
fn malformed_compressed_members_keep_fixture_values_and_resource_bounds_still_fail() {
    for n in [2, -1] {
        let fixture = object_stream_fixture(&format!("/Type /ObjStm /N {n} /First 5"), b"10 0 (ten)", &[(10, 0)]);
        assert_eq!(
            open_reader(&fixture.pdf, ResolverLimits::default())
                .resolve_object((10, 0))
                .unwrap(),
            Object::string_literal("ten")
        );
    }

    let equal_offsets = object_stream_fixture(
        "/Type /ObjStm /N 2 /First 10",
        b"10 0 11 0 (shared)",
        &[(10, 0), (11, 1)],
    );
    let reader = open_reader(&equal_offsets.pdf, ResolverLimits::default());
    for id in [(10, 0), (11, 0)] {
        assert_eq!(reader.resolve_object(id).unwrap(), Object::string_literal("shared"));
    }

    let malformed = [
        ("/Type /ObjStm /N 1 /First 1", b"10 0 (ten)".as_slice()),
        ("/Type /ObjStm /N 1 /First 5", b"10 0 << /Broken".as_slice()),
    ];
    for (dictionary, content) in malformed {
        let fixture = object_stream_fixture(dictionary, content, &[(10, 0)]);
        assert!(matches!(
            open_reader(&fixture.pdf, ResolverLimits::default()).resolve_object((10, 0)),
            Err(IndexedReaderError::ObjectStreamMember { .. })
        ));
    }

    let large = format!("({})", "x".repeat(16 * 1_024));
    let (first, decoded) = object_stream_content(&[(10, large.as_bytes())]);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < 1_024);
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
        &compressed,
        &[(10, 0)],
    );
    let reader = open_reader(
        &fixture.pdf,
        ResolverLimits {
            max_stream_bytes: 1_024,
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        reader.resolve_object((10, 0)),
        Err(IndexedReaderError::ObjectStreamMember { .. })
    ));
}

#[test]
fn encrypted_and_object_stream_errors_are_deterministic_across_threads() {
    let encrypted = encrypted_page_tree_pdf();
    for _ in 0..3 {
        let mut threads = Vec::new();
        for _ in 0..4 {
            let encrypted = encrypted.clone();
            threads.push(std::thread::spawn(move || {
                matches!(
                    open_encrypted(&encrypted, Some(b"wrong")),
                    Err(IndexedReaderError::InvalidPassword)
                )
            }));
        }
        assert!(threads.into_iter().all(|thread| thread.join().unwrap()));
    }
    let authenticated = open_encrypted(&encrypted, Some(b"user")).unwrap();
    assert!(authenticated.is_encrypted());
    assert!(authenticated.is_authenticated());
    assert_eq!(authenticated.page_count().unwrap(), 2);
    assert_eq!(authenticated.source_len(), u64::try_from(encrypted.len()).unwrap());

    let malformed = object_stream_fixture("/Type /ObjStm /N 1 /First 5", b"10 0 << /Broken", &[(10, 0)]);
    let reader = Arc::new(open_reader(&malformed.pdf, ResolverLimits::default()));
    let classify = |reader: &IndexedReader| match reader.resolve_object((10, 0)).unwrap_err() {
        IndexedReaderError::ObjectStreamMember {
            id,
            container,
            index,
            source,
        } => (id, container, index, source.to_string()),
        other => panic!("unexpected object-stream error: {other}"),
    };
    let expected = classify(&reader);
    for _ in 0..3 {
        let mut threads = Vec::new();
        for _ in 0..4 {
            let reader = Arc::clone(&reader);
            threads.push(std::thread::spawn(move || classify(&reader)));
        }
        assert!(threads.into_iter().all(|thread| thread.join().unwrap() == expected));
    }
}

#[test]
fn fw9_style_compressed_members_match_generated_fixture_values() {
    let bodies: Vec<_> = (10..110)
        .map(|id| format!("<< /T (field-{id}) /V ({}) /Rect [0 0 100 20] >>", id * 17))
        .collect();
    let members: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| (u32::try_from(index).unwrap() + 10, body.as_bytes()))
        .collect();
    let (first, plain) = object_stream_content(&members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 100 /First {first} /Filter /FlateDecode"),
        &encoder.finish().unwrap(),
        &(10..110).map(|id| (id, id - 10)).collect::<Vec<_>>(),
    );
    let reader = open_reader(&fixture.pdf, ResolverLimits::default());
    for id in 10..110 {
        assert_eq!(
            reader.resolve_object((id, 0)).unwrap(),
            Object::Dictionary(dictionary! {
                "T" => Object::string_literal(format!("field-{id}")),
                "V" => Object::string_literal((id * 17).to_string()),
                "Rect" => vec![0.into(), 0.into(), 100.into(), 20.into()],
            })
        );
    }
}

#[test]
fn degraded_stream_position_is_relative_to_pdf_origin() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /MissingLength true >>\nstream\nignored\nendstream",
    }]);
    let prefix = b"twenty-nine-byte-prefix.....\n";
    assert_eq!(prefix.len(), 29);
    let mut prefixed = prefix.to_vec();
    prefixed.extend_from_slice(&pdf);

    let resolved = open_reader(&prefixed, ResolverLimits::default())
        .resolve_object((1, 0))
        .unwrap();
    assert_eq!(
        resolved.as_stream().unwrap().dict,
        dictionary! { "MissingLength" => true }
    );
    assert!(resolved.as_stream().unwrap().content.is_empty());
    let physical_stream_start = prefixed
        .windows(b"stream\n".len())
        .position(|window| window == b"stream\n")
        .unwrap()
        + b"stream\n".len();
    assert_eq!(
        resolved.as_stream().unwrap().start_position.unwrap() + prefix.len(),
        physical_stream_start
    );
}

#[test]
fn parsed_normal_header_is_authoritative_over_xref_generation() {
    let definitions = [
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 1,
            body: b"<< /Type /Catalog /Pages 2 0 R >>",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
    ];
    let pdf = object_pdf(&definitions);
    let catalog = Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => (2, 0) });

    let reader = Arc::new(open_reader(&pdf, ResolverLimits::default()));
    for _ in 0..3 {
        assert_eq!(reader.resolve_object((1, 0)).unwrap(), catalog.clone());
        assert_eq!(*reader.resolve_object_shared((1, 0)).unwrap(), catalog.clone());
        assert_eq!(reader.page_map().unwrap().pages[0].id, (3, 0));
        assert!(matches!(
            reader.resolve_object((1, 1)),
            Err(IndexedReaderError::MissingNormalObjectAtXref {
                id: (1, 1),
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: (1, 1),
                    indexed: 1,
                    actual: (1, 0)
                }
            })
        ));
        assert!(matches!(
            reader.resolve_object_shared((1, 1)).unwrap_err().as_ref(),
            IndexedReaderError::MissingNormalObjectAtXref {
                id: (1, 1),
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: (1, 1),
                    indexed: 1,
                    actual: (1, 0)
                }
            }
        ));
    }

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let reader = Arc::clone(&reader);
            std::thread::spawn(move || {
                (
                    reader.resolve_object((1, 0)).unwrap(),
                    reader.page_map().unwrap().pages[0].id,
                    reader.resolve_object((1, 1)).unwrap_err().to_string(),
                )
            })
        })
        .collect();
    let expected_missing = reader.resolve_object((1, 1)).unwrap_err().to_string();
    for thread in threads {
        let (object, page, missing) = thread.join().unwrap();
        assert_eq!(object, catalog);
        assert_eq!(page, (3, 0));
        assert_eq!(missing, expected_missing);
    }

    let missing_root = object_pdf_with_root(&definitions, (1, 1));
    let reader = open_reader(&missing_root, ResolverLimits::default());
    assert!(matches!(
        reader.resolve_object((1, 1)),
        Err(IndexedReaderError::MissingNormalObjectAtXref {
            id: (1, 1),
            reason: MissingNormalObjectReason::GenerationMismatch {
                requested: (1, 1),
                indexed: 1,
                actual: (1, 0)
            }
        })
    ));
    assert!(reader.page_map().unwrap().is_empty());

    let mismatch = xref_number_header_mismatch_fixture();
    let reader = open_reader(&mismatch, ResolverLimits::default());
    assert!(matches!(
        reader.resolve_object((1, 0)),
        Err(IndexedReaderError::MissingNormalObjectAtXref {
            id: (1, 0),
            reason: MissingNormalObjectReason::HeaderMismatch {
                expected: (1, 0),
                actual: (2, 0)
            }
        })
    ));
    assert!(reader.page_map().unwrap().is_empty());
}

#[test]
fn raw_compressed_generation_mismatch_is_not_a_semantic_page_map_omission() {
    let (first, content) = object_stream_content(&[(10, b"<< /Type /Catalog >>")]);
    let mut fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
    let marker = b"/Root 10 0 R";
    let root = fixture
        .pdf
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    fixture.pdf[root + b"/Root 10 ".len()] = b'1';

    let reader = open_reader(&fixture.pdf, ResolverLimits::default());
    assert!(matches!(
        reader.resolve_object((10, 1)),
        Err(IndexedReaderError::GenerationMismatch {
            id: (10, 1),
            indexed: 0
        })
    ));
    assert!(matches!(
        reader.page_map(),
        Err(IndexedReaderError::GenerationMismatch {
            id: (10, 1),
            indexed: 0
        })
    ));
}

#[test]
fn malformed_normal_xref_header_probe_reports_fixture_omission() {
    let pdf = nul_header_probe_fixture();

    let classify = |error: &IndexedReaderError| match error {
        IndexedReaderError::MissingNormalObjectAtXref {
            id,
            reason: MissingNormalObjectReason::HeaderProbeLimit { offset, limit },
        } => (*id, *offset, *limit),
        other => panic!("unexpected malformed-normal-object error: {other}"),
    };
    let reader = Arc::new(open_reader(&pdf, ResolverLimits::default()));
    let expected = classify(&reader.resolve_object((1, 0)).unwrap_err());
    assert_eq!(expected.0, (1, 0));
    assert_eq!(expected.2, INDIRECT_HEADER_LIMIT);
    for _ in 0..3 {
        assert_eq!(classify(&reader.resolve_object((1, 0)).unwrap_err()), expected);
        assert_eq!(
            classify(reader.resolve_object_shared((1, 0)).unwrap_err().as_ref()),
            expected
        );
    }
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let reader = Arc::clone(&reader);
            std::thread::spawn(move || classify(&reader.resolve_object((1, 0)).unwrap_err()))
        })
        .collect();
    assert!(threads.into_iter().all(|thread| thread.join().unwrap() == expected));
}

#[test]
fn xref_offset_inside_object_body_reports_fixture_omission() {
    const NASA_OBJECT_ID: u32 = 34_472;
    // The observed file records an offset exactly twelve bytes after the
    // matching `34472 0 obj` header, at the first byte of the object body.
    let pdf = malformed_normal_xref_fixture(NASA_OBJECT_ID, 12, b"[/Indexed 34471 0 R 255 34473 0 R]");

    let reader = open_reader(&pdf, ResolverLimits::default());
    for _ in 0..3 {
        assert!(matches!(
            reader.resolve_object((NASA_OBJECT_ID, 0)),
            Err(IndexedReaderError::MissingNormalObjectAtXref {
                id: (NASA_OBJECT_ID, 0),
                reason: MissingNormalObjectReason::HeaderProbeLimit { .. }
            })
        ));
        assert!(matches!(
            reader.resolve_object_shared((NASA_OBJECT_ID, 0)).unwrap_err().as_ref(),
            IndexedReaderError::MissingNormalObjectAtXref {
                id: (NASA_OBJECT_ID, 0),
                reason: MissingNormalObjectReason::HeaderProbeLimit { .. }
            }
        ));
    }
}

#[test]
fn scalar_before_reference_in_array_matches_the_nasa_fixture_shape() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"[/Indexed 2 0 R 255 3 0 R]",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"/DeviceRGB",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 0 >>\nstream\n\nendstream",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    assert_eq!(
        reader.resolve_object((1, 0)).unwrap(),
        Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Reference((2, 0)),
            Object::Integer(255),
            Object::Reference((3, 0))
        ])
    );
    assert!(matches!(
        reader.resolve_stream_descriptor((1, 0)),
        Err(IndexedStreamReadError::NotStream { id: (1, 0) })
    ));
}

#[test]
fn large_object_stream_is_call_local_owned_and_uncached() {
    let large = format!("({})", "z".repeat(4 * 1_024 * 1_024));
    let (first, content) = object_stream_content(&[(10, b"(tiny)"), (11, large.as_bytes())]);
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {first}"),
        &content,
        &[(10, 0), (11, 1)],
    );
    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf,
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let first_value = reader.resolve_object((10, 0)).unwrap();
    let second_value = reader.resolve_object((10, 0)).unwrap();
    assert_eq!(first_value, Object::string_literal("tiny"));
    assert_eq!(second_value, first_value);
    let requests = source.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(offset, length)| {
                *offset == fixture.container_stream_start
                    && u64::try_from(*length).unwrap() == fixture.container_stream_length
            })
            .count(),
        2
    );
    assert!(
        requests
            .iter()
            .all(|(_, length)| u64::try_from(*length).unwrap() <= fixture.container_stream_length)
    );
    drop(requests);
    drop(reader);
    drop(source);
    assert_eq!(first_value, Object::string_literal("tiny"));
}

#[test]
fn public_object_ids_and_bounded_owner_cover_declared_objstm_members() {
    let (first, content) = object_stream_content(&[(10, b"<< /Answer 42 >>"), (11, b"(second member)")]);
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {first}"),
        &content,
        &[(10, 0), (11, 1)],
    );
    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();

    assert_eq!(reader.object_ids(), vec![(5, 0), (6, 0), (10, 0), (11, 0)]);
    let IndexedObjectLocation::Compressed { container, index } = reader.object_location((10, 0)).unwrap() else {
        panic!("member 10 was not declared compressed")
    };
    assert_eq!(container, (5, 0));
    assert_eq!(index, 0);
    let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let prepared = reader.prepare_object_stream_with_permit(container, &permit).unwrap();
    let reads_after_prepare = source.requests.lock().unwrap().len();
    let bounded = prepared.resolve_member((10, 0), index).unwrap();
    assert_eq!(
        bounded
            .as_object()
            .as_dict()
            .unwrap()
            .get(b"Answer")
            .unwrap()
            .as_i64()
            .unwrap(),
        42
    );
    assert_eq!(
        permit.stats().current_bytes,
        prepared.retained_bytes() + bounded.retained_bytes()
    );
    drop(bounded);
    let IndexedObjectLocation::Compressed { container, index } = reader.object_location((11, 0)).unwrap() else {
        panic!("member 11 was not declared compressed")
    };
    assert_eq!(container, prepared.container_id());
    let second = prepared.resolve_member((11, 0), index).unwrap();
    assert_eq!(second.as_object().as_str().unwrap(), b"second member");
    assert_eq!(source.requests.lock().unwrap().len(), reads_after_prepare);
    drop(second);
    assert_eq!(permit.stats().current_bytes, prepared.retained_bytes());
    drop(prepared);
    assert_eq!(permit.stats().current_bytes, 0);
}

#[test]
fn encrypted_raw_trailer_lookup_never_resolves_or_reads_reference_targets() {
    let source = Arc::new(TracingBytesSource {
        bytes: encrypted_pdf_with_stream(6, "owner", "user", b"encrypted payload"),
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

    assert_eq!(reader.trailer_entry_raw_owned(b"Root"), Some(Object::Reference((3, 0))));
    assert!(matches!(
        reader.trailer_entry_raw_owned(b"Encrypt"),
        Some(Object::Reference(_))
    ));
    assert!(matches!(reader.trailer_entry_raw_owned(b"ID"), Some(Object::Array(_))));
    assert_eq!(reader.trailer_entry_raw_owned(b"Missing"), None);
    assert!(source.requests.lock().unwrap().is_empty());
}
