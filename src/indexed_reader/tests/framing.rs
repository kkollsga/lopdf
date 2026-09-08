//! Indexed-reader tests: framing.

use super::*;

#[test]
fn malformed_stream_syntax_backtracks_to_dictionary_like_eager() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Case /BadHeader >>\nstream % gap\nvalue\nendstream",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Case /BadEnd >>\nstream\nvalue\nnot-endstream",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 5 /Case /MissingEnd >>\nstream\nvalue",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Length 1000000 /Case /PastSource >>\nstream\nshort",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    for id in [(1, 0), (2, 0), (3, 0), (4, 0)] {
        let resolved = reader.resolve_object(id).unwrap();
        assert!(matches!(resolved, Object::Dictionary(_)));
        // This fixture pins the indexed reader's established malformed-object
        // contract. Upstream #568 made eager parsing reject these objects while
        // adding bounded recovery for a different shape: an unambiguous,
        // EOL-framed `endstream` followed by `endobj`. Porting that recovery
        // into the random-access framer is separate work; it must not silently
        // remove the indexed reader's conservative dictionary fallback.
        let expected: &[u8] = match id.0 {
            1 => b"BadHeader",
            2 => b"BadEnd",
            3 => b"MissingEnd",
            4 => b"PastSource",
            _ => unreachable!(),
        };
        assert_eq!(
            resolved.as_dict().unwrap().get(b"Case").unwrap().as_name().unwrap(),
            expected
        );
    }
}

#[test]
fn object_parser_limit_is_explicit() {
    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Payload (abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz) >>",
    }]);
    let reader = open_reader(
        &pdf,
        ResolverLimits {
            max_object_bytes: 32,
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        reader.resolve_object((1, 0)),
        Err(IndexedReaderError::ObjectLimitExceeded {
            id: (1, 0),
            limit: 32,
            provenance: ObjectLimitProvenance::FrameNeedMoreAtMaximum,
        })
    ));
}

#[test]
fn semantic_invalid_object_stops_after_initial_probe() {
    let len = 100_u64 * 1_024 * 1_024;
    let object_offset = 1_024_u64 * 1_024;
    let object_prefix = b"1 0 obj\n<< /Broken @".to_vec();
    let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
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
            (xref, xref_bytes),
        ],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let error = reader.resolve_object((1, 0)).unwrap_err();
    assert!(
        matches!(error, IndexedReaderError::InvalidIndirectObject { id: (1, 0), .. }),
        "unexpected error: {error:?}"
    );
    let requests = source.requests.lock().unwrap();
    let body_lengths: Vec<_> = requests
        .iter()
        .filter_map(|(offset, length)| (*offset == body_offset).then_some(*length))
        .collect();
    assert_eq!(body_lengths, vec![usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()]);
}

#[test]
fn dictionary_framer_matches_parser_at_every_split_point() {
    let valid = [
        b"<< /Value 1 >>\nendobj".as_slice(),
        b"<< /Nested << /Array [1 (two \\) >>) <3e3e>] >> /Name /has@sign >>\nendobj".as_slice(),
        b"<< /Length 5 >> % comment\nstream\nhello".as_slice(),
    ];
    for sample in valid {
        assert!(parse_object_body(sample, (1, 0), 0).is_ok());
        for split in 2..sample.len() {
            let mut framer = DirectObjectFramer::for_dictionary(&sample[..2]).unwrap();
            let first = framer.advance(&sample[..split]);
            if first != FrameStatus::Ready {
                assert_eq!(framer.advance(sample), FrameStatus::Ready, "split {split}: {sample:?}");
            }
            assert!(framer.scanned_work <= sample.len());
        }
    }

    let invalid = b"<< /Broken @";
    assert!(matches!(
        parse_object_body(invalid, (1, 0), 0),
        Err(IndexedReaderError::InvalidIndirectObject { .. })
    ));
    for split in 2..=invalid.len() {
        let mut framer = DirectObjectFramer::for_dictionary(&invalid[..2]).unwrap();
        let _ = framer.advance(&invalid[..split]);
        assert_eq!(framer.advance(invalid), FrameStatus::Invalid);
        assert!(framer.scanned_work <= invalid.len());
    }
}

