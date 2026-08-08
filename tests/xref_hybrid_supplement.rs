//! Hybrid-reference files (ISO 32000-1, 7.5.8.4) pair a classic cross-reference
//! section with an `/XRefStm` supplement that describes the *same revision's*
//! compressed objects. The classic section is required to mask those objects as
//! free, so a reader that understands object streams has to read the supplement
//! of every section it visits, and has to let it win inside that section.

#![cfg(not(feature = "async"))]

use lopdf::Document;
use lopdf::xref::XrefEntry;

fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
    let offset = pdf.len();
    pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    offset
}

/// A one-page document, objects 1..=4, with `/Info` carrying `title`.
fn basic_body(title: &str) -> (Vec<u8>, Vec<usize>) {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    let mut offsets = vec![0];
    offsets.push(push_object(&mut pdf, 1, b"<< /Type /Catalog /Pages 2 0 R >>"));
    offsets.push(push_object(&mut pdf, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"));
    offsets.push(push_object(
        &mut pdf,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
    ));
    offsets.push(push_object(&mut pdf, 4, format!("<< /Title ({title}) >>").as_bytes()));
    (pdf, offsets)
}

/// Append a classic `xref` section plus trailer, and return the section's offset.
fn append_classic_revision<F>(pdf: &mut Vec<u8>, sections: Vec<(u32, Vec<Option<usize>>)>, trailer: F) -> usize
where
    F: FnOnce(usize) -> String,
{
    let xref_start = pdf.len();
    pdf.extend_from_slice(b"xref\n");
    for (starting_id, entries) in sections {
        pdf.extend_from_slice(format!("{starting_id} {}\n", entries.len()).as_bytes());
        for entry in entries {
            match entry {
                Some(offset) => pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()),
                None => pdf.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }
    }
    pdf.extend_from_slice(b"trailer\n");
    pdf.extend_from_slice(trailer(xref_start).as_bytes());
    pdf.extend_from_slice(format!("\nstartxref\n{xref_start}\n%%EOF\n").as_bytes());
    xref_start
}

fn encode_xref_entry(entry_type: u8, field_2: u32, field_3: u16, output: &mut Vec<u8>) {
    output.push(entry_type);
    output.extend_from_slice(&field_2.to_be_bytes());
    output.extend_from_slice(&field_3.to_be_bytes());
}

/// Write an object stream holding a single member, plus the `/XRefStm` supplement
/// that maps object 7 to it. Returns the supplement's offset.
fn push_supplement(pdf: &mut Vec<u8>, container_id: u32, supplement_id: u32, member: &[u8]) -> (usize, usize) {
    let object_stream = format!("<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n", member.len());
    let container_offset = pdf.len();
    pdf.extend_from_slice(format!("{container_id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(object_stream.as_bytes());
    pdf.extend_from_slice(member);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let supplement_offset = pdf.len();
    let mut supplement = Vec::new();
    encode_xref_entry(2, container_id, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "{supplement_id} 0 obj\n<< /Type /XRef /Size {} /Index [7 1] /W [1 4 2] /Length {} >>\nstream\n",
            supplement_id + 1,
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    (container_offset, supplement_offset)
}

/// An incremental update whose *newest* section is the hybrid one: object 7 lives
/// only in the supplement that section names.
fn hybrid_incremental_pdf() -> (Vec<u8>, usize) {
    let (mut pdf, base_offsets) = basic_body("hybrid");
    let base_entries = base_offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(&mut pdf, vec![(0, base_entries)], |_| {
        "<< /Size 5 /Root 1 0 R /Info 4 0 R >>".to_string()
    });

    let (container_offset, supplement_offset) = push_supplement(&mut pdf, 5, 6, b"7 0 << /Hybrid true >>");

    let hybrid_xref = append_classic_revision(
        &mut pdf,
        vec![(5, vec![Some(container_offset), Some(supplement_offset)])],
        |_| {
            format!(
                "<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /XRefStm {supplement_offset} >>"
            )
        },
    );
    (pdf, hybrid_xref)
}

/// A hybrid file with no `/Prev` at all: the only section carries the supplement.
fn xrefstm_without_prev_pdf() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("no-prev");
    let (container_offset, supplement_offset) = push_supplement(&mut pdf, 5, 6, b"7 0 << /Supplement true >>");

    let mut entries: Vec<_> = offsets.into_iter().map(Some).collect();
    entries.extend([Some(container_offset), Some(supplement_offset)]);
    append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        format!("<< /Size 8 /Root 1 0 R /Info 4 0 R /XRefStm {supplement_offset} >>")
    });
    pdf
}

/// An older section defines object 7 directly; the newest section's supplement
/// redefines it as a compressed object. The newest revision must win.
fn hybrid_collision_pdf() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("collision");
    let previous_object_offset = push_object(&mut pdf, 7, b"<< /Winner (previous-table) >>");
    let base_entries = offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(
        &mut pdf,
        vec![(0, base_entries), (7, vec![Some(previous_object_offset)])],
        |_| "<< /Size 8 /Root 1 0 R /Info 4 0 R >>".to_string(),
    );

    let (container_offset, supplement_offset) = push_supplement(&mut pdf, 8, 9, b"7 0 << /Winner (supplement) >>");

    append_classic_revision(
        &mut pdf,
        vec![(8, vec![Some(container_offset), Some(supplement_offset)])],
        |_| {
            format!(
                "<< /Size 10 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /XRefStm {supplement_offset} >>"
            )
        },
    );
    pdf
}

/// A chain whose newest section is an ordinary incremental update and whose
/// *older* section is the hybrid one — the shape a linearized hybrid file takes
/// after any edit. The supplement is then reachable only by reading each
/// section's own `/XRefStm`.
fn hybrid_under_a_newer_plain_revision_pdf() -> Vec<u8> {
    let (mut pdf, hybrid_xref) = hybrid_incremental_pdf();
    let marker = push_object(&mut pdf, 10, b"<< /Marker (newest) >>");
    append_classic_revision(&mut pdf, vec![(10, vec![Some(marker)])], |_| {
        format!("<< /Size 11 /Root 1 0 R /Info 4 0 R /Prev {hybrid_xref} >>")
    });
    pdf
}

fn compressed_member(document: &Document, key: &[u8]) -> Option<Vec<u8>> {
    let object = document.get_object((7, 0)).ok()?;
    let dict = object.as_dict().ok()?;
    match dict.get(key).ok()? {
        lopdf::Object::Boolean(value) => Some(vec![u8::from(*value)]),
        other => other.as_str().ok().map(<[u8]>::to_vec),
    }
}

// The supplement of the section that declares it is read even when that section
// is the newest one and the chain stops there. Before, `/XRefStm` was only read
// inside the `/Prev` loop, so a file without a `/Prev` never read it at all: the
// key was left in the trailer and object 7 had no cross-reference entry.
#[test]
fn a_lone_section_reads_its_own_xrefstm_supplement() {
    let document = Document::load_mem(&xrefstm_without_prev_pdf()).unwrap();

    assert!(
        document.trailer.get(b"XRefStm").is_err(),
        "the supplement is consumed with the section that declares it"
    );
    assert!(matches!(
        document.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 5, index: 0 })
    ));
    assert_eq!(compressed_member(&document, b"Supplement"), Some(vec![1]));
}

