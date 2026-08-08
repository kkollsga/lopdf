//! Indexed-reader tests: streams.

use super::*;

#[test]
fn direct_indirect_and_nested_lengths_read_exact_owned_content() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 3 0 R >>\nstream\nworld\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"5",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 0 R >>\nstream\nabcde\nendstream",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"6 0 R",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"5",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let eager = Document::load_mem(&pdf).unwrap();

    for (id, expected) in [((1, 0), b"hello".as_slice()), ((2, 0), b"world"), ((4, 0), b"abcde")] {
        let resolved = reader.resolve_object(id).unwrap();
        assert_eq!(resolved.as_stream().unwrap().content, expected);
        assert_eq!(
            resolved.as_stream().unwrap().content,
            eager.get_object(id).unwrap().as_stream().unwrap().content
        );
    }

    let descriptor_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let descriptor = reader
        .resolve_stream_descriptor_with_permit((2, 0), &descriptor_permit)
        .unwrap();
    assert_eq!(descriptor.encoded_len(), Some(5));
    assert!(descriptor_permit.stats().current_bytes > 0);
    let descriptor_peak = descriptor_permit.stats().peak_bytes;
    drop(descriptor);
    assert_eq!(descriptor_permit.close().unwrap().current_bytes, 0);

    let refused = crate::ScalarResolutionPermit::new(descriptor_peak - 1);
    assert!(matches!(
        reader.resolve_stream_descriptor_with_permit((2, 0), &refused),
        Err(IndexedStreamReadError::Resolve(
            IndexedReaderError::ScalarResourceLimit { .. }
        ))
    ));
    assert_eq!(refused.stats().current_bytes, 0);
    refused.close().unwrap();

    let stream_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let stream = reader.resolve_stream_with_permit((4, 0), &stream_permit).unwrap();
    assert_eq!(stream.as_stream().content, b"abcde");
    drop(stream);
    assert_eq!(stream_permit.close().unwrap().current_bytes, 0);
}

#[test]
fn stream_descriptors_distinguish_known_zero_from_unavailable_lengths() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Kind /Direct >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 3 0 R /Kind /Indirect >>\nstream\nworld\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"5",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 0 /Kind /Zero >>\nstream\n\nendstream",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Kind /Missing >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length (bad) /Kind /Malformed >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 7,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 8 0 R /Kind /Cycle >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 8,
            object_generation: 0,
            xref_generation: 0,
            body: b"7 0 R",
        },
        ObjectDef {
            id: 9,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 99 >>\nstream\nshort\nendstream",
        },
        ObjectDef {
            id: 10,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 99 0 R /Kind /Dangling >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 11,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 12 0 R /Kind /Depth >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 12,
            object_generation: 0,
            xref_generation: 0,
            body: b"13 0 R",
        },
        ObjectDef {
            id: 13,
            object_generation: 0,
            xref_generation: 0,
            body: b"7",
        },
        ObjectDef {
            id: 14,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Kind /MissingEndstream >>\nstream\nhello",
        },
    ]);
    let eager = Document::load_mem(&pdf).unwrap();
    let source = Arc::new(LengthTracingBytesSource {
        bytes: pdf.clone(),
        len_calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    for id in [1, 2, 4] {
        let scalar = reader.resolve_object((id, 0)).unwrap();
        let scalar = scalar.as_stream().unwrap();
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
        assert_eq!(
            descriptor.dictionary().get(b"Kind").unwrap(),
            scalar.dict.get(b"Kind").unwrap()
        );
        let expected = u64::try_from(scalar.content.len()).unwrap();
        assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Known(expected));
        assert_eq!(descriptor.encoded_len(), Some(expected));
        assert_eq!(read_all_encoded(&descriptor, 3), scalar.content);
        assert_eq!(
            reader.resolve_object((id, 0)).unwrap(),
            eager.get_object((id, 0)).unwrap().clone()
        );
    }
    for id in [5, 6, 7, 10] {
        let scalar = reader.resolve_object((id, 0)).unwrap();
        assert!(scalar.as_stream().unwrap().content.is_empty());
        assert_eq!(scalar, eager.get_object((id, 0)).unwrap().clone());
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
        let reason = EncodedStreamLengthUnavailableReason::MissingOrInvalid;
        assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Unavailable(reason));
        assert_eq!(descriptor.encoded_len(), None);
        source.requests.lock().unwrap().clear();
        source.len_calls.store(0, Ordering::SeqCst);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::LengthUnavailable {
                id: error_id,
                reason: EncodedStreamLengthUnavailableReason::MissingOrInvalid,
            }) if error_id == (id, 0)
        ));
        assert_eq!(source.len_calls.load(Ordering::SeqCst), 0);
        assert!(source.requests.lock().unwrap().is_empty());
    }
    assert_eq!(
        reader
            .resolve_stream_descriptor((2, 0))
            .unwrap()
            .dictionary()
            .get(b"Length")
            .unwrap(),
        &Object::Reference((3, 0))
    );
    assert!(matches!(reader.resolve_object((9, 0)).unwrap(), Object::Dictionary(_)));
    assert!(matches!(
        reader.resolve_stream_descriptor((9, 0)),
        Err(IndexedStreamReadError::NotStream { id: (9, 0) })
    ));
    assert!(matches!(reader.resolve_object((14, 0)).unwrap(), Object::Dictionary(_)));
    assert!(matches!(
        reader.resolve_stream_descriptor((14, 0)),
        Err(IndexedStreamReadError::NotStream { id: (14, 0) })
    ));
    assert!(matches!(
        reader.resolve_stream_descriptor((3, 0)),
        Err(IndexedStreamReadError::NotStream { id: (3, 0) })
    ));
    assert!(matches!(
        reader.resolve_stream_descriptor((99, 0)),
        Err(IndexedStreamReadError::Resolve(
            IndexedReaderError::MissingNormalObject { id: (99, 0) }
        ))
    ));

    let limited_source = Arc::new(LengthTracingBytesSource {
        bytes: pdf,
        len_calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let limited = IndexedReader::open_with_limits(
        limited_source.clone(),
        ResolverLimits {
            max_length_depth: 1,
            ..ResolverLimits::default()
        },
    )
    .unwrap();
    let scalar = limited.resolve_object((11, 0)).unwrap();
    assert!(scalar.as_stream().unwrap().content.is_empty());
    let descriptor = limited.resolve_stream_descriptor((11, 0)).unwrap();
    let reason = EncodedStreamLengthUnavailableReason::MissingOrInvalid;
    assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Unavailable(reason));
    limited_source.requests.lock().unwrap().clear();
    limited_source.len_calls.store(0, Ordering::SeqCst);
    assert!(matches!(
        descriptor.open_plain_encoded(),
        Err(IndexedStreamReadError::LengthUnavailable {
            id: (11, 0),
            reason: EncodedStreamLengthUnavailableReason::MissingOrInvalid,
        })
    ));
    assert_eq!(limited_source.len_calls.load(Ordering::SeqCst), 0);
    assert!(limited_source.requests.lock().unwrap().is_empty());
}

