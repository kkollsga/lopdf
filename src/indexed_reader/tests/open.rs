//! Indexed-reader tests: open.

use super::*;

#[test]
fn classic_bootstrap_matches_fixture_fingerprint_and_eager() {
    let pdf = classic_pdf();
    let index = open_bytes(&pdf);

    assert_eq!(index.xref_type, IndexXrefType::Table);
    assert_eq!(index.source_origin, 0);
    assert_fixture_fingerprint_and_eager_agreement(&pdf, &index, &[1, 2, 3, 4]);
}

#[test]
fn encrypted_revisions_accept_user_and_owner_passwords_and_reject_missing_or_wrong() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");

        assert!(matches!(
            open_encrypted(&pdf, None),
            Err(IndexedReaderError::PasswordRequired)
        ));
        let wrong_a = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
        let wrong_b = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
        assert!(matches!(wrong_a, IndexedReaderError::InvalidPassword));
        assert_eq!(format!("{wrong_a:?}"), format!("{wrong_b:?}"));

        let user = open_encrypted(&pdf, Some(b"user")).unwrap();
        assert_encrypted_fixture_plaintext(&user);
        assert_eq!(
            user.resolve_object((1, 0)).unwrap(),
            Object::string_literal("encrypted string")
        );
        assert_eq!(
            user.resolve_object((2, 0)).unwrap(),
            Object::Stream(Stream::new(
                dictionary! { "Type" => "Metadata" },
                b"encrypted stream".to_vec()
            ))
        );
        assert_eq!(
            user.resolve_object((3, 0)).unwrap(),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Sentinel" => (1, 0) })
        );

        let encrypt_id = user.index.encrypt_object_id.unwrap();
        assert_eq!(
            user.resolve_object(encrypt_id)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Filter")
                .unwrap()
                .as_name()
                .unwrap(),
            b"Standard"
        );

        let owner = open_encrypted(&pdf, Some(b"owner")).unwrap();
        assert_encrypted_fixture_plaintext(&owner);
    }
}

#[test]
fn empty_user_password_and_inline_encrypt_dictionary_open_without_materializing_document() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "");
        let reader = open_encrypted(&pdf, None).unwrap();
        assert_encrypted_fixture_plaintext(&reader);
    }

    let pdf = inline_encrypt_dictionary(encrypted_pdf(3, "owner", ""));
    let reader = open_encrypted(&pdf, None).unwrap();
    assert!(reader.index.encryption_state.is_some());
    assert_eq!(reader.index.encrypt_object_id, None);
    assert_encrypted_fixture_plaintext(&reader);
}

