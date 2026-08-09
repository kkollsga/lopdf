use lopdf::xref::XrefEntry;
use lopdf::{BytesSource, Document, IndexedReader, LoadOptions, Object, ObjectId};
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy)]
struct ObjectStreamSpec {
    xref_key: u32,
    header_id: u32,
    winner: &'static str,
}

/// Build a PDF whose xref marks object 5 as an unused hole (`0000000000 65535 f`) while two
/// normal ObjStm entries both define it. `xref_key` deliberately remains separate from the
/// parsed indirect-object header, so a malformed key/header mismatch exercises the same path
/// under a container the outer table and the inner header disagree about.
fn duplicate_unindexed_object_streams(first: ObjectStreamSpec, second: ObjectStreamSpec) -> Vec<u8> {
    const MAX_XREF_ID: u32 = 800;

    let mut pdf = b"%PDF-1.5\n".to_vec();
    let mut offsets = BTreeMap::new();
    let mut indirect = |xref_key: u32, header_id: u32, body: &str| {
        offsets.insert(xref_key, pdf.len());
        pdf.extend_from_slice(format!("{header_id} 0 obj\n{body}\nendobj\n").as_bytes());
    };

    indirect(1, 1, "<< /Type /Catalog /Pages 2 0 R >>");
    indirect(2, 2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    indirect(3, 3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");

    for xref_key in 4..=MAX_XREF_ID {
        if xref_key == 5
            || xref_key == first.xref_key
            || xref_key == second.xref_key
            || xref_key == first.header_id
            || xref_key == second.header_id
        {
            continue;
        }
        indirect(xref_key, xref_key, "<< /Filler true >>");
    }

    for spec in [first, second] {
        let content = format!("5 0 << /Winner /{} >>", spec.winner);
        indirect(
            spec.xref_key,
            spec.header_id,
            &format!(
                "<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        );
    }

    let startxref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", MAX_XREF_ID + 1).as_bytes());
    for xref_key in 1..=MAX_XREF_ID {
        if let Some(offset) = offsets.get(&xref_key) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        } else {
            pdf.extend_from_slice(b"0000000000 65535 f \n");
        }
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF",
            MAX_XREF_ID + 1
        )
        .as_bytes(),
    );
    pdf
}

fn delay_container_100(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    if id == (100, 0) {
        thread::sleep(Duration::from_millis(75));
    }
    Some((id, object.clone()))
}

fn delay_mismatched_container_700(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    if id == (700, 0) {
        thread::sleep(Duration::from_millis(75));
    }
    Some((id, object.clone()))
}

/// The two engines' answer for object 5, which the fixture leaves outside the cross-reference
/// table. Both must refuse it: the eager loader by never expanding the member, the indexed reader
/// by having nothing to resolve.
fn assert_member_5_is_refused(document: &Document, pdf: &[u8], context: &str) {
    assert!(
        matches!(document.reference_table.get(5), Some(XrefEntry::UnusableFree) | None),
        "{context}: the fixture's premise is that the table does not place object 5"
    );
    assert!(
        !document.objects.contains_key(&(5, 0)),
        "{context}: an unindexed member must not be expanded into the object map"
    );
    assert!(
        matches!(
            document.get_object((5, 0)).map(|object| matches!(object, Object::Null)),
            Ok(true) | Err(_)
        ),
        "{context}: an unindexed member must not read back as a live object"
    );
    let lazy = IndexedReader::open(BytesSource::from(pdf.to_vec()))
        .expect("the indexed reader opens the fixture")
        .resolve_object((5, 0))
        .expect("resolving an unplaced id is not an error");
    assert!(
        matches!(lazy, Object::Null),
        "{context}: the indexed reader must agree that object 5 is not there"
    );
    assert_eq!(
        document.get_pages().len(),
        1,
        "{context}: the rest of the file is intact"
    );
}

/// Two `/ObjStm` containers both define object 5, which the cross-reference table places nowhere.
/// Neither copy may be loaded: the table is the sole authority on where an object lives, and it
/// does not say object 5 lives in either container (see `objstm_member_xref_authority_test`).
///
/// This test previously asserted the opposite — that the copy from the lower xref key was
/// *retained*, with the point being that duplicate members resolve in outer-xref order rather
/// than worker-completion order. That expectation pinned a defect: the same leniency resurrected
/// members a later revision had freed, and the indexed reader already answered `Null` here, so
/// the two engines disagreed on this very fixture. With unindexed members refused outright,
/// duplicates are structurally impossible and the ordering question it asked no longer exists;
/// `merge_object_stream_batches`'s xref-key sort survives as belt-and-braces.
///
/// What is still worth pinning, and is what this test now checks, is the other half of that
/// subject: the outcome does not depend on which worker finishes first. The filter still delays
/// container 100 past container 600, and the answer is the same on every run.
#[test]
fn duplicate_unindexed_members_are_refused_whichever_worker_finishes_first() {
    let pdf = duplicate_unindexed_object_streams(
        ObjectStreamSpec {
            xref_key: 100,
            header_id: 100,
            winner: "First",
        },
        ObjectStreamSpec {
            xref_key: 600,
            header_id: 600,
            winner: "Second",
        },
    );

    for run in 0..4 {
        let document = Document::load_mem_with_options(&pdf, LoadOptions::with_filter(delay_container_100))
            .expect("generated PDF loads");
        assert_member_5_is_refused(&document, &pdf, &format!("completion-inverted load {run}"));
    }
}

/// The same refusal when the outer xref key and the indirect-object header disagree about a
/// container's id. The reader stays lenient about the mismatch — the file still loads — but the
/// leniency does not extend to the member: object 5 is unplaced either way.
///
/// Before the strict rule this test asserted that the copy keyed by the *outer* xref entry won
/// over the one whose parsed header claimed a lower id, i.e. which of the two duplicates was
/// retained. That tie-break no longer has a case to decide, so what remains expressible is that
/// a mismatched container does not become a back door for an unindexed member.
#[test]
fn a_mismatched_xref_key_and_object_header_still_refuses_the_member() {
    let pdf = duplicate_unindexed_object_streams(
        ObjectStreamSpec {
            xref_key: 100,
            header_id: 700,
            winner: "FirstXrefEntry",
        },
        ObjectStreamSpec {
            xref_key: 600,
            header_id: 20,
            winner: "LowerParsedId",
        },
    );

    let document = Document::load_mem_with_options(&pdf, LoadOptions::with_filter(delay_mismatched_container_700))
        .expect("lenient reader accepts mismatched xref key and indirect-object header");
    assert_member_5_is_refused(&document, &pdf, "mismatched key and header");
}