#[test]
fn stream_descriptors_separate_encoded_span_policy_from_materialized_stream_limit() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length -1 >>\nstream\n\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 20 >>\nstream\n01234567890123456789\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 >>\nstream\nhello\nendst",
        },
    ]);
    let reader = open_reader(
        &pdf,
        ResolverLimits {
            max_stream_bytes: 10,
            max_endstream_tail_bytes: 6,
            ..ResolverLimits::default()
        },
    );
    for id in [1, 3] {
        let scalar = reader.resolve_object((id, 0)).unwrap_err().to_string();
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap_err().to_string();
        assert!(
            descriptor.contains(&scalar),
            "scalar={scalar:?}, descriptor={descriptor:?}"
        );
    }
    let stream_reader = open_reader(
        &pdf,
        ResolverLimits {
            max_stream_bytes: 10,
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        stream_reader.resolve_object((2, 0)),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (2, 0),
            length: 20,
            limit: 10,
        })
    ));
    let descriptor = stream_reader.resolve_stream_descriptor((2, 0)).unwrap();
    assert_eq!(read_all_encoded(&descriptor, 64 * 1_024), b"01234567890123456789");
}

#[test]
fn encoded_stream_cap_is_inclusive_and_wired_to_both_descriptor_paths() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Length 5 >>\nstream\nhello\nendstream",
    }]);

    let below = IndexedReader::open_with_options(
        BytesSource::from(pdf.clone()),
        IndexedReaderOptions {
            stream_bytes: 4,
            encoded_stream_bytes: Some(4),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    assert!(matches!(
        below.resolve_stream_descriptor((1, 0)),
        Err(IndexedStreamReadError::EncodedStreamLimitExceeded {
            id: (1, 0),
            length: 5,
            limit: 4,
        })
    ));
    let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    assert!(matches!(
        below.resolve_stream_descriptor_with_permit((1, 0), &permit),
        Err(IndexedStreamReadError::EncodedStreamLimitExceeded {
            id: (1, 0),
            length: 5,
            limit: 4,
        })
    ));
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
    assert!(matches!(
        below.resolve_object((1, 0)),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (1, 0),
            length: 5,
            limit: 4,
        })
    ));

    for limit in [Some(5), Some(6), None] {
        let reader = IndexedReader::open_with_options(
            BytesSource::from(pdf.clone()),
            IndexedReaderOptions {
                stream_bytes: 4,
                encoded_stream_bytes: limit,
                ..IndexedReaderOptions::default()
            },
        )
        .unwrap();
        let descriptor = reader.resolve_stream_descriptor((1, 0)).unwrap();
        assert_eq!(read_all_encoded(&descriptor, 2), b"hello");

        let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
        let descriptor = reader.resolve_stream_descriptor_with_permit((1, 0), &permit).unwrap();
        assert_eq!(descriptor.encoded_len(), Some(5));
        drop(descriptor);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }
}

