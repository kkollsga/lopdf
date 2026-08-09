//! The merged cross-reference table is the sole authority on where an object lives.
//!
//! An `/ObjStm` container is just bytes: it keeps carrying every member it was written with, long
//! after a later revision freed one of them, replaced one with a normal object, or moved one into
//! a different container. Eager loading (`Reader::load_objects_raw`) expands containers it finds
//! at `Normal` xref offsets, so without a check it would hand those stale copies to callers, while
//! the indexed reader — which resolves through the table — answers `Null` per ISO 32000-1, 7.3.10.
//!
//! This matrix pins that the two engines agree on every shape a real writer can produce, and that
//! the answer they agree on is the one the merged table dictates. Object 7 is the member under
//! test and object 5 is its container throughout, so the variants differ only in how the file
//! talks about 7.

use lopdf::xref::XrefEntry;
use lopdf::{BytesSource, Document, IndexedReader, Object};

// ------------------------------------------------------------------ fixture construction

fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
    let offset = pdf.len();
    pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    offset
}

/// Objects 1..=4: catalog, page tree, one page, info. Returns the buffer and the offset of each
/// object, indexed by its id (slot 0 is unused, mirroring the xref's own free head).
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

/// An `/ObjStm` holding the single member `member_id`, written as object `container_id`.
fn push_objstm(pdf: &mut Vec<u8>, container_id: u32, member_id: u32, body: &str) -> usize {
    let member = format!("{member_id} 0 {body}");
    let first = format!("{member_id} 0 ").len();
    let offset = pdf.len();
    pdf.extend_from_slice(format!("{container_id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(
        format!(
            "<< /Type /ObjStm /N 1 /First {first} /Length {} >>\nstream\n",
            member.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(member.as_bytes());
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    offset
}

/// One cross-reference row, in whichever of the two encodings the section uses.
#[derive(Clone, Copy)]
enum Row {
    /// A type-0 row with a usable generation: the deletion marker a writer emits when it drops an
    /// object and expects the id to be reused.
    Free,
    /// A type-0 row with generation 65535: what a writer emits both for a generation-exhausted
    /// deletion and for a hole it never used. Indistinguishable at this layer, so both must land
    /// on the same answer.
    Unusable,
    Normal(usize),
    Compressed(u32, u16),
}

fn encode(entry_type: u8, f2: u32, f3: u16, out: &mut Vec<u8>) {
    out.push(entry_type);
    out.extend_from_slice(&f2.to_be_bytes());
    out.extend_from_slice(&f3.to_be_bytes());
}

/// Append a cross-reference **stream** revision. `runs` are `(starting_id, rows)`, mapping onto
/// the `/Index` pairs.
fn append_xref_stream(
    pdf: &mut Vec<u8>, self_id: u32, runs: &[(u32, Vec<Row>)], size: u32, prev: Option<usize>,
) -> usize {
    let xref_start = pdf.len();
    let mut index = Vec::new();
    let mut data = Vec::new();
    for (start, rows) in runs {
        index.push(format!("{start} {}", rows.len()));
        for row in rows {
            match row {
                Row::Free => encode(0, 0, 0, &mut data),
                Row::Unusable => encode(0, 0, 65535, &mut data),
                Row::Normal(offset) => encode(1, *offset as u32, 0, &mut data),
                Row::Compressed(container, index) => encode(2, *container, *index, &mut data),
            }
        }
    }
    let prev_txt = prev.map(|p| format!(" /Prev {p}")).unwrap_or_default();
    pdf.extend_from_slice(
        format!(
            "{self_id} 0 obj\n<< /Type /XRef /Size {size} /Root 1 0 R /Info 4 0 R /Index [{}] \
             /W [1 4 2]{prev_txt} /Length {} >>\nstream\n",
            index.join(" "),
            data.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&data);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_start}\n%%EOF\n").as_bytes());
    xref_start
}

/// Append a classic cross-reference **table** revision. `runs` are `(starting_id, rows)`.
fn append_classic(pdf: &mut Vec<u8>, runs: &[(u32, Vec<Row>)], trailer_extra: &str) -> usize {
    let xref_start = pdf.len();
    pdf.extend_from_slice(b"xref\n");
    for (start, rows) in runs {
        pdf.extend_from_slice(format!("{start} {}\n", rows.len()).as_bytes());
        for row in rows {
            match row {
                Row::Free => pdf.extend_from_slice(b"0000000000 00001 f \n"),
                Row::Unusable => pdf.extend_from_slice(b"0000000000 65535 f \n"),
                Row::Normal(offset) => pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()),
                Row::Compressed(..) => unreachable!("a classic table cannot express a compressed entry"),
            }
        }
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Root 1 0 R /Info 4 0 R {trailer_extra} >>\nstartxref\n{xref_start}\n%%EOF\n").as_bytes(),
    );
    xref_start
}

/// The shared starting point: an xref-stream file where object 7 is compressed inside container 5.
/// Objects 1..=4 are normal, 5 is the `/ObjStm`, 6 is the cross-reference stream itself.
fn objstm_base(body: &str) -> (Vec<u8>, usize) {
    let (mut pdf, offsets) = basic_body("base");
    let objstm = push_objstm(&mut pdf, 5, 7, body);
    let xref_start = pdf.len();
    let rows = vec![
        Row::Free,
        Row::Normal(offsets[1]),
        Row::Normal(offsets[2]),
        Row::Normal(offsets[3]),
        Row::Normal(offsets[4]),
        Row::Normal(objstm),
        Row::Normal(xref_start), // the cross-reference stream describes itself
        Row::Compressed(5, 0),
    ];
    let start = append_xref_stream(&mut pdf, 6, &[(0, rows)], 8, None);
    assert_eq!(start, xref_start, "the self-row must name where the stream landed");
    (pdf, start)
}

// ------------------------------------------------------------------ the ten variants

/// V1: freed by a later cross-reference **stream** revision; the container stays live.
fn v1_xrefstream_free() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let xref_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        8,
        &[(7, vec![Row::Free]), (8, vec![Row::Normal(xref_start)])],
        9,
        Some(base),
    );
    pdf
}

