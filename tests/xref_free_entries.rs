//! Free cross-reference entries are how a PDF *deletes* an object.
//!
//! A revision that drops an object writes an `f` row (or a type-0 row in a
//! cross-reference stream) for its number. A reference to the deleted object is
//! then a reference to null (ISO 32000-1, 7.3.10). A reader that discards those
//! rows never sees the deletion and resurrects the object from the older `/Prev`
//! section instead.

#![cfg(not(feature = "async"))]

use lopdf::xref::XrefEntry;
use lopdf::{Document, Object, dictionary};

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
/// `None` writes the free-list head spelling, `0000000000 65535 f`.
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

/// Three revisions around one object: revision 1 defines object 5, revision 2
/// **frees** it, revision 3 defines it again. `revisions` picks how many are
/// written, so the same body reads as live / deleted / redefined.
fn deleted_object_pdf(revisions: usize) -> Vec<u8> {
    assert!((1..=3).contains(&revisions));
    let (mut pdf, base_offsets) = basic_body("deletion");
    let doomed = push_object(&mut pdf, 5, b"<< /RevisionObject (doomed) >>");
    let mut entries: Vec<Option<usize>> = base_offsets.into_iter().map(Some).collect();
    entries.push(Some(doomed));
    let base_xref = append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        "<< /Size 6 /Root 1 0 R /Info 4 0 R >>".to_string()
    });
    if revisions == 1 {
        return pdf;
    }

    // Revision 2 deletes object 5: the free-list head points at it, and its own
    // entry links back to the head carrying the generation a reuse would take.
    // Written by hand because it is the *non*-65535 free flavour, which
    // `append_classic_revision` cannot spell.
    let delete_xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 1\n0000000005 65535 f \n5 1\n0000000000 00001 f \n");
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size 6 /Root 1 0 R /Info 4 0 R /Prev {base_xref} >>\
             \nstartxref\n{delete_xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    if revisions == 2 {
        return pdf;
    }

    let revived = push_object(&mut pdf, 5, b"<< /RevisionObject (revived) >>");
    append_classic_revision(&mut pdf, vec![(0, vec![None]), (5, vec![Some(revived)])], |_| {
        format!("<< /Size 6 /Root 1 0 R /Info 4 0 R /Prev {delete_xref} >>")
    });
    pdf
}

fn encode_xref_entry(entry_type: u8, field_2: u32, field_3: u16, output: &mut Vec<u8>) {
    output.push(entry_type);
    output.extend_from_slice(&field_2.to_be_bytes());
    output.extend_from_slice(&field_3.to_be_bytes());
}

/// The same deletion expressed with cross-reference *streams*: the newer section
/// holds a type-0 row for object 5.
fn deleted_object_xref_stream_pdf() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("deletion");
    let doomed = push_object(&mut pdf, 5, b"<< /RevisionObject (doomed) >>");

    let base_xref = pdf.len();
    let mut table = Vec::new();
    encode_xref_entry(0, 0, 65535, &mut table);
    for offset in offsets.iter().skip(1) {
        encode_xref_entry(1, *offset as u32, 0, &mut table);
    }
    encode_xref_entry(1, doomed as u32, 0, &mut table);
    encode_xref_entry(1, base_xref as u32, 0, &mut table);
    pdf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 7 /Root 1 0 R /Info 4 0 R /W [1 4 2] /Length {} >>\nstream\n",
            table.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&table);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{base_xref}\n%%EOF\n").as_bytes());

    let delete_xref = pdf.len();
    let mut deletion = Vec::new();
    encode_xref_entry(0, 0, 1, &mut deletion);
    encode_xref_entry(1, delete_xref as u32, 0, &mut deletion);
    pdf.extend_from_slice(
        format!(
            "7 0 obj\n<< /Type /XRef /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
             /Index [5 1 7 1] /W [1 4 2] /Length {} >>\nstream\n",
            deletion.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&deletion);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{delete_xref}\n%%EOF\n").as_bytes());
    pdf
}

fn revision_object(document: &Document, id: u32) -> Option<String> {
    let object = document.get_object((id, 0)).ok()?;
    let name = object.as_dict().ok()?.get(b"RevisionObject").ok()?.as_str().ok()?;
    Some(String::from_utf8_lossy(name).into_owned())
}

// The classic table dropped `f` rows outright, so `Xref::merge` found no entry
// for the deleted object and the older `/Prev` section's `Normal` entry won —
// the reader un-deleted content the file says is gone.
#[test]
fn a_newer_revision_free_entry_masks_the_older_definition() {
    let live = Document::load_mem(&deleted_object_pdf(1)).unwrap();
    assert_eq!(revision_object(&live, 5).as_deref(), Some("doomed"));

    let deleted = Document::load_mem(&deleted_object_pdf(2)).unwrap();
    assert!(matches!(deleted.reference_table.get(5), Some(XrefEntry::Free)));
    assert_eq!(revision_object(&deleted, 5), None);
    assert!(deleted.get_object((5, 0)).is_err());
    assert!(!deleted.has_object((5, 0)));
}

// The cross-reference stream decoder read a type-0 row's two fields and threw
// them away, with the same consequence.
#[test]
fn a_type_zero_xref_stream_row_masks_the_older_definition() {
    let deleted = Document::load_mem(&deleted_object_xref_stream_pdf()).unwrap();

    assert!(matches!(deleted.reference_table.get(5), Some(XrefEntry::Free)));
    assert_eq!(revision_object(&deleted, 5), None);
    assert!(deleted.get_object((5, 0)).is_err());
    assert_eq!(deleted.get_pages().len(), 1);
}

// Sections merge newest-first and the newest wins, so a free entry only ever
// masks what is *older* than it. Recording free entries must not invert that.
#[test]
fn an_older_revision_free_entry_does_not_delete_a_newer_definition() {
    let document = Document::load_mem(&deleted_object_pdf(3)).unwrap();

    assert!(matches!(
        document.reference_table.get(5),
        Some(XrefEntry::Normal { generation: 0, .. })
    ));
    assert_eq!(revision_object(&document, 5).as_deref(), Some("revived"));
}

// `Xref::size` — and `Document::max_id`, which is `size - 1` and numbers every
// object a later save appends — is derived from `Xref::max_id`. Counting a
// trailing free entry there would renumber saved output purely because some
// revision deleted an object, which is what kept the parsers from recording free
// entries at all.
#[test]
fn a_deleted_object_does_not_move_the_id_a_save_numbers_from() {
    let mut document = Document::load_mem(&deleted_object_pdf(2)).unwrap();

    // The trailer still declares `/Size 6` because object 5 once existed; the
    // deletion must leave the id space at 4 so new objects are numbered from 5.
    assert_eq!(document.max_id, 4);
    assert_eq!(document.reference_table.size, 5);
    assert_eq!(
        document.add_object(dictionary! { "RevisionObject" => Object::string_literal("added") }),
        (5, 0)
    );

    let mut saved = Vec::new();
    document.save_to(&mut saved).unwrap();

    // A full save writes a fresh table from the objects it holds, so the deletion
    // does not survive it — but neither does the object that was deleted.
    let reloaded = Document::load_mem(&saved).unwrap();
    assert_eq!(reloaded.max_id, 5);
    assert_eq!(reloaded.reference_table.size, 6);
    assert_eq!(revision_object(&reloaded, 5).as_deref(), Some("added"));
    assert_eq!(reloaded.get_pages().len(), 1);
}