#[test]
fn flate_descriptor_streams_raw_encoded_bytes_without_weakening_materialized_cap() {
    let plain = std::iter::repeat_n(b'x', 128 * 1_024).collect::<Vec<_>>();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let encoded = encoder.finish().unwrap();
    assert!(encoded.len() > 4);

    let mut body = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", encoded.len()).into_bytes();
    body.extend_from_slice(&encoded);
    body.extend_from_slice(b"\nendstream");
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: &body,
    }]);
    let reader = IndexedReader::open_with_options(
        BytesSource::from(pdf),
        IndexedReaderOptions {
            stream_bytes: 4,
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();

    assert!(matches!(
        reader.resolve_object((1, 0)),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (1, 0),
            limit: 4,
            ..
        })
    ));
    let descriptor = reader.resolve_stream_descriptor((1, 0)).unwrap();
    assert_eq!(descriptor.protection(), EncodedStreamProtection::Plain);
    assert_eq!(read_all_encoded(&descriptor, 256 * 1_024), encoded);
}

#[test]
fn stream_descriptor_rejects_objstm_and_protected_payloads_before_open_read() {
    let (first, content) = object_stream_content(&[(10, b"(member)")]);
    let fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
    let reader = open_reader(&fixture.pdf, ResolverLimits::default());
    assert!(matches!(
        reader.resolve_stream_descriptor((10, 0)),
        Err(IndexedStreamReadError::NotNormalObject { id: (10, 0) })
    ));

    let filters = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 2 0 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"/Crypt",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 4 0 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"[/FlateDecode 5 0 R]",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"/Crypt",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter [7 0 R /FlateDecode] >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 7,
            object_generation: 0,
            xref_generation: 0,
            body: b"/ASCIIHexDecode",
        },
        ObjectDef {
            id: 8,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 99 0 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 9,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 10 0 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 10,
            object_generation: 0,
            xref_generation: 0,
            body: b"9 0 R",
        },
        ObjectDef {
            id: 11,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 12 1 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 12,
            object_generation: 0,
            xref_generation: 0,
            body: b"/Crypt",
        },
        ObjectDef {
            id: 13,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 42 >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 14,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter /Crypt /DecodeParms << /Name /Identity >> >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 15,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter [16 0 R /FlateDecode] >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 16,
            object_generation: 0,
            xref_generation: 0,
            body: b"/Crypt",
        },
        ObjectDef {
            id: 17,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter [/FlateDecode 99 0 R] >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 18,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Filter 19 0 R >>\nstream\nhello\nendstream",
        },
        ObjectDef {
            id: 19,
            object_generation: 0,
            xref_generation: 0,
            body: b"[/FlateDecode /ASCIIHexDecode]",
        },
    ]);
    let reader = open_reader(&filters, ResolverLimits::default());
    for id in [1, 3, 14, 15] {
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
        assert_eq!(descriptor.protection(), EncodedStreamProtection::CryptFilter);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::Protected {
                protection: EncodedStreamProtection::CryptFilter,
                ..
            })
        ));
    }
    for id in [8, 9, 11, 13, 17] {
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
        assert_eq!(descriptor.protection(), EncodedStreamProtection::UnresolvedFilter);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::Protected {
                protection: EncodedStreamProtection::UnresolvedFilter,
                ..
            })
        ));
    }
    for id in [6, 18] {
        let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
        assert_eq!(descriptor.protection(), EncodedStreamProtection::Plain);
        assert_eq!(read_all_encoded(&descriptor, 64 * 1_024), b"hello");
    }

    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let descriptor = reader.resolve_stream_descriptor((2, 0)).unwrap();
        assert!(descriptor.dictionary().has_type(b"Metadata"));
        assert_eq!(descriptor.protection(), EncodedStreamProtection::DocumentEncrypted);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::Protected {
                protection: EncodedStreamProtection::DocumentEncrypted,
                ..
            })
        ));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn stream_descriptor_physical_reads_fail_closed_after_detectable_mutation() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Length 5 >>\nstream\nhello\nendstream",
    }]);
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&pdf).unwrap();
    file.as_file_mut().sync_all().unwrap();
    let reader = IndexedReader::open(FileSource::open(file.path()).unwrap()).unwrap();
    let descriptor = Arc::new(reader.resolve_stream_descriptor((1, 0)).unwrap());

    file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
    let mut changed = pdf.clone();
    changed[0] = b'!';
    file.write_all(&changed).unwrap();
    file.as_file_mut().sync_all().unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(5));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let descriptor = Arc::clone(&descriptor);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                match descriptor.open_plain_encoded() {
                    Ok(mut stream) => stream.read_chunk(&mut [0; 5]),
                    Err(error) => Err(error),
                }
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        assert!(matches!(
            worker.join().unwrap(),
            Err(IndexedStreamReadError::Source(SourceError::SourceChanged))
        ));
    }
}