#[test]
fn encrypted_object_stream_container_is_decrypted_once_and_xref_stays_plain() {
    let (pdf, image_plaintext, xref_plaintext) = encrypted_object_stream_pdf(4, false, 0);
    let reader = open_encrypted(&pdf, Some(b"user")).unwrap();

    let member = reader.resolve_object((10, 0)).unwrap();
    let member = member.as_dict().unwrap();
    assert_eq!(
        member.get(b"Text").unwrap(),
        &Object::String(b"member secret".to_vec(), StringFormat::Literal)
    );
    assert_eq!(member.get(b"Image").unwrap(), &Object::Reference((20, 0)));

    for _ in 0..2 {
        assert_eq!(
            reader.resolve_object((20, 0)).unwrap().as_stream().unwrap().content,
            image_plaintext
        );
    }
    assert_eq!(
        reader.resolve_object((21, 0)).unwrap(),
        Object::String(b"normal secret".to_vec(), StringFormat::Literal)
    );
    assert_eq!(
        reader.resolve_object((31, 0)).unwrap().as_stream().unwrap().content,
        xref_plaintext
    );
    assert_eq!(
        reader
            .resolve_object((30, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Filter")
            .unwrap()
            .as_name()
            .unwrap(),
        b"Standard"
    );
}

#[test]
fn plain_and_compressed_xref_streams_match_fixture_and_eager() {
    for compressed in [false, true] {
        let pdf = xref_stream_pdf(compressed);
        let index = open_bytes(&pdf);
        assert_eq!(index.xref_type, IndexXrefType::Stream);
        assert_fixture_fingerprint_and_eager_agreement(&pdf, &index, &[1, 2, 3, 4, 5]);
    }
}

#[test]
fn incremental_hybrid_merge_keeps_newest_and_supplement() {
    let (mut pdf, offsets) = basic_body();
    let base_entries = offsets
        .into_iter()
        .enumerate()
        .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
        .collect();
    let base_xref = append_classic(&mut pdf, &[(0, base_entries)], "<< /Size 5 /Root 1 0 R /Info 4 0 R >>");

    let new_info = push_object(&mut pdf, 4, b"<< /Title (newest) >>");
    let marker = push_object(&mut pdf, 6, b"<< /Marker (hybrid) >>");
    let supplement = u64::try_from(pdf.len()).unwrap();
    let mut encoded = Vec::new();
    encode_field(1, 1, &mut encoded);
    encode_field(marker, 8, &mut encoded);
    encode_field(0, 2, &mut encoded);
    pdf.extend_from_slice(
        format!(
            "7 0 obj\n<< /Type /XRef /Size 8 /Index [6 1] /W [1 8 2] /Length {} >>\nstream\n",
            encoded.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    append_classic(
        &mut pdf,
        &[(4, vec![(new_info, 0, true)]), (7, vec![(supplement, 0, true)])],
        &format!("<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} /XRefStm {supplement} >>"),
    );
    let index = open_bytes(&pdf);

    assert_eq!(
        index.locations.get(&4),
        Some(&ObjectLocation64::Normal {
            offset: new_info,
            generation: 0
        })
    );
    assert_eq!(
        index.locations.get(&6),
        Some(&ObjectLocation64::Normal {
            offset: marker,
            generation: 0
        })
    );
    assert_fixture_fingerprint_and_eager_agreement(&pdf, &index, &[1, 2, 3, 4, 6, 7]);
}

#[test]
fn hybrid_supplement_of_an_inner_section_supersedes_older_object_streams() {
    let pdf = hybrid_superseded_across_object_streams();
    let newest = Object::Dictionary(dictionary! { "Rev" => Object::string_literal("update") });

    let index = open_bytes(&pdf);
    assert_eq!(
        index.locations.get(&6),
        Some(&ObjectLocation64::Compressed { container: 8, index: 0 })
    );

    let eager = Document::load_mem(&pdf).unwrap();
    assert!(
        matches!(
            eager.reference_table.get(6),
            Some(XrefEntry::Compressed { container: 8, index: 0 })
        ),
        "eager kept {:?} for the superseded object",
        eager.reference_table.get(6)
    );
    assert_eq!(eager.get_object((6, 0)).unwrap(), &newest);

    let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    assert_eq!(reader.resolve_object((6, 0)).unwrap(), newest);

    // The newest section still wins for everything it does declare.
    assert_eq!(eager.trailer.get(b"Info").unwrap().as_reference().unwrap(), (10, 0));
    assert_fixture_fingerprint_and_eager_agreement(&pdf, &index, &[1, 2, 3, 4, 5, 8, 9, 10]);
}

#[test]
fn leading_junk_is_rebased_like_the_eager_reader() {
    let pdf = classic_pdf();
    let prefix = b"ignored transport prefix\n";
    let mut prefixed = prefix.to_vec();
    prefixed.extend_from_slice(&pdf);
    let index = open_bytes(&prefixed);

    assert_eq!(index.source_origin, u64::try_from(prefix.len()).unwrap());
    assert_fixture_fingerprint_and_eager_agreement(&prefixed, &index, &[1, 2, 3, 4]);
}

#[test]
fn header_starting_at_last_scannable_byte_uses_bounded_overlap() {
    let pdf = classic_pdf();
    let mut boundary = vec![b'x'; usize::try_from(HEADER_SCAN_LIMIT - 1).unwrap()];
    boundary.extend_from_slice(&pdf);
    let index = open_bytes(&boundary);
    assert_eq!(index.source_origin, HEADER_SCAN_LIMIT - 1);

    let mut outside = vec![b'x'; usize::try_from(HEADER_SCAN_LIMIT).unwrap()];
    outside.extend_from_slice(&pdf);
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(outside))),
        Err(IndexedReaderError::InvalidHeader { .. })
    ));
}