/// V2: freed by a later classic **table** over an `/ObjStm` file — the mixed-encoding chain a
/// writer produces when it appends a plain section to a stream-indexed document.
fn v2_classic_free() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    append_classic(
        &mut pdf,
        &[(0, vec![Row::Free]), (7, vec![Row::Free])],
        &format!("/Size 8 /Prev {base}"),
    );
    pdf
}

/// V3: freed **and** container 5 superseded by a fresh `/ObjStm` that does not carry 7.
fn v3_container_superseded() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let new_container = push_objstm(&mut pdf, 5, 99, "<< /Filler true >>");
    let xref_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        8,
        &[
            (5, vec![Row::Normal(new_container)]),
            (7, vec![Row::Free]),
            (8, vec![Row::Normal(xref_start)]),
        ],
        9,
        Some(base),
    );
    pdf
}

/// V4: the container itself is freed alongside the member.
fn v4_container_freed() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let xref_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        8,
        &[
            (5, vec![Row::Free]),
            (7, vec![Row::Free]),
            (8, vec![Row::Normal(xref_start)]),
        ],
        9,
        Some(base),
    );
    pdf
}

/// V5: freed in an **intermediate** revision, with a third revision that says nothing about 7 —
/// the deletion has to survive one more merge to be seen.
fn v5_intermediate_free() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let mid_start = pdf.len();
    let mid = append_xref_stream(
        &mut pdf,
        8,
        &[(7, vec![Row::Free]), (8, vec![Row::Normal(mid_start)])],
        9,
        Some(base),
    );
    let new_info = push_object(&mut pdf, 4, b"<< /Title (third) >>");
    let last_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        9,
        &[(4, vec![Row::Normal(new_info)]), (9, vec![Row::Normal(last_start)])],
        10,
        Some(mid),
    );
    pdf
}

/// V6: freed, then the id is **reused** as a normal object by a third revision. The container
/// still carries the old body, so this pins that the reused definition wins.
fn v6_freed_then_reused() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let mid_start = pdf.len();
    let mid = append_xref_stream(
        &mut pdf,
        8,
        &[(7, vec![Row::Free]), (8, vec![Row::Normal(mid_start)])],
        9,
        Some(base),
    );
    let reused = push_object(&mut pdf, 7, b"<< /Reused true >>");
    let last_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        9,
        &[(7, vec![Row::Normal(reused)]), (9, vec![Row::Normal(last_start)])],
        10,
        Some(mid),
    );
    pdf
}