// The supplement was read off the *newest* trailer while walking `/Prev`, so a
// chain whose newest section carries no `/XRefStm` — an ordinary incremental
// update on top of a linearized hybrid file — dropped every compressed object in
// the document.
#[test]
fn an_older_sections_supplement_is_still_read_under_a_newer_revision() {
    let document = Document::load_mem(&hybrid_under_a_newer_plain_revision_pdf()).unwrap();

    assert!(matches!(
        document.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 5, index: 0 })
    ));
    assert_eq!(compressed_member(&document, b"Hybrid"), Some(vec![1]));
    assert_eq!(document.get_pages().len(), 1);
}

// The straightforward hybrid chain keeps working, and the key is consumed rather
// than left behind in the trailer.
#[test]
fn a_hybrid_revision_contributes_its_compressed_objects() {
    let (pdf, _) = hybrid_incremental_pdf();
    let document = Document::load_mem(&pdf).unwrap();

    assert!(document.trailer.get(b"XRefStm").is_err());
    assert!(matches!(
        document.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 5, index: 0 })
    ));
    assert_eq!(compressed_member(&document, b"Hybrid"), Some(vec![1]));
}

// Precedence within a revision: the supplement describes the same revision as the
// section that names it, so it supersedes that section — and the section as a
// whole still beats anything older. Merging the supplement *after* an older
// `/Prev` section inverted this and resolved object 7 to the stale definition.
#[test]
fn a_supplement_wins_a_collision_with_an_older_section() {
    let document = Document::load_mem(&hybrid_collision_pdf()).unwrap();

    assert!(matches!(
        document.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 8, index: 0 })
    ));
    assert_eq!(
        compressed_member(&document, b"Winner").as_deref(),
        Some(&b"supplement"[..])
    );
}

// `Document::load_metadata_mem` walks the same chain through its own copy of the
// loop, so it has to agree with the full load.
#[test]
fn the_metadata_reader_walks_the_same_supplements() {
    for pdf in [
        xrefstm_without_prev_pdf(),
        hybrid_incremental_pdf().0,
        hybrid_under_a_newer_plain_revision_pdf(),
        hybrid_collision_pdf(),
    ] {
        let document = Document::load_mem(&pdf).unwrap();
        let metadata = Document::load_metadata_mem(&pdf).unwrap();
        assert_eq!(metadata.version, document.version);
        assert_eq!(metadata.page_count, document.get_pages().len() as u32);
    }
}