#[test]
fn direct_object_framer_covers_every_object_kind_at_every_split_point() {
    let samples = [
        b"[true false null 1 -2 +3 1. .5 7 0 R /A#20B (x \\) y) <abc> << /K /V >>]\nendobj".as_slice(),
        b"[/Indexed 34471 0 R 255 34473 0 R]\rendobj".as_slice(),
        b"/Name#20with#23escapes\nendobj".as_slice(),
        b"(literal (nested) \\) text)\nendobj".as_slice(),
        b"<0a B>\nendobj".as_slice(),
        b"false\nendobj".as_slice(),
        b"-123.5\nendobj".as_slice(),
        b"4294967295 65535 R\nendobj".as_slice(),
    ];
    for sample in samples {
        assert!(parse_object_body(sample, (1, 0), 0).is_ok(), "{sample:?}");
        for split in 0..sample.len() {
            let mut framer = DirectObjectFramer::new();
            let first = framer.advance(&sample[..split]);
            if first != FrameStatus::Ready {
                assert_eq!(framer.advance(sample), FrameStatus::Ready, "split {split}: {sample:?}");
            }
            assert!(framer.scanned_work <= sample.len());
        }
    }
}

#[test]
fn direct_object_framer_rejects_invalid_tokens_without_parsing() {
    let invalid = [
        b"@".as_slice(),
        b"truX ".as_slice(),
        b"<0g> ".as_slice(),
        b"[1 0 Q] ".as_slice(),
        b"[>> ".as_slice(),
        b"<< 1 /NotAKey >> ".as_slice(),
        b"<< /MissingValue >> ".as_slice(),
    ];
    for sample in invalid {
        for split in 0..=sample.len() {
            let mut framer = DirectObjectFramer::new();
            let _ = framer.advance(&sample[..split]);
            assert_eq!(
                framer.advance(sample),
                FrameStatus::Invalid,
                "split {split}: {sample:?}"
            );
            assert!(framer.scanned_work <= sample.len());
        }
    }
}

#[test]
fn malformed_top_level_tokens_match_eager_prefix_objects_and_consumption() {
    let cases = [
        (b"trueX".as_slice(), Object::Boolean(true), 4),
        (b"1x".as_slice(), Object::Integer(1), 1),
        (b"1 0 RX".as_slice(), Object::Reference((1, 0)), 5),
        (b"/bad#G0".as_slice(), Object::Name(b"bad".to_vec()), 4),
        (b"1.2.3".as_slice(), Object::Real(1.2), 3),
    ];

    for (body, expected, expected_consumed) in cases {
        let (consumed, parsed) = crate::parser::direct_object_with_consumed(body).unwrap();
        assert_eq!(parsed, expected, "{body:?}");
        assert_eq!(consumed, expected_consumed, "{body:?}");

        for split in 0..body.len() {
            let mut framer = DirectObjectFramer::new();
            let first = framer.advance(&body[..split]);
            if first != FrameStatus::Ready {
                assert_eq!(framer.advance(body), FrameStatus::Ready, "split {split}: {body:?}");
            }
            assert!(framer.scanned_work <= body.len() + 6, "split {split}: {body:?}");
        }

        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body,
        }]);
        let eager = Document::load_mem(&pdf).unwrap();
        let indexed = open_reader(&pdf, ResolverLimits::default())
            .resolve_object((1, 0))
            .unwrap();
        assert_eq!(eager.objects.get(&(1, 0)).unwrap(), &expected, "{body:?}");
        assert_eq!(indexed, expected, "{body:?}");
    }
}

#[test]
fn malformed_prefixes_remain_parser_compatible_when_nested() {
    for body in [
        b"[trueX] ".as_slice(),
        b"[1x] ".as_slice(),
        b"[1 0 RX] ".as_slice(),
        b"[/bad#G0] ".as_slice(),
    ] {
        assert!(crate::parser::direct_object_with_consumed(body).is_none(), "{body:?}");
        let mut framer = DirectObjectFramer::new();
        assert_eq!(framer.advance(body), FrameStatus::Invalid, "{body:?}");
    }

    let body = b"[1.2.3] ";
    let (consumed, expected) = crate::parser::direct_object_with_consumed(body).unwrap();
    assert_eq!(consumed, body.len());
    assert_eq!(expected, Object::Array(vec![Object::Real(1.2), Object::Real(0.3)]));
    let mut framer = DirectObjectFramer::new();
    assert_eq!(framer.advance(body), FrameStatus::Ready);
}