/// V7: the control — 7 is never freed, so both engines must still return it.
fn v7_control() -> Vec<u8> {
    objstm_base("<< /Doomed true >>").0
}

/// V8: the hybrid shape (classic table plus an `/XRefStm` supplement), with a third hybrid
/// revision freeing the compressed object through its classic section.
fn v8_hybrid_free() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("hybrid");
    let base = append_classic(
        &mut pdf,
        &[(
            0,
            vec![
                Row::Free,
                Row::Normal(offsets[1]),
                Row::Normal(offsets[2]),
                Row::Normal(offsets[3]),
                Row::Normal(offsets[4]),
            ],
        )],
        "/Size 5",
    );
    let objstm = push_objstm(&mut pdf, 5, 7, "<< /Hybrid true >>");
    let supplement_offset = pdf.len();
    let mut supplement = Vec::new();
    encode(2, 5, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 8 /Index [7 1] /W [1 4 2] /Length {} >>\nstream\n",
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let hybrid = append_classic(
        &mut pdf,
        &[(5, vec![Row::Normal(objstm), Row::Normal(supplement_offset)])],
        &format!("/Size 8 /Prev {base} /XRefStm {supplement_offset}"),
    );
    append_classic(
        &mut pdf,
        &[(0, vec![Row::Free]), (7, vec![Row::Free])],
        &format!("/Size 8 /Prev {hybrid}"),
    );
    pdf
}

/// V9: freed via a classic `0000000000 65535 f` row.
fn v9_unusable_free() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    append_classic(
        &mut pdf,
        &[(0, vec![Row::Free]), (7, vec![Row::Unusable])],
        &format!("/Size 8 /Prev {base}"),
    );
    pdf
}

/// V10: the same as V9 through a cross-reference stream type-0 row with generation 65535.
fn v10_unusable_free_stream() -> Vec<u8> {
    let (mut pdf, base) = objstm_base("<< /Doomed true >>");
    let xref_start = pdf.len();
    append_xref_stream(
        &mut pdf,
        8,
        &[(7, vec![Row::Unusable]), (8, vec![Row::Normal(xref_start)])],
        9,
        Some(base),
    );
    pdf
}

// ------------------------------------------------------------------ assertions

/// What the merged table records for object 7 — the premise of each row. Offsets are not
/// load-bearing here, so `Normal` deliberately carries none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    Free,
    UnusableFree,
    Normal,
    Compressed(u32, u16),
}

/// What the table therefore obliges *both* engines to answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// No live definition, so a reference into the slot reads as null (ISO 32000-1, 7.3.10).
    Deleted,
    /// A live definition, whose dictionary must carry exactly this key.
    Live(&'static str),
}

fn entry_of(document: &Document, id: u32) -> Option<Entry> {
    match document.reference_table.get(id) {
        None => None,
        Some(XrefEntry::Free) => Some(Entry::Free),
        Some(XrefEntry::UnusableFree) => Some(Entry::UnusableFree),
        Some(XrefEntry::Normal { .. }) => Some(Entry::Normal),
        Some(XrefEntry::Compressed { container, index }) => Some(Entry::Compressed(*container, *index)),
    }
}

/// Reduce a resolved object to the verdict it expresses, so the eager and the indexed answer are
/// compared on the same footing regardless of which produced it.
fn verdict_of(object: &Object) -> String {
    match object {
        Object::Null => "Deleted".into(),
        Object::Dictionary(dictionary) => {
            let keys: Vec<_> = dictionary
                .iter()
                .map(|(key, _)| String::from_utf8_lossy(key).into_owned())
                .collect();
            format!("Live{{{}}}", keys.join(","))
        }
        other => format!("unexpected({other:?})"),
    }
}

fn expected_verdict(verdict: Verdict) -> String {
    match verdict {
        Verdict::Deleted => "Deleted".into(),
        Verdict::Live(key) => format!("Live{{{key}}}"),
    }
}

