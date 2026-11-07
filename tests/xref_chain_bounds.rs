//! The bounds on one walk of the `/Prev` chain: small hostile files asking for a long walk, many reads or a huge
//! index. Every test says in its doc comment what failure it catches.

use flate2::{Compression, write::ZlibEncoder};
use lopdf::{BytesSource, IndexedReader, IndexedReaderError, IndexedReaderOptions};
use std::io::Write;

const BODY: &[u8] = b"%PDF-1.7\n1 0 obj\n<</Type/Catalog>>\nendobj\n";

/// `sections` classic sections, each pointing to the one before.
fn classic_chain(sections: usize) -> Vec<u8> {
    let mut bytes = BODY.to_vec();
    let mut prev = None;
    for _ in 0..sections {
        let xref = bytes.len();
        let chained = prev.map_or(String::new(), |prev: usize| format!("/Prev {prev}"));
        bytes.extend_from_slice(
            format!(
                "xref\n1 1\n0000000009 00000 n \ntrailer<</Size 2/Root 1 0 R{chained}>>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        prev = Some(xref);
    }
    bytes
}

fn with_cap(max_xref_sections: usize) -> IndexedReaderOptions {
    IndexedReaderOptions {
        max_xref_sections,
        ..IndexedReaderOptions::default()
    }
}

/// A hostile chain of sections keeping an open busy. It has to fail with the named limit, and a rescan mustn't step
/// around it.
#[test]
fn a_long_chain_fails_with_a_clear_error_at_the_default_cap() {
    let result = IndexedReader::open(BytesSource::from(classic_chain(100_000)));
    assert!(
        matches!(result, Err(IndexedReaderError::RevisionLimitExceeded { limit: 1_024 })),
        "{:?}",
        result.err()
    );
    assert!(result.err().unwrap().to_string().contains("1024"));
}

/// The option doing nothing, or being off by one in either direction.
#[test]
fn the_chosen_cap_is_the_one_that_applies() {
    let bytes = classic_chain(10);
    let opened = IndexedReader::open_with_options(BytesSource::from(bytes.clone()), with_cap(10));
    assert!(opened.is_ok());
    let refused = IndexedReader::open_with_options(BytesSource::from(bytes), with_cap(9));
    assert!(matches!(
        refused,
        Err(IndexedReaderError::RevisionLimitExceeded { limit: 9 })
    ));
}

/// 1,024 sections whose windows overlap the same 8 MiB: 16 GiB of reads from a small file, about 1.7 s per open on this
/// Mac before the bound.
#[test]
fn sections_overlapping_the_same_bytes_run_out_of_budget() {
    let head = |number: u64, prev: u64, length: u64| {
        format!(
            "{:04} 0 obj\n<</Type/XRef/Size 3/W[1 2 0]/Index[1 1]/Root 1 0 R/Prev {prev:010}/Length {length:010}>>\nstream\n",
            number + 1_000
        )
        .into_bytes()
    };
    let head_len = head(0, 0, 0).len() as u64;
    let padding = 8_u64 * 1_024 * 1_024;
    let sections = 1_024_u64;
    let start = BODY.len() as u64;
    let end = start + head_len * sections + padding;
    let mut bytes = BODY.to_vec();
    for section in 0..sections {
        let offset = start + section * head_len;
        bytes.extend_from_slice(&head(section, offset + head_len, end - (offset + head_len)));
    }
    bytes.extend(std::iter::repeat_n(1_u8, padding as usize));
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
    let result = IndexedReader::open(BytesSource::from(bytes));
    assert!(
        matches!(
            result,
            Err(IndexedReaderError::StructureLimitExceeded {
                structure: "cross-reference chain",
                ..
            })
        ),
        "{:?}",
        result.err()
    );
}

/// Deflated sections naming a million numbers each: a 2 MiB file took 47 GiB and 138 s to open on this Mac before the
/// bound, which is more than the machine has.
#[test]
fn deflated_sections_cant_grow_the_index_without_limit() {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![0_u8; 2_000_000]).unwrap();
    let payload = encoder.finish().unwrap();
    let mut bytes = BODY.to_vec();
    let mut prev = None;
    let mut last = 0;
    for section in 0..8_u64 {
        last = bytes.len();
        let chained = prev.map_or(String::new(), |prev: usize| format!("/Prev {prev}"));
        bytes.extend_from_slice(
            format!(
                "{} 0 obj\n<</Type/XRef/Size 3/W[1 1 0]/Index[{} 1000000]/Root 1 0 R/Filter/FlateDecode{chained}/Length {}>>\nstream\n",
                section + 100,
                1_000 + section * 1_000_000,
                payload.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        prev = Some(last);
    }
    bytes.extend_from_slice(format!("startxref\n{last}\n%%EOF\n").as_bytes());
    let result = IndexedReader::open(BytesSource::from(bytes));
    assert!(
        matches!(result, Err(IndexedReaderError::EntryLimitExceeded { .. })),
        "{:?}",
        result.err()
    );
}

/// The bound catching real files: a chain of ordinary updates has to open at any length up to the cap.
#[test]
fn a_chain_of_ordinary_updates_opens_at_the_cap() {
    let reader = IndexedReader::open(BytesSource::from(classic_chain(1_024))).unwrap();
    assert!(!reader.index_stats().unwrap().recovered());
    assert_eq!(reader.object_ids().len(), 1);
}