#[test]
fn literal_nesting_limit_matches_eager_at_exact_boundary_and_one_beyond() {
    let literal = |levels: usize| {
        let mut body = Vec::with_capacity(levels * 2 + 1);
        body.extend(std::iter::repeat_n(b'(', levels));
        body.extend(std::iter::repeat_n(b')', levels));
        body.push(b' ');
        body
    };

    let accepted = literal(crate::reader::MAX_BRACKET + 1);
    assert!(crate::parser::direct_object_with_consumed(&accepted).is_some());
    let mut framer = DirectObjectFramer::new();
    assert_eq!(framer.advance(&accepted), FrameStatus::Ready);
    let accepted_pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: &accepted,
    }]);
    let eager = Document::load_mem(&accepted_pdf).unwrap();
    assert_eq!(
        open_reader(&accepted_pdf, ResolverLimits::default())
            .resolve_object((1, 0))
            .unwrap(),
        eager.objects.get(&(1, 0)).unwrap().clone()
    );

    let rejected = literal(crate::reader::MAX_BRACKET + 2);
    assert!(crate::parser::direct_object_with_consumed(&rejected).is_none());
    let mut framer = DirectObjectFramer::new();
    assert_eq!(framer.advance(&rejected), FrameStatus::Invalid);

    let pdf = object_pdf(&[ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: &rejected,
    }]);
    let eager = Document::load_mem(&pdf).unwrap();
    assert!(!eager.objects.contains_key(&(1, 0)));
    assert!(matches!(
        open_reader(&pdf, ResolverLimits::default()).resolve_object((1, 0)),
        Err(IndexedReaderError::InvalidIndirectObject { .. })
    ));
}

#[test]
fn failed_reference_probe_does_not_revisit_long_space_or_comments() {
    let mut body = b"[1 ".to_vec();
    body.extend(std::iter::repeat_n(b' ', 512 * 1_024));
    body.extend_from_slice(b"% gap");
    body.extend(std::iter::repeat_n(b'x', 512 * 1_024));
    body.extend_from_slice(b"\n2 ");
    body.extend(std::iter::repeat_n(b' ', 512 * 1_024));
    body.extend_from_slice(b"% tail");
    body.extend(std::iter::repeat_n(b'y', 512 * 1_024));
    body.extend_from_slice(b"\n3] ");

    let (_, expected) = crate::parser::direct_object_with_consumed(&body).unwrap();
    assert_eq!(
        expected,
        Object::Array(vec![Object::Integer(1), Object::Integer(2), Object::Integer(3)])
    );

    let mut framer = DirectObjectFramer::new();
    for end in (1..body.len()).step_by(7_919) {
        assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
    }
    assert_eq!(framer.advance(&body), FrameStatus::Ready);
    assert!(
        framer.scanned_work <= body.len() + 6,
        "work={} input={}",
        framer.scanned_work,
        body.len()
    );
}

#[test]
fn reference_probe_distinguishes_adjacent_decimal_from_separated_scalar() {
    let mut spaced = b"[1 2".to_vec();
    spaced.extend(std::iter::repeat_n(b' ', 127));
    spaced.extend_from_slice(b".3] ");

    let mut commented = b"[1 2%".to_vec();
    commented.extend(std::iter::repeat_n(b'x', 256 * 1_024));
    commented.extend_from_slice(b"\n.3] ");

    for body in [b"[1 2.3] ".to_vec(), spaced, commented] {
        let expected = if body == b"[1 2.3] " {
            Object::Array(vec![Object::Integer(1), Object::Real(2.3)])
        } else {
            Object::Array(vec![Object::Integer(1), Object::Integer(2), Object::Real(0.3)])
        };
        assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

        let mut framer = DirectObjectFramer::new();
        for end in (1..body.len()).step_by(7_919) {
            assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
        }
        assert_eq!(framer.advance(&body), FrameStatus::Ready);
        assert!(
            framer.scanned_work <= body.len() + 6,
            "work={} input={} expected={expected:?}",
            framer.scanned_work,
            body.len()
        );
    }
}

#[test]
fn leading_zero_reference_probe_continues_adjacent_real_without_replay() {
    let mut body = b"[1 ".to_vec();
    body.extend(std::iter::repeat_n(b'0', 256 * 1_024));
    body.extend_from_slice(b"2.3 -4.5 +.6] ");
    let expected = Object::Array(vec![
        Object::Integer(1),
        Object::Real(2.3),
        Object::Real(-4.5),
        Object::Real(0.6),
    ]);
    assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

    let dot = body.windows(b"2.3".len()).position(|window| window == b"2.3").unwrap() + 1;
    let minus = body
        .windows(b"-4.5".len())
        .position(|window| window == b"-4.5")
        .unwrap();
    let plus = body.windows(b"+.6".len()).position(|window| window == b"+.6").unwrap();
    let mut splits: Vec<_> = (OBJECT_GROWTH_CHUNK as usize..body.len())
        .step_by(OBJECT_GROWTH_CHUNK as usize)
        .collect();
    splits.extend([dot, dot + 1, minus, minus + 1, plus, plus + 1]);
    splits.sort_unstable();
    splits.dedup();

    for split in splits {
        let mut framer = DirectObjectFramer::new();
        assert_ne!(framer.advance(&body[..split]), FrameStatus::Invalid, "split {split}");
        assert_eq!(framer.advance(&body), FrameStatus::Ready, "split {split}");
        assert!(
            framer.scanned_work <= body.len() + 6,
            "split={split} work={} input={}",
            framer.scanned_work,
            body.len()
        );
    }
}