#[test]
fn stream_descriptor_never_reads_compressed_filter_metadata_before_lease() {
    const OBJECT_ID: u32 = 1;
    const CONTAINER_ID: u32 = 5;
    const XREF_ID: u32 = 6;
    const FILTER_ID: u32 = 10;

    let object_offset = 1_024_u64;
    let object =
        format!("{OBJECT_ID} 0 obj\n<< /Length 5 /Filter {FILTER_ID} 0 R >>\nstream\nhello\nendstream\nendobj\n")
            .into_bytes();

    let container_offset = 1_024_u64 * 1_024;
    let container_stream_length = 64_u64 * 1_024 * 1_024 - 1;
    let container_prefix =
        format!("{CONTAINER_ID} 0 obj\n<< /Type /ObjStm /N 1 /First 5 /Length {container_stream_length} >>\nstream\n")
            .into_bytes();
    let container_stream_start = container_offset + u64::try_from(container_prefix.len()).unwrap();
    let container_stream_end = container_stream_start + container_stream_length;
    let container_suffix = b"\nendstream\nendobj\n".to_vec();

    let xref_offset = container_stream_end + 1_024_u64 * 1_024;
    let mut xref_content = Vec::new();
    for id in 0..=FILTER_ID {
        match id {
            OBJECT_ID => {
                encode_field(1, 1, &mut xref_content);
                encode_field(object_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
            CONTAINER_ID => {
                encode_field(1, 1, &mut xref_content);
                encode_field(container_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
            XREF_ID => {
                encode_field(1, 1, &mut xref_content);
                encode_field(xref_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
            FILTER_ID => {
                encode_field(2, 1, &mut xref_content);
                encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
            _ => {
                encode_field(0, 1, &mut xref_content);
                encode_field(0, 8, &mut xref_content);
                encode_field(if id == 0 { 65_535 } else { 0 }, 4, &mut xref_content);
            }
        }
    }
    let mut xref = format!(
        "{XREF_ID} 0 obj\n<< /Type /XRef /Size {} /Root {OBJECT_ID} 0 R /W [1 8 4] /Length {} >>\nstream\n",
        FILTER_ID + 1,
        xref_content.len()
    )
    .into_bytes();
    xref.extend_from_slice(&xref_content);
    xref.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
    let source_len = xref_offset + u64::try_from(xref.len()).unwrap();
    let container_region_end = container_stream_end + u64::try_from(container_suffix.len()).unwrap();

    let source = Arc::new(OverlaySource {
        len: source_len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (object_offset, object),
            (container_offset, container_prefix),
            (container_stream_start, b"10 0 /Crypt".to_vec()),
            (container_stream_end, container_suffix),
            (xref_offset, xref),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let descriptor = reader.resolve_stream_descriptor((OBJECT_ID, 0)).unwrap();
    assert_eq!(descriptor.protection(), EncodedStreamProtection::UnresolvedFilter);
    assert!(matches!(
        descriptor.open_plain_encoded(),
        Err(IndexedStreamReadError::Protected {
            protection: EncodedStreamProtection::UnresolvedFilter,
            ..
        })
    ));

    let requests = source.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|(offset, length)| {
        let end = offset.saturating_add(u64::try_from(*length).unwrap_or(u64::MAX));
        end <= container_offset || *offset >= container_region_end
    }));
}

#[test]
fn stream_keyword_split_across_initial_window_grows_before_classifying_dictionary() {
    let target = usize::try_from(INITIAL_OBJECT_WINDOW).unwrap() - 3;
    let prefix = b"<< /Pad (";
    let suffix = b") /Length 5 >>\nstream\nhello\nendstream";
    let padding = target - prefix.len() - (b") /Length 5 >>\n").len();
    let mut body = prefix.to_vec();
    body.resize(body.len() + padding, b'x');
    body.extend_from_slice(suffix);
    assert_eq!(
        body.windows(b"stream".len()).position(|window| window == b"stream"),
        Some(target)
    );
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: &body,
    }]);

    let stream = open_reader(&pdf, ResolverLimits::default())
        .resolve_object((1, 0))
        .unwrap();
    assert_eq!(stream.as_stream().unwrap().content, b"hello");
}

#[test]
fn missing_malformed_cyclic_and_deep_lengths_degrade_to_empty_streams() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length (bad) >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 99 0 R >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 0 R >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"6 0 R",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"5 0 R",
        },
        ObjectDef {
            id: 7,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 8 0 R >>\nstream\nignored\nendstream",
        },
        ObjectDef {
            id: 8,
            object_generation: 0,
            xref_generation: 0,
            body: b"9 0 R",
        },
        ObjectDef {
            id: 9,
            object_generation: 0,
            xref_generation: 0,
            body: b"10 0 R",
        },
        ObjectDef {
            id: 10,
            object_generation: 0,
            xref_generation: 0,
            body: b"7",
        },
    ]);
    let reader = open_reader(
        &pdf,
        ResolverLimits {
            max_length_depth: 3,
            ..ResolverLimits::default()
        },
    );
    let eager = Document::load_mem(&pdf).unwrap();

    for id in [(1, 0), (2, 0), (3, 0), (4, 0)] {
        let resolved = reader.resolve_object(id).unwrap();
        assert_eq!(&resolved, eager.get_object(id).unwrap());
        assert!(resolved.as_stream().unwrap().content.is_empty());
    }
    assert!(
        reader
            .resolve_object((7, 0))
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .is_empty()
    );
    assert_eq!(
        eager.get_object((7, 0)).unwrap().as_stream().unwrap().content,
        b"ignored"
    );
}