#[test]
fn corrupt_startxref_recovers_by_rescan_while_other_failures_still_fail() {
    let healthy = PdfIndex::open(Arc::new(BytesSource::from(classic_pdf()))).unwrap();
    assert!(!healthy.recovered, "a healthy file must never trigger recovery");

    // `startxref` unreadable — `InvalidStartXref`.
    let mut missing = classic_pdf();
    let marker = rfind(&missing, b"startxref").unwrap();
    missing[marker..marker + b"startxref".len()].fill(b'x');

    // `startxref` points past the end — `StartXrefOutOfBounds`.
    let mut out_of_bounds = classic_pdf();
    let marker = rfind(&out_of_bounds, b"startxref\n").unwrap() + b"startxref\n".len();
    let end = out_of_bounds[marker..].iter().position(|byte| *byte == b'\n').unwrap() + marker;
    out_of_bounds.splice(marker..end, b"999999999".iter().copied());

    // The section `startxref` names is not a cross-reference table — `InvalidXref`.
    let mut wrong_section = classic_pdf();
    let marker = rfind(&wrong_section, b"startxref\n").unwrap() + b"startxref\n".len();
    let end = wrong_section[marker..].iter().position(|byte| *byte == b'\n').unwrap() + marker;
    wrong_section.splice(marker..end, b"9".iter().copied());

    for (name, damaged) in [
        ("unreadable-startxref", missing),
        ("out-of-bounds-startxref", out_of_bounds),
        ("startxref-into-the-body", wrong_section),
    ] {
        let recovered = PdfIndex::open(Arc::new(BytesSource::from(damaged))).unwrap();
        assert!(recovered.recovered, "{name}");
        // The rescan finds every live object where the intact table put it,
        // and reads the same trailer the table's own revision carried.
        assert_eq!(
            live_normal_locations(&recovered),
            live_normal_locations(&healthy),
            "{name}"
        );
        assert_eq!(recovered.trailer, healthy.trailer, "{name}");
        assert_eq!(recovered.declared_size, healthy.declared_size, "{name}");
        assert_eq!(recovered.version, healthy.version, "{name}");
    }

    // A trailer offset that is merely nonsense is not an unusable
    // cross-reference machine, so it keeps failing rather than rescanning.
    let (mut bad_prev, offsets) = basic_body();
    let entries = offsets
        .into_iter()
        .enumerate()
        .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
        .collect();
    append_classic(&mut bad_prev, &[(0, entries)], "<< /Size 5 /Root 1 0 R /Prev -1 >>");
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(bad_prev))),
        Err(IndexedReaderError::InvalidTrailerOffset { key: "Prev" })
    ));
}

#[test]
fn rescan_that_cannot_prove_a_catalog_reports_the_original_failure() {
    // No body at all: nothing to recover from, so the caller sees exactly
    // the cross-reference failure it saw before recovery existed.
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(malformed_startxref_pdf("1")))),
        Err(IndexedReaderError::InvalidXref { .. })
    ));
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(malformed_startxref_pdf("-1")))),
        Err(IndexedReaderError::InvalidStartXref { .. })
    ));

    // A body whose catalog is gone. The rescan finds objects and a trailer,
    // but the `/Root` it names is not among them, so opening would produce a
    // document that resolves to nothing.
    let mut orphaned = classic_pdf();
    let catalog = orphaned
        .windows(b"1 0 obj".len())
        .position(|window| window == b"1 0 obj")
        .unwrap();
    orphaned[catalog..catalog + b"1 0 obj".len()].fill(b'x');
    let marker = rfind(&orphaned, b"startxref").unwrap();
    orphaned[marker..marker + b"startxref".len()].fill(b'x');
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(orphaned))),
        Err(IndexedReaderError::InvalidStartXref { .. })
    ));

    // An encrypted body cannot be opened from a synthesized trailer, which
    // would drop `/Encrypt` and decode every string as ciphertext.
    let mut encrypted = classic_pdf();
    let trailer = rfind(&encrypted, b"trailer").unwrap();
    encrypted.splice(trailer..trailer + b"trailer".len(), b"xxxxxxx".iter().copied());
    let marker = rfind(&encrypted, b"startxref").unwrap();
    encrypted[marker..marker + b"startxref".len()].fill(b'x');
    let without_encrypt = PdfIndex::open(Arc::new(BytesSource::from(encrypted.clone()))).unwrap();
    assert!(without_encrypt.recovered);
    assert_eq!(
        without_encrypt.trailer.get(b"Root").unwrap().as_reference().unwrap(),
        (1, 0)
    );
    let catalog = encrypted
        .windows(b"/Type /Catalog".len())
        .position(|window| window == b"/Type /Catalog")
        .unwrap();
    encrypted.splice(catalog..catalog, b"/Encrypt 9 0 R ".iter().copied());
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(encrypted))),
        Err(IndexedReaderError::InvalidStartXref { .. })
    ));
}

