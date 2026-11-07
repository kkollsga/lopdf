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

/// The bounds refusing a real file with no way back. Counting each number every time it was listed stopped a 200,000-object file at about
/// 21 updates that list every number, which some producers write, and a fixed 256 MiB read limit stopped it as well, since growing the
/// window for each table of a little over 4 MiB reads about 16 MiB.
#[test]
fn a_large_file_with_many_full_updates_opens() {
    let objects = 220_000_u64;
    let updates = 21;
    let mut table = Vec::with_capacity(objects as usize * 20 + 32);
    table.extend_from_slice(format!("xref\n1 {objects}\n").as_bytes());
    for _ in 0..objects {
        table.extend_from_slice(b"0000000009 00000 n \n");
    }
    let mut bytes = BODY.to_vec();
    let mut prev = None;
    let mut last = 0;
    for _ in 0..updates {
        last = bytes.len();
        bytes.extend_from_slice(&table);
        let chained = prev.map_or(String::new(), |prev: usize| format!("/Prev {prev}"));
        bytes.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R{chained}>>\nstartxref\n{last}\n%%EOF\n",
                objects + 1
            )
            .as_bytes(),
        );
        prev = Some(last);
    }
    let _ = last;
    let reader = IndexedReader::open(BytesSource::from(bytes)).unwrap();
    assert!(!reader.index_stats().unwrap().recovered());
    assert_eq!(reader.object_ids().len(), objects as usize);
}

/// A chain of deflated stream sections, each with its own `/W`, `/Index` and body.
fn deflated_stream_chain(sections: u64, widths: &str, index: &str, inflated: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(inflated).unwrap();
    let payload = encoder.finish().unwrap();
    let mut bytes = BODY.to_vec();
    let mut prev = None;
    let mut last = 0;
    for section in 0..sections {
        last = bytes.len();
        let chained = prev.map_or(String::new(), |prev: usize| format!("/Prev {prev}"));
        bytes.extend_from_slice(
            format!(
                "{} 0 obj\n<</Type/XRef/Size 3/W[{widths}]/Index[{index}]/Root 1 0 R/Filter/FlateDecode{chained}/Length {}>>\nstream\n",
                section + 100,
                payload.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        prev = Some(last);
    }
    bytes.extend_from_slice(format!("startxref\n{last}\n%%EOF\n").as_bytes());
    bytes
}

/// Rows of an unknown type being skipped at no charge. They leave nothing in the index, so a limit counted after decoding never saw them, and a
/// walk could decode a billion rows. The limit has to stop a real walk over real rows, before any more of them are decoded.
#[test]
fn rows_of_an_unknown_type_are_charged_before_decoding() {
    let bytes = deflated_stream_chain(40, "1 0 0", "1000 1000000", &vec![3_u8; 1_000_000]);
    let result = IndexedReader::open(BytesSource::from(bytes));
    assert!(
        matches!(
            result,
            Err(IndexedReaderError::EntryLimitExceeded { limit: 33_554_432, .. })
        ),
        "{:?}",
        result.err()
    );
}

/// A section declaring one row and inflating 32 MiB. Nothing charged for it, so 1,024 of them inflated 24 GiB and more: 52 s and 1.6 GiB from a 60 MiB
/// file. The limit on inflated bytes has to stop a real walk.
#[test]
fn inflating_far_more_than_a_section_declares_runs_out_of_budget() {
    let bytes = deflated_stream_chain(12, "1 1 1", "5 1", &vec![0_u8; 30_000_000]);
    let result = IndexedReader::open(BytesSource::from(bytes));
    assert!(
        matches!(
            result,
            Err(IndexedReaderError::StructureLimitExceeded {
                structure: "inflated cross-reference data",
                ..
            })
        ),
        "{:?}",
        result.err()
    );
}

/// The inflate limit catching a real file: ordinary deflated sections, enough for a long history, have to open.
#[test]
fn ordinary_deflated_sections_open_within_the_limit() {
    let mut rows = Vec::with_capacity(140_000);
    for _ in 0..20_000 {
        rows.extend_from_slice(&[1, 0, 0, 0, 9, 0, 0]);
    }
    let bytes = deflated_stream_chain(100, "1 4 2", "1 20000", &rows);
    let reader = IndexedReader::open(BytesSource::from(bytes)).unwrap();
    assert!(!reader.index_stats().unwrap().recovered());
    assert_eq!(reader.object_ids().len(), 20_000);
}

/// The bound catching real files: a chain of ordinary updates has to open at any length up to the cap.
#[test]
fn a_chain_of_ordinary_updates_opens_at_the_cap() {
    let reader = IndexedReader::open(BytesSource::from(classic_chain(1_024))).unwrap();
    assert!(!reader.index_stats().unwrap().recovered());
    assert_eq!(reader.object_ids().len(), 1);
}