#[test]
fn negative_and_explicit_stream_resource_limits_fail() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length -1 >>\nstream\nvalue\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 3 0 R >>\nstream\nvalue\nendstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"-1",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 >>\nstream\nvalue\nendstream",
        },
    ]);
    let default_reader = open_reader(&pdf, ResolverLimits::default());
    for id in [(1, 0), (2, 0)] {
        assert!(matches!(
            default_reader.resolve_object(id),
            Err(IndexedReaderError::NegativeStreamLength { id: actual, length: -1 }) if actual == id
        ));
    }
    let limited_reader = open_reader(
        &pdf,
        ResolverLimits {
            max_stream_bytes: 4,
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        limited_reader.resolve_object((4, 0)),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (4, 0),
            length: 5,
            limit: 4
        })
    ));

    let tail_limited_reader = open_reader(
        &pdf,
        ResolverLimits {
            max_endstream_tail_bytes: 4,
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        tail_limited_reader.resolve_object((4, 0)),
        Err(IndexedReaderError::MissingEndstream { id: (4, 0) })
    ));
}

#[test]
fn five_kib_stream_dictionary_extends_without_prefix_or_payload_reread() {
    let len = 100_u64 * 1_024 * 1_024;
    let object_offset = 1_024_u64 * 1_024;
    let stream_length = 8_u64 * 1_024 * 1_024;
    let mut object_prefix = b"1 0 obj\n<< /Pad (".to_vec();
    object_prefix.resize(object_prefix.len() + 5 * 1_024, b'x');
    object_prefix.extend_from_slice(format!(") /Length {stream_length} >>\nstream\n").as_bytes());
    let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
    let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
    let stream_end = stream_start + stream_length;
    let xref = len - 512;
    let xref_bytes = format!(
        "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (object_offset, object_prefix),
            (stream_end, b"\nendstream\nendobj\n".to_vec()),
            (xref, xref_bytes),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

    let stream = reader.resolve_object((1, 0)).unwrap();
    assert_eq!(
        u64::try_from(stream.as_stream().unwrap().content.len()).unwrap(),
        stream_length
    );

    let requests = source.requests.lock().unwrap();
    let body_requests: Vec<_> = requests
        .iter()
        .filter(|(offset, _)| *offset >= body_offset && *offset < stream_start)
        .copied()
        .collect();
    assert_eq!(
        body_requests,
        vec![
            (body_offset, usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()),
            (
                body_offset + INITIAL_OBJECT_WINDOW,
                usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()
            )
        ]
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(offset, length)| { *offset == stream_start && u64::try_from(*length).unwrap() == stream_length })
            .count(),
        1
    );
    OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));
}

#[test]
fn large_image_stream_is_read_once_at_its_exact_declared_length() {
    let len = 500_u64 * 1_024 * 1_024;
    let object_offset = 1_024_u64 * 1_024;
    let stream_length = 8_u64 * 1_024 * 1_024;
    let object_prefix =
        format!("1 0 obj\n<< /Type /XObject /Subtype /Image /Length {stream_length} >>\nstream\n").into_bytes();
    let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
    let stream_end = stream_start + stream_length;
    let xref = len - 512;
    let xref_bytes = format!(
        "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (object_offset, object_prefix),
            (stream_end, b"\nendstream\nendobj\n".to_vec()),
            (xref, xref_bytes),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let image = reader.resolve_object((1, 0)).unwrap();
    assert_eq!(
        u64::try_from(image.as_stream().unwrap().content.len()).unwrap(),
        stream_length
    );

    let requests = source.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(offset, length)| { *offset == stream_start && u64::try_from(*length).unwrap() == stream_length })
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .all(|(_, length)| u64::try_from(*length).unwrap() <= stream_length)
    );
    let total: u64 = requests.iter().map(|(_, length)| u64::try_from(*length).unwrap()).sum();
    assert!(total <= stream_length + 8 * 1_024);
}