#[test]
fn rescan_is_one_forward_pass_of_bounded_chunks_over_a_multi_megabyte_body() {
    let healthy_pdf = generated_page_tree_pdf(20_000, 20_000);
    assert!(healthy_pdf.len() > 1024 * 1024, "{}", healthy_pdf.len());
    let healthy = PdfIndex::open(Arc::new(BytesSource::from(healthy_pdf.clone()))).unwrap();

    let mut damaged = healthy_pdf.clone();
    let marker = rfind(&damaged, b"startxref").unwrap();
    damaged[marker..marker + b"startxref".len()].fill(b'x');
    let source = Arc::new(TracingBytesSource {
        bytes: damaged,
        requests: Mutex::new(Vec::new()),
    });
    let recovered = PdfIndex::open(source.clone()).unwrap();
    assert!(recovered.recovered);
    assert_eq!(live_normal_locations(&recovered), live_normal_locations(&healthy));

    let requests = source.requests.lock().unwrap();
    let chunk = usize::try_from(RECOVERY_CHUNK_BYTES).unwrap();
    // Never a whole-file read, and never more than a chunk at a time — the
    // scan's own retention is the offsets map, not the body.
    assert!(
        requests.iter().all(|(_, length)| *length <= chunk),
        "unbounded read: {requests:?}"
    );
    let scan_reads = requests.iter().filter(|(_, length)| *length == chunk).count();
    let expected = source.bytes.len() / chunk;
    assert!(
        scan_reads == expected || scan_reads == expected + 1,
        "{scan_reads} full chunks for a {}-byte body",
        source.bytes.len()
    );
    // One pass: no chunk offset is ever revisited.
    let mut offsets: Vec<_> = requests
        .iter()
        .filter(|(_, length)| *length == chunk)
        .map(|(offset, _)| *offset)
        .collect();
    let issued = offsets.len();
    offsets.sort_unstable();
    offsets.dedup();
    assert_eq!(offsets.len(), issued, "the scan must not re-read a chunk");
}

#[test]
fn initial_startxref_out_of_bounds_matches_the_fixture_and_retains_u64_fields() {
    let cases = [
        ("18446744073709551615", u64::MAX),
        ("999999999", 999_999_999),
        ("9223372036854775807", i64::MAX as u64),
        ("9223372036854775808", (i64::MAX as u64) + 1),
    ];

    for (text, expected_offset) in cases {
        let pdf = malformed_startxref_pdf(text);
        let logical_len = u64::try_from(pdf.len()).unwrap();
        let lazy = PdfIndex::open(Arc::new(BytesSource::from(pdf))).err().unwrap();

        assert_eq!(lazy.to_string(), "failed parsing cross reference table");
        assert!(matches!(
            lazy,
            IndexedReaderError::StartXrefOutOfBounds { offset, logical_len: actual_len }
                if offset == expected_offset && actual_len == logical_len
        ));
    }

    assert_eq!(malformed_startxref_pdf("18446744073709551615").len(), 46);
}

#[test]
fn invalid_textual_startxref_matches_the_fixture_without_losing_parse_classification() {
    for text in ["-1", "18446744073709551616"] {
        let pdf = malformed_startxref_pdf(text);
        let lazy = PdfIndex::open(Arc::new(BytesSource::from(pdf))).err().unwrap();

        assert_eq!(lazy.to_string(), "failed parsing cross reference table");
        assert!(matches!(lazy, IndexedReaderError::InvalidStartXref { .. }));
    }
}