#[test]
fn zero_padded_generation_boundary_continues_numbers_without_replay() {
    let cases = [
        (b"65535".as_slice(), Object::Integer(65_535)),
        (b"65535.3".as_slice(), Object::Real(65_535.3)),
        (b"65536".as_slice(), Object::Integer(65_536)),
        (b"65536.3".as_slice(), Object::Real(65_536.3)),
        (b"-65536.3".as_slice(), Object::Real(-65_536.3)),
        (b"+65536.3".as_slice(), Object::Real(65_536.3)),
    ];

    for (suffix, value) in cases {
        let mut body = b"[1 ".to_vec();
        let sign = suffix.first().copied().filter(|byte| matches!(byte, b'+' | b'-'));
        if let Some(sign) = sign {
            body.push(sign);
        }
        body.extend(std::iter::repeat_n(b'0', 256 * 1_024));
        body.extend_from_slice(&suffix[usize::from(sign.is_some())..]);
        body.extend_from_slice(b"] ");
        let expected = Object::Array(vec![Object::Integer(1), value]);
        assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

        let number_end = body.len() - 2;
        let mut splits: Vec<_> = (OBJECT_GROWTH_CHUNK as usize..body.len())
            .step_by(OBJECT_GROWTH_CHUNK as usize)
            .collect();
        splits.extend(number_end.saturating_sub(8)..=number_end);
        splits.sort_unstable();
        splits.dedup();
        for split in splits {
            let mut framer = DirectObjectFramer::new();
            assert_ne!(framer.advance(&body[..split]), FrameStatus::Invalid, "split {split}");
            assert_eq!(framer.advance(&body), FrameStatus::Ready, "split {split}");
            assert!(
                framer.scanned_work <= body.len() + 6,
                "suffix={suffix:?} split={split} work={} input={}",
                framer.scanned_work,
                body.len()
            );
        }
    }
}

#[test]
fn stream_crlf_split_waits_for_lf_before_fixing_payload_offset() {
    let sample = b"<< /Length 1 >>\nstream\r\nx\nendstream";
    let split = sample.windows(2).position(|window| window == b"\r\n").unwrap() + 1;
    let mut framer = DirectObjectFramer::new();
    assert_eq!(framer.advance(&sample[..split]), FrameStatus::NeedMore);
    assert_eq!(framer.advance(sample), FrameStatus::Ready);
    let parsed = parse_object_body(sample, (1, 0), 0).unwrap();
    let prefix = usize::try_from(parsed.stream_prefix.unwrap()).unwrap();
    assert_eq!(&sample[parsed.consumed + prefix..parsed.consumed + prefix + 1], b"x");
}

#[test]
fn two_mib_literal_is_framed_linearly_and_parsed_once_from_nonoverlapping_growth_reads() {
    let literal_length = 2_usize * 1_024 * 1_024;
    let mut body = Vec::with_capacity(literal_length + 16);
    body.push(b'(');
    body.resize(literal_length + 1, b'x');
    body.extend_from_slice(b")\nendobj\n");

    let mut framer = DirectObjectFramer::new();
    for end in (1..body.len()).step_by(7_919) {
        assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
    }
    assert_eq!(framer.advance(&body), FrameStatus::Ready);
    assert!(framer.scanned_work <= body.len());

    let object_offset = 1_024_u64 * 1_024;
    let mut object = b"1 0 obj\n".to_vec();
    object.extend_from_slice(&body);
    let xref = object_offset + u64::try_from(object.len()).unwrap() + 1_024;
    let xref_bytes = format!(
        "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .into_bytes();
    let source = Arc::new(OverlaySource {
        len: xref + u64::try_from(xref_bytes.len()).unwrap(),
        regions: vec![(0, b"%PDF-1.7\n".to_vec()), (object_offset, object), (xref, xref_bytes)],
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(
        source.clone(),
        ResolverLimits {
            max_object_bytes: 3 * 1_024 * 1_024,
            ..ResolverLimits::default()
        },
    )
    .unwrap();
    source.requests.lock().unwrap().clear();
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));
    assert_eq!(
        reader.resolve_object((1, 0)).unwrap().as_str().unwrap().len(),
        literal_length
    );
    OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));

    let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
    let requests = source.requests.lock().unwrap();
    let mut growth: Vec<_> = requests
        .iter()
        .copied()
        .filter(|(offset, _)| *offset >= body_offset && *offset < xref)
        .collect();
    growth.sort_unstable();
    for pair in growth.windows(2) {
        assert!(pair[0].0 + u64::try_from(pair[0].1).unwrap() <= pair[1].0, "{pair:?}");
    }
}
