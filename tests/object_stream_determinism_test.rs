use lopdf::{Document, LoadOptions, Object, ObjectId};
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy)]
struct ObjectStreamSpec {
    xref_key: u32,
    header_id: u32,
    winner: &'static str,
}

/// Build a PDF whose xref leaves object 5 untracked while two normal ObjStm
/// entries both define it. `xref_key` deliberately remains separate from the
/// parsed indirect-object header so malformed key/header mismatches can verify
/// that eager loading preserves outer xref traversal order.
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

fn winner(document: &Document) -> &[u8] {
    document
        .get_object((5, 0))
        .and_then(Object::as_dict)
        .and_then(|dictionary| dictionary.get(b"Winner"))
        .and_then(Object::as_name)
        .expect("duplicate unindexed member is retained")
}

#[test]
fn duplicate_unindexed_members_follow_xref_order_not_worker_completion() {
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
        assert_eq!(
            winner(&document),
            b"First",
            "wrong winner on completion-inverted load {run}"
        );
    }
}

#[test]
fn duplicate_priority_uses_the_outer_xref_key_when_the_object_header_disagrees() {
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
    assert_eq!(winner(&document), b"FirstXrefEntry");
}