#[test]
fn prefixed_max_startxref_is_checked_in_logical_coordinates() {
    let prefix = b"ignored transport prefix\n";
    let mut pdf = prefix.to_vec();
    pdf.extend_from_slice(&malformed_startxref_pdf("18446744073709551615"));
    let logical_len = u64::try_from(pdf.len() - prefix.len()).unwrap();

    let lazy = PdfIndex::open(Arc::new(BytesSource::from(pdf))).err().unwrap();
    assert_eq!(lazy.to_string(), "failed parsing cross reference table");
    assert!(matches!(
        lazy,
        IndexedReaderError::StartXrefOutOfBounds { offset: u64::MAX, logical_len: actual_len }
            if actual_len == logical_len
    ));
}

#[test]
fn initial_startxref_boundaries_do_not_change_equal_or_in_range_behavior() {
    const SOURCE_LEN: usize = 128;
    for (offset, is_out_of_bounds) in [(127_u64, false), (128, false), (129, true)] {
        let mut pdf = malformed_startxref_pdf(&offset.to_string());
        pdf.resize(SOURCE_LEN, b' ');
        let error = PdfIndex::open(Arc::new(BytesSource::from(pdf))).err().unwrap();
        if is_out_of_bounds {
            assert!(matches!(
                error,
                IndexedReaderError::StartXrefOutOfBounds {
                    offset: 129,
                    logical_len: 128
                }
            ));
        } else {
            assert!(!matches!(error, IndexedReaderError::StartXrefOutOfBounds { .. }));
        }
    }
}

#[test]
fn out_of_bounds_startxref_stops_after_the_bounded_header_and_tail_reads() {
    let bytes = malformed_startxref_pdf("18446744073709551615");
    let source = Arc::new(TracingBytesSource {
        bytes,
        requests: Mutex::new(Vec::new()),
    });
    let error = PdfIndex::open(source.clone()).err().unwrap();
    assert!(matches!(
        error,
        IndexedReaderError::StartXrefOutOfBounds {
            offset: u64::MAX,
            logical_len: 46
        }
    ));

    let requests = source.requests.lock().unwrap();
    // Header probe, tail probe, then the recovery rescan's single chunk over
    // this 46-byte body — which finds no object header, so the original
    // failure is what the caller sees.
    assert_eq!(requests.as_slice(), &[(0, 46), (0, 46), (0, 46)]);
    assert!(
        requests
            .iter()
            .all(|(_, length)| *length <= usize::try_from(TAIL_SCAN_LIMIT).unwrap())
    );
    assert!(!requests.iter().any(|(offset, _)| *offset == u64::MAX));
}

#[test]
fn sparse_valid_startxref_above_i64_max_remains_supported() {
    let xref = (i64::MAX as u64) + 1;
    let tail =
        format!("xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\nstartxref\n{xref}\n%%EOF\n").into_bytes();
    let len = xref.checked_add(u64::try_from(tail.len()).unwrap()).unwrap();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![(0, b"%PDF-1.7\n".to_vec()), (xref, tail)],
        requests: Mutex::new(Vec::new()),
    });

    let index = PdfIndex::open(source.clone()).unwrap();
    assert_eq!(index.xref_start, xref);
    assert!(
        source
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(offset, _)| *offset == xref)
    );
}

#[test]
fn repeated_prev_is_cycle_checked_and_terminates() {
    let (mut pdf, offsets) = basic_body();
    let xref = u64::try_from(pdf.len()).unwrap();
    let entries = offsets
        .into_iter()
        .enumerate()
        .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
        .collect();
    append_classic(
        &mut pdf,
        &[(0, entries)],
        &format!("<< /Size 5 /Root 1 0 R /Prev {xref} >>"),
    );
    let index = open_bytes(&pdf);
    assert_eq!(index.xref_start, xref);
}

#[test]
fn raw_xref_window_accepts_boundary_and_rejects_one_byte_over() {
    assert!(PdfIndex::open(Arc::new(BytesSource::from(padded_classic_xref(0)))).is_ok());
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(padded_classic_xref(1)))),
        Err(IndexedReaderError::StructureLimitExceeded {
            structure: "cross-reference section",
            limit: XREF_WINDOW_LIMIT
        })
    ));
}