#[test]
fn hundred_megabyte_stream_descriptor_has_bounded_lookahead_and_chunk_reads() {
    let source_len = 200_u64 * 1_024 * 1_024;
    let object_offset = 1_024_u64 * 1_024;
    let stream_length = 100_u64 * 1_024 * 1_024;
    let object_prefix =
        format!("1 0 obj\n<< /Type /XObject /Subtype /Image /Length {stream_length} >>\nstream\n").into_bytes();
    let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
    let stream_end = stream_start + stream_length;
    let xref = source_len - 512;
    let xref_bytes = format!(
        "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .into_bytes();
    let source = Arc::new(OverlaySource {
        len: source_len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (object_offset, object_prefix),
            (stream_end, b"\nendstream\nendobj\n".to_vec()),
            (xref, xref_bytes),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(
        source.clone(),
        ResolverLimits {
            max_stream_bytes: 4 * 1_024 * 1_024,
            ..ResolverLimits::default()
        },
    )
    .unwrap();
    source.requests.lock().unwrap().clear();

    assert!(matches!(
        reader.resolve_object((1, 0)),
        Err(IndexedReaderError::StreamLimitExceeded {
            id: (1, 0),
            length,
            limit,
        }) if length == stream_length && limit == 4 * 1_024 * 1_024
    ));
    assert!(
        !source
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(offset, _)| *offset >= stream_start && *offset < stream_end),
        "materialized limit refusal issued a payload read"
    );
    source.requests.lock().unwrap().clear();

    let descriptor = reader.resolve_stream_descriptor((1, 0)).unwrap();
    assert_eq!(descriptor.encoded_len(), Some(stream_length));
    let metadata_requests = source.requests.lock().unwrap().clone();
    assert!(metadata_requests.iter().all(|(_, length)| *length <= 64 * 1_024));
    assert!(
        !metadata_requests
            .iter()
            .any(|(offset, length)| *offset == stream_start && u64::try_from(*length).unwrap() == stream_length)
    );
    let lookahead: u64 = metadata_requests
        .iter()
        .filter_map(|(offset, length)| {
            let end = offset.checked_add(u64::try_from(*length).ok()?)?;
            (*offset < stream_start && end > stream_start).then_some(end - stream_start)
        })
        .sum();
    assert!(lookahead <= 64 * 1_024);

    source.requests.lock().unwrap().clear();
    let mut encoded = descriptor.open_plain_encoded().unwrap();
    let mut output = vec![0xff; 128 * 1_024];
    assert_eq!(encoded.read_chunk(&mut output).unwrap(), 64 * 1_024);
    assert!(output[..64 * 1_024].iter().all(|byte| *byte == 0));
    assert!(
        source
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, length)| *length <= 64 * 1_024)
    );
}

#[test]
fn multi_megabyte_dictionary_caps_payload_lookahead_then_reads_stream_once() {
    let len = 100_u64 * 1_024 * 1_024;
    let object_offset = 1_024_u64 * 1_024;
    let stream_length = 8_u64 * 1_024 * 1_024;
    let mut object_prefix = b"1 0 obj\n<< /Pad (".to_vec();
    object_prefix.resize(object_prefix.len() + 2 * 1_024 * 1_024 + 1, b'x');
    object_prefix.extend_from_slice(format!(") /Length {stream_length} >>\nstream\n").as_bytes());
    let framed_body = object_prefix[b"1 0 obj\n".len()..].to_vec();
    let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
    let stream_end = stream_start + stream_length;
    let xref = len - 512;
    let xref_bytes = format!(
        "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (object_offset, object_prefix),
            (stream_end, b"\nendstream\nendobj\n".to_vec()),
            (xref, xref_bytes),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

    let stream = reader.resolve_object((1, 0)).unwrap();
    assert_eq!(
        u64::try_from(stream.as_stream().unwrap().content.len()).unwrap(),
        stream_length
    );

    let requests = source.requests.lock().unwrap();
    let speculative_payload_bytes: u64 = requests
        .iter()
        .filter_map(|(offset, length)| {
            let end = offset.checked_add(u64::try_from(*length).ok()?)?;
            (*offset < stream_start && end > stream_start).then(|| end - stream_start)
        })
        .sum();
    assert!(speculative_payload_bytes <= OBJECT_GROWTH_CHUNK);
    assert_eq!(
        requests
            .iter()
            .filter(|(offset, length)| { *offset == stream_start && u64::try_from(*length).unwrap() == stream_length })
            .count(),
        1
    );
    OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));

    let mut framer = DirectObjectFramer::for_dictionary(&framed_body[..2]).unwrap();
    for end in (2..framed_body.len()).step_by(usize::try_from(OBJECT_GROWTH_CHUNK).unwrap()) {
        let _ = framer.advance(&framed_body[..end]);
    }
    assert_eq!(framer.advance(&framed_body), FrameStatus::Ready);
    assert!(framer.scanned_work <= framed_body.len());
}