/// Assert that the merged table is the premise this row claims, that both engines answer
/// `verdict`, and that the document is otherwise intact.
fn assert_agrees_with_the_xref(name: &str, pdf: &[u8], entry: Entry, verdict: Verdict) {
    let document = Document::load_mem(pdf).unwrap_or_else(|e| panic!("{name}: the fixture must load: {e}"));

    assert_eq!(
        entry_of(&document, 7),
        Some(entry),
        "{name}: the merged table is the premise of this row"
    );
    assert_eq!(
        document.get_pages().len(),
        1,
        "{name}: the deletion must not disturb the page tree"
    );

    let eager = document
        .get_object((7, 0))
        .cloned()
        .unwrap_or_else(|e| panic!("{name}: the eager lookup must not error: {e}"));
    let lazy = IndexedReader::open(BytesSource::from(pdf.to_vec()))
        .unwrap_or_else(|e| panic!("{name}: the indexed reader must open the fixture: {e}"))
        .resolve_object((7, 0))
        .unwrap_or_else(|e| panic!("{name}: indexed resolution must not error: {e}"));

    let expected = expected_verdict(verdict);
    assert_eq!(
        verdict_of(&eager),
        expected,
        "{name}: eager loading disagrees with the cross-reference table"
    );
    assert_eq!(
        verdict_of(&lazy),
        expected,
        "{name}: indexed resolution disagrees with the cross-reference table"
    );
}

/// Ten ways a file can talk about the compressed object 7 while container 5 remains on disk. In
/// every one of them the eager and the indexed engine must return the same thing, and that thing
/// must be what the merged cross-reference table says.
#[test]
fn every_way_of_deleting_or_moving_a_compressed_member_agrees_across_engines() {
    let variants: Vec<(&str, Vec<u8>, Entry, Verdict)> = vec![
        (
            "V1 freed by a later xref stream, container live",
            v1_xrefstream_free(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V2 freed by a later classic table over an ObjStm file",
            v2_classic_free(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V3 freed and the container superseded",
            v3_container_superseded(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V4 freed and the container freed too",
            v4_container_freed(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V5 freed in an intermediate revision",
            v5_intermediate_free(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V6 freed, then the id reused as a normal object",
            v6_freed_then_reused(),
            Entry::Normal,
            Verdict::Live("Reused"),
        ),
        (
            "V7 control: never freed",
            v7_control(),
            Entry::Compressed(5, 0),
            Verdict::Live("Doomed"),
        ),
        (
            "V8 hybrid, freed through the classic section",
            v8_hybrid_free(),
            Entry::Free,
            Verdict::Deleted,
        ),
        (
            "V9 freed by a classic 65535 f row",
            v9_unusable_free(),
            Entry::UnusableFree,
            Verdict::Deleted,
        ),
        (
            "V10 freed by an xref-stream type-0 row, generation 65535",
            v10_unusable_free_stream(),
            Entry::UnusableFree,
            Verdict::Deleted,
        ),
    ];

    assert_eq!(variants.len(), 10, "the matrix is the point; keep every shape in it");
    for (name, pdf, entry, verdict) in &variants {
        assert_agrees_with_the_xref(name, pdf, *entry, *verdict);
    }
}

/// Freeing the container as well as the member takes the container out too — V4 above only speaks
/// for the member, and a container that survived its own deletion would be a second leak.
#[test]
fn a_freed_container_is_itself_gone() {
    let pdf = v4_container_freed();
    let document = Document::load_mem(&pdf).unwrap();
    assert!(
        matches!(document.reference_table.get(5), Some(XrefEntry::Free)),
        "the fixture frees the container"
    );
    assert!(
        matches!(document.get_object((5, 0)), Ok(Object::Null) | Err(_)),
        "a freed container must not stay readable"
    );
}

/// The stale copy is not out-voted after the fact, it is never expanded: the freed id gets no
/// entry in `Document::objects` at all, and no dictionary anywhere in the loaded document still
/// carries the deleted body. `get_object` reporting `Null` would also be satisfied by an absent
/// key, so this is the stronger statement the matrix cannot make.
#[test]
fn a_freed_member_is_never_expanded_into_the_object_map() {
    for (name, pdf) in [
        ("V1 freed by a later xref stream", v1_xrefstream_free()),
        ("V6 freed, then the id reused", v6_freed_then_reused()),
    ] {
        let document = Document::load_mem(&pdf).unwrap();
        let doomed = document
            .objects
            .iter()
            .filter(|((id, _), _)| *id == 7)
            .any(|(_, object)| matches!(object, Object::Dictionary(d) if d.get(b"Doomed").is_ok()));
        assert!(!doomed, "{name}: the superseded member must not be loaded at all");
    }
}