#[test]
fn decoded_xref_limit_accepts_boundary_and_rejects_one_byte_over() {
    assert!(
        PdfIndex::open(Arc::new(BytesSource::from(compressed_limit_xref(
            XREF_DECOMPRESSED_LIMIT
        ))))
        .is_ok()
    );
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(compressed_limit_xref(
            XREF_DECOMPRESSED_LIMIT + 1
        )))),
        Err(IndexedReaderError::XrefDecompression(_))
    ));
}

#[test]
fn xref_field_width_and_entry_count_limits_are_inclusive() {
    assert!(PdfIndex::open(Arc::new(BytesSource::from(empty_width_xref(8)))).is_ok());
    assert!(matches!(
        PdfIndex::open(Arc::new(BytesSource::from(empty_width_xref(9)))),
        Err(IndexedReaderError::InvalidXref { .. })
    ));
    assert!(check_entry_limit(MAX_XREF_ENTRIES).is_ok());
    assert!(matches!(
        check_entry_limit(MAX_XREF_ENTRIES + 1),
        Err(IndexedReaderError::EntryLimitExceeded {
            count,
            limit: MAX_XREF_ENTRIES
        }) if count == MAX_XREF_ENTRIES + 1
    ));
}

#[cfg(any(unix, windows))]
#[test]
fn sparse_file_preserves_normal_offset_beyond_u32() {
    use crate::source::FileSource;
    use std::io::{Seek, SeekFrom};

    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"%PDF-1.7\n").unwrap();
    let xref = u64::from(u32::MAX) + 4_096;
    let object_offset = u64::from(u32::MAX) + 17;
    file.seek(SeekFrom::Start(xref)).unwrap();
    file.write_all(
        format!(
            "xref\n1 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .as_bytes(),
    )
    .unwrap();
    file.flush().unwrap();

    let index = PdfIndex::open(Arc::new(FileSource::from_file(file).unwrap())).unwrap();
    assert_eq!(
        index.locations.get(&1),
        Some(&ObjectLocation64::Normal {
            offset: object_offset,
            generation: 0
        })
    );
}

#[test]
fn traced_hundred_megabyte_source_never_requests_the_whole_source() {
    let len = 100_u64 * 1_024 * 1_024;
    let xref = len - 256;
    let tail =
        format!("xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
            .into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![(0, b"%PDF-1.7\n".to_vec()), (xref, tail)],
        requests: Mutex::new(Vec::new()),
    });
    let index = PdfIndex::open(source.clone()).unwrap();
    assert_eq!(index.xref_start, xref);

    let requests = source.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|(_, length)| u64::try_from(*length).unwrap() <= TAIL_SCAN_LIMIT)
    );
    let total: usize = requests.iter().map(|(_, length)| *length).sum();
    assert!(u64::try_from(total).unwrap() < len / 100);
}

#[test]
fn encrypted_open_and_resolution_are_bounded_on_a_sparse_hundred_megabyte_source() {
    let pdf = encrypted_pdf(4, "owner", "user");
    let pdf_source = BytesSource::from(pdf.clone());
    let pdf_len = u64::try_from(pdf.len()).unwrap();
    let xref = read_startxref(&pdf_source, pdf_len).unwrap();
    let len = 100_u64 * 1_024 * 1_024;
    let tail = format!("startxref\n{xref}\n%%EOF\n").into_bytes();
    let tail_offset = len - u64::try_from(tail.len()).unwrap();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![(0, pdf), (tail_offset, tail)],
        requests: Mutex::new(Vec::new()),
    });

    let reader = IndexedReader::open_with_password(source.clone(), ResolverLimits::default(), Some(b"user")).unwrap();
    assert_encrypted_fixture_plaintext(&reader);

    let requests = source.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|(_, length)| u64::try_from(*length).unwrap() <= TAIL_SCAN_LIMIT)
    );
    assert!(
        !requests
            .iter()
            .any(|(offset, length)| { *offset == 0 && u64::try_from(*length).unwrap_or(u64::MAX) == len })
    );
    let total: usize = requests.iter().map(|(_, length)| *length).sum();
    assert!(u64::try_from(total).unwrap() < 1_024 * 1_024);
}