#[test]
fn bounded_object_stream_decodes_every_admitted_filter_and_predictor_form() {
    const ROW: usize = 8;
    let body = b"<< /Type /Catalog /Pages 11 0 R >>";
    let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
    let rows = pad_to_rows(&plain, ROW);
    let forms: Vec<(&str, String, Vec<u8>)> = vec![
        ("plain", String::new(), plain.clone()),
        ("flate", "/Filter /FlateDecode".into(), flate_encode(&plain)),
        (
            "flate-in-a-one-element-array",
            "/Filter [/FlateDecode]".into(),
            flate_encode(&plain),
        ),
        (
            "flate-png-predictor",
            format!(
                "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns {ROW} /Colors 1 /BitsPerComponent 8 >>"
            ),
            flate_encode(&png_up_predict(&rows, ROW)),
        ),
        (
            "flate-tiff-predictor",
            format!(
                "/Filter /FlateDecode /DecodeParms << /Predictor 2 /Columns {ROW} /Colors 1 /BitsPerComponent 8 >>"
            ),
            flate_encode(&tiff_predict2(&rows, ROW)),
        ),
        (
            "ascii85-then-flate",
            "/Filter [/ASCII85Decode /FlateDecode]".into(),
            ascii85_encode(&flate_encode(&plain)),
        ),
        (
            "asciihex-then-flate",
            "/Filter [/ASCIIHexDecode /FlateDecode]".into(),
            ascii_hex_encode(&flate_encode(&plain)),
        ),
        ("lzw", "/Filter /LZWDecode".into(), lzw_encode(&plain)),
        (
            "ascii85-then-lzw",
            "/Filter [/ASCII85Decode /LZWDecode]".into(),
            ascii85_encode(&lzw_encode(&plain)),
        ),
        (
            "neutral-array-decode-parms",
            "/Filter [/ASCIIHexDecode /FlateDecode] /DecodeParms [null << /Predictor 1 >>]".into(),
            ascii_hex_encode(&flate_encode(&plain)),
        ),
        // ISO 32000-1, 7.4.1: a filter chain carries its parameters as an
        // array parallel to `/Filter`, `null` for the layers that take none.
        // The predictor here is live, so an implementation that dropped the
        // array would hand back predicted (wrong) bytes.
        (
            "live-array-decode-parms",
            format!(
                "/Filter [/ASCIIHexDecode /FlateDecode] /DecodeParms [null << /Predictor 12 /Columns {ROW} /Colors 1 /BitsPerComponent 8 >>]"
            ),
            ascii_hex_encode(&flate_encode(&png_up_predict(&rows, ROW))),
        ),
        (
            "single-filter-array-decode-parms",
            format!(
                "/Filter [/FlateDecode] /DecodeParms [<< /Predictor 12 /Columns {ROW} /Colors 1 /BitsPerComponent 8 >>]"
            ),
            flate_encode(&png_up_predict(&rows, ROW)),
        ),
        (
            "lzw-early-change-array-decode-parms",
            "/Filter [/LZWDecode] /DecodeParms [<< /EarlyChange 0 >>]".into(),
            lzw_encode_late_change(&plain),
        ),
        (
            "lzw-early-change-dictionary-decode-parms",
            "/Filter /LZWDecode /DecodeParms << /EarlyChange 0 >>".into(),
            lzw_encode_late_change(&plain),
        ),
    ];

    let expected = crate::parser::direct_object(body.as_slice()).unwrap();
    for (name, filter, content) in forms {
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} {filter}"),
            &content,
            &[(10, 0)],
        );
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf.clone())).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let resolved = reader
            .resolve_scalar_with_permit((10, 0), &permit)
            .unwrap_or_else(|error| panic!("{name} must decode within the envelope: {error:?}"));
        assert_eq!(resolved.as_object(), &expected, "{name}");
        assert!(permit.stats().peak_bytes <= permit.limit_bytes(), "{name}");
        drop(resolved);
        assert_eq!(permit.stats().current_bytes, 0, "{name}");
        permit.close().unwrap();

        // Whatever the encoding, the lazy answer is the eager answer.
        let eager = crate::Document::load_mem(&fixture.pdf).unwrap();
        assert_eq!(eager.get_object((10, 0)).unwrap(), &expected, "{name}");
    }
}