#[test]
fn tiny_xref_at_one_megabyte_uses_only_the_initial_window() {
    let len = 100_u64 * 1_024 * 1_024;
    let xref = 1_024_u64 * 1_024;
    let xref_bytes = b"xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\n".to_vec();
    let eof_offset = len - 128;
    let tail = format!("startxref\n{xref}\n%%EOF\n").into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![(0, b"%PDF-1.7\n".to_vec()), (xref, xref_bytes), (eof_offset, tail)],
        requests: Mutex::new(Vec::new()),
    });

    PdfIndex::open(source.clone()).unwrap();
    let requests = source.requests.lock().unwrap();
    assert!(requests.contains(&(xref, usize::try_from(XREF_INITIAL_WINDOW).unwrap())));
    assert!(
        !requests
            .iter()
            .any(|(offset, length)| { *offset == xref && u64::try_from(*length).unwrap() > XREF_INITIAL_WINDOW })
    );
}

#[test]
fn incremental_revisions_each_stop_at_the_initial_window() {
    let len = 100_u64 * 1_024 * 1_024;
    let base = 1_024_u64 * 1_024;
    let newest = 2_u64 * 1_024 * 1_024;
    let base_bytes = b"xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\n".to_vec();
    let newest_bytes =
        format!("xref\n1 1\n0000000042 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {base} >>\n").into_bytes();
    let eof_offset = len - 128;
    let tail = format!("startxref\n{newest}\n%%EOF\n").into_bytes();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (base, base_bytes),
            (newest, newest_bytes),
            (eof_offset, tail),
        ],
        requests: Mutex::new(Vec::new()),
    });

    let index = PdfIndex::open(source.clone()).unwrap();
    assert_eq!(
        index.locations.get(&1),
        Some(&ObjectLocation64::Normal {
            offset: 42,
            generation: 0
        })
    );
    let requests = source.requests.lock().unwrap();
    for offset in [base, newest] {
        let lengths: Vec<_> = requests
            .iter()
            .filter_map(|(actual, length)| (*actual == offset).then_some(*length))
            .collect();
        assert_eq!(lengths, vec![usize::try_from(XREF_INITIAL_WINDOW).unwrap()]);
    }
}

#[test]
fn revision_limit_accepts_1024_and_rejects_1025() {
    let boundary = revision_source(MAX_XREF_REVISIONS);
    assert!(PdfIndex::open(boundary).is_ok());

    let over = revision_source(MAX_XREF_REVISIONS + 1);
    assert!(matches!(
        PdfIndex::open(over),
        Err(IndexedReaderError::RevisionLimitExceeded {
            limit: MAX_XREF_REVISIONS
        })
    ));
}

#[test]
fn two_node_prev_cycle_is_detected_without_extra_reads() {
    let len = 4_u64 * 1_024 * 1_024;
    let first = 1_024_u64 * 1_024;
    let second = 2_u64 * 1_024 * 1_024;
    let first_bytes =
        format!("xref\n1 1\n0000000011 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {second} >>\n").into_bytes();
    let second_bytes =
        format!("xref\n1 1\n0000000022 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {first} >>\n").into_bytes();
    let eof_offset = len - 128;
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (first, first_bytes),
            (second, second_bytes),
            (eof_offset, format!("startxref\n{second}\n%%EOF\n").into_bytes()),
        ],
        requests: Mutex::new(Vec::new()),
    });

    let index = PdfIndex::open(source.clone()).unwrap();
    assert_eq!(
        index.locations.get(&1),
        Some(&ObjectLocation64::Normal {
            offset: 22,
            generation: 0
        })
    );
    let requests = source.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|(offset, _)| *offset == first).count(), 1);
    assert_eq!(requests.iter().filter(|(offset, _)| *offset == second).count(), 1);
}

#[test]
fn semantic_xref_stream_error_stops_after_initial_window() {
    let len = 100_u64 * 1_024 * 1_024;
    let xref = 1_024_u64 * 1_024;
    let xref_bytes =
        b"1 0 obj\n<< /Type /XRef /Size 0 /Index [0 0] /W [1 9 1] /Length 0 >>\nstream\n\nendstream\nendobj\n".to_vec();
    let eof_offset = len - 128;
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![
            (0, b"%PDF-1.7\n".to_vec()),
            (xref, xref_bytes),
            (eof_offset, format!("startxref\n{xref}\n%%EOF\n").into_bytes()),
        ],
        requests: Mutex::new(Vec::new()),
    });

    assert!(matches!(
        open_without_recovery(source.clone()),
        Err(IndexedReaderError::InvalidXref { .. })
    ));
    let requests = source.requests.lock().unwrap();
    let lengths: Vec<_> = requests
        .iter()
        .filter_map(|(offset, length)| (*offset == xref).then_some(*length))
        .collect();
    assert_eq!(lengths, vec![usize::try_from(XREF_INITIAL_WINDOW).unwrap()]);
}

#[test]
fn xref_stream_framing_matches_eager_reader() {
    let valid = xref_stream_pdf(true);
    assert!(Document::load_mem(&valid).is_ok());
    assert!(open_without_recovery(Arc::new(BytesSource::from(valid.clone()))).is_ok());

    let missing_endstream = corrupt_marker(valid.clone(), b"endstream");
    assert!(Document::load_mem(&missing_endstream).is_err());
    assert!(matches!(
        open_without_recovery(Arc::new(BytesSource::from(missing_endstream.clone()))),
        Err(IndexedReaderError::InvalidXref { .. })
    ));
    // The framing refusal above is what the public open then answers with a
    // rescan: this file's body is intact and its `/Type /XRef` dictionary
    // still names the catalog, so the document stays on the indexed route.
    let recovered = PdfIndex::open(Arc::new(BytesSource::from(missing_endstream))).unwrap();
    assert!(recovered.recovered);
    assert_eq!(
        live_normal_locations(&recovered),
        live_normal_locations(&open_bytes(&valid))
    );

    let mut missing_endobj = valid.clone();
    let marker = rfind(&missing_endobj, b"endobj").unwrap();
    missing_endobj.drain(marker..marker + b"endobj".len());
    assert!(Document::load_mem(&missing_endobj).is_ok());
    assert!(open_without_recovery(Arc::new(BytesSource::from(missing_endobj))).is_ok());

    let mut spaced_header = valid.clone();
    let marker = spaced_header
        .windows(b"stream\n".len())
        .position(|window| window == b"stream\n")
        .unwrap();
    spaced_header.splice(
        marker + b"stream".len()..marker + b"stream".len(),
        b" \t".iter().copied(),
    );
    assert!(Document::load_mem(&spaced_header).is_ok());
    assert!(open_without_recovery(Arc::new(BytesSource::from(spaced_header))).is_ok());

    for replacement in [b"".as_slice(), b"\x0c\n", b" % gap\n"] {
        let mut rejected = valid.clone();
        let marker = rejected
            .windows(b"stream\n".len())
            .position(|window| window == b"stream\n")
            .unwrap();
        let eol = marker + b"stream".len();
        rejected.splice(eol..eol + 1, replacement.iter().copied());
        assert!(Document::load_mem(&rejected).is_err());
        assert!(matches!(
            open_without_recovery(Arc::new(BytesSource::from(rejected))),
            Err(IndexedReaderError::InvalidXref { .. })
        ));
    }

    for replacement in [b"".as_slice(), b"\r\n"] {
        let mut accepted = valid.clone();
        let marker = rfind(&accepted, b"endstream").unwrap();
        assert_eq!(accepted[marker - 1], b'\n');
        accepted.splice(marker - 1..marker, replacement.iter().copied());
        assert!(Document::load_mem(&accepted).is_ok());
        assert!(open_without_recovery(Arc::new(BytesSource::from(accepted))).is_ok());
    }

    for replacement in [b" \n".as_slice(), b"\n% gap\n"] {
        let mut rejected = valid.clone();
        let marker = rfind(&rejected, b"endstream").unwrap();
        rejected.splice(marker - 1..marker, replacement.iter().copied());
        assert!(Document::load_mem(&rejected).is_err());
        assert!(matches!(
            open_without_recovery(Arc::new(BytesSource::from(rejected))),
            Err(IndexedReaderError::InvalidXref { .. })
        ));
    }
}