#[test]
fn bounded_compressed_scalar_refuses_unsupported_encodings_before_encoded_allocation() {
    let body = b"<< /Type /Catalog >>";
    let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
    let encoded = flate_encode(&plain);
    let refused = [
        // Predictor operands outside the range the bounded decode is defined
        // and budgeted for.
        "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Colors 64 >>",
        "/Filter /FlateDecode /DecodeParms << /Predictor 12 /BitsPerComponent 12 >>",
        "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 1000000 >>",
        "/Filter /FlateDecode /DecodeParms << /Predictor 3 >>",
        // An operand the decoder would silently replace with its default.
        "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 8 0 R >>",
        // The array form is decoded now, so the same operand limits apply to
        // whatever entry the terminal layer is handed, and an entry that is
        // neither a dictionary nor `null` still reaches it as its defaults.
        "/Filter [/ASCIIHexDecode /FlateDecode] /DecodeParms [null << /Predictor 3 >>]",
        "/Filter [/FlateDecode] /DecodeParms [<< /Predictor 12 /Columns 1000000 >>]",
        "/Filter [/ASCIIHexDecode /FlateDecode] /DecodeParms [null 9 0 R]",
        // Terminal filters and chain shapes outside the envelope.
        "/Filter /RunLengthDecode",
        "/Filter /Crypt",
        "/Filter [/FlateDecode /ASCII85Decode]",
        "/Filter [/FlateDecode /FlateDecode]",
        "/Filter [/ASCIIHexDecode /ASCIIHexDecode /ASCIIHexDecode /ASCII85Decode /FlateDecode]",
        "/Filter 7 0 R",
    ];
    for filter in refused {
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} {filter}"),
            &encoded,
            &[(10, 0)],
        );
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(
            matches!(
                reader.resolve_scalar_with_permit((10, 0), &permit),
                Err(IndexedReaderError::UnsupportedBoundedScalar { .. })
            ),
            "{filter} must fail closed with a typed refusal"
        );
        assert!(permit.stats().peak_bytes < permit.limit_bytes(), "{filter}");
        assert_eq!(permit.stats().current_bytes, 0, "{filter}");
        permit.close().unwrap();
    }
}

#[test]
fn bounded_object_stream_member_can_use_an_independent_permit() {
    let (first, content) = object_stream_content(&[(10, b"<< /Answer 42 >>")]);
    let fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
    let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
    let IndexedObjectLocation::Compressed { container, index } = reader.object_location((10, 0)).unwrap() else {
        panic!("member 10 was not declared compressed")
    };
    let container_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let member_permit = crate::ScalarResolutionPermit::new(1024 * 1024);
    let prepared = reader
        .prepare_object_stream_with_permit(container, &container_permit)
        .unwrap();
    assert_eq!(prepared.charges.len(), BOUNDED_OBJECT_STREAM_CHARGE_HANDLES);
    assert_eq!(
        prepared.excluded_structural_bytes(),
        BOUNDED_OBJECT_STREAM_EXCLUDED_STRUCTURAL_BYTES as u64
    );
    assert!(prepared.excluded_structural_bytes() <= BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES);
    assert_eq!(container_permit.stats().current_bytes, prepared.retained_bytes());
    assert_eq!(member_permit.stats().current_bytes, 0);

    let refusing_member_permit = crate::ScalarResolutionPermit::new(1);
    assert!(matches!(
        prepared.resolve_member_with_permit((10, 0), index, &refusing_member_permit),
        Err(IndexedReaderError::ScalarResourceLimit { .. })
    ));
    assert_eq!(refusing_member_permit.stats().current_bytes, 0);
    refusing_member_permit.close().unwrap();
    assert_eq!(container_permit.stats().current_bytes, prepared.retained_bytes());

    let member = prepared
        .resolve_member_with_permit((10, 0), index, &member_permit)
        .unwrap();
    assert_eq!(
        member
            .as_object()
            .as_dict()
            .unwrap()
            .get(b"Answer")
            .unwrap()
            .as_i64()
            .unwrap(),
        42
    );
    assert_eq!(container_permit.stats().current_bytes, prepared.retained_bytes());
    assert_eq!(member_permit.stats().current_bytes, member.retained_bytes());

    drop(member);
    assert_eq!(member_permit.stats().current_bytes, 0);
    member_permit.close().unwrap();
    assert_eq!(container_permit.stats().current_bytes, prepared.retained_bytes());
    drop(prepared);
    assert_eq!(container_permit.stats().current_bytes, 0);
    container_permit.close().unwrap();
}
