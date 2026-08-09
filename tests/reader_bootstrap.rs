use flate2::Compression;
use flate2::write::ZlibEncoder;
use log::{Level, LevelFilter, Metadata, Record};
use lopdf::xref::{XrefEntry, XrefType};
use lopdf::{
    BytesSource, DecompressError, Document, EncryptionState, EncryptionVersion, Error, IndexedReader, LoadOptions,
    Object, Permissions, StringFormat, dictionary,
};
use std::io::Write;
use std::sync::{Mutex, Once};

struct CaptureLogger;

static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;
static CAPTURED_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static INIT_LOGGER: Once = Once::new();

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            CAPTURED_WARNINGS.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Held for the duration of any test that reads [`CAPTURED_WARNINGS`]. The capture is one
/// process-wide buffer, so two such tests running side by side would each see the other's
/// output; taking this first makes each one's window its own.
static WARNING_CAPTURE: Mutex<()> = Mutex::new(());

/// Start capturing warnings and clear whatever a previous test left, returning the guard that
/// keeps this test's window to itself. Poison is ignored: an unrelated test's panic must not
/// turn every later warning assertion into a failure of its own.
fn capture_warnings() -> std::sync::MutexGuard<'static, ()> {
    INIT_LOGGER.call_once(|| {
        log::set_logger(&CAPTURE_LOGGER).unwrap();
        log::set_max_level(LevelFilter::Warn);
    });
    let guard = WARNING_CAPTURE.lock().unwrap_or_else(|error| error.into_inner());
    captured_warnings().clear();
    guard
}

/// The warnings logged since the last [`capture_warnings`] or [`clear`](Vec::clear).
fn captured_warnings() -> std::sync::MutexGuard<'static, Vec<String>> {
    CAPTURED_WARNINGS.lock().unwrap_or_else(|error| error.into_inner())
}

fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
    let offset = pdf.len();
    pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    offset
}

fn basic_body(title: &str) -> (Vec<u8>, Vec<usize>) {
    basic_body_with_second_line(title, b"")
}

fn basic_body_with_second_line(title: &str, second_line: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    pdf.extend_from_slice(second_line);
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

/// The offset the file's last `startxref` names, i.e. the section a further revision appended
/// to it has to point its `/Prev` at.
fn last_startxref(pdf: &[u8]) -> usize {
    let text = String::from_utf8_lossy(pdf);
    let (_, tail) = text.rsplit_once("startxref").expect("a fixture always has one");
    tail.split_whitespace()
        .next()
        .expect("startxref is followed by its offset")
        .parse()
        .expect("…which is a number")
}

fn classic_pdf(size: u32) -> (Vec<u8>, usize) {
    let (mut pdf, offsets) = basic_body("classic");
    let entries = offsets.into_iter().map(Some).collect();
    let xref_start = append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        format!("<< /Size {size} /Root 1 0 R /Info 4 0 R /Revision (classic) >>")
    });
    (pdf, xref_start)
}

fn encode_xref_entry(entry_type: u8, field_2: u32, field_3: u16, output: &mut Vec<u8>) {
    output.push(entry_type);
    output.extend_from_slice(&field_2.to_be_bytes());
    output.extend_from_slice(&field_3.to_be_bytes());
}

fn finish_xref_stream_pdf(mut pdf: Vec<u8>, offsets: &[usize], compressed: bool) -> Vec<u8> {
    let xref_start = pdf.len();
    let mut decoded = Vec::new();
    encode_xref_entry(0, 0, 65535, &mut decoded);
    for offset in offsets.iter().skip(1) {
        encode_xref_entry(1, *offset as u32, 0, &mut decoded);
    }
    encode_xref_entry(1, xref_start as u32, 0, &mut decoded);

    let (stored, filter) = if compressed {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        (encoder.finish().unwrap(), " /Filter /FlateDecode")
    } else {
        (decoded, "")
    };

    pdf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XRef /Size 6 /Root 1 0 R /Info 4 0 R \
             /Revision (xref-stream) /W [1 4 2] /Length {}{} >>\nstream\n",
            stored.len(),
            filter
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&stored);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_start}\n%%EOF\n").as_bytes());
    pdf
}

fn xref_stream_pdf(compressed: bool) -> Vec<u8> {
    let (pdf, offsets) = basic_body("xref-stream");
    finish_xref_stream_pdf(pdf, &offsets, compressed)
}

fn incremental_pdf() -> (Vec<u8>, usize) {
    let (mut pdf, base_offsets) = basic_body("old");
    let base_entries = base_offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(&mut pdf, vec![(0, base_entries)], |_| {
        "<< /Size 5 /Root 1 0 R /Info 4 0 R /Revision (old) >>".to_string()
    });

    let new_info = push_object(&mut pdf, 4, b"<< /Title (newest) >>");
    let new_marker = push_object(&mut pdf, 5, b"<< /RevisionObject (newest) >>");
    append_classic_revision(&mut pdf, vec![(4, vec![Some(new_info), Some(new_marker)])], |_| {
        format!(
            "<< /Size 6 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /Revision (newest) >>"
        )
    });
    (pdf, new_info)
}

fn hybrid_incremental_pdf() -> Vec<u8> {
    hybrid_incremental_pdf_declaring(|supplement| supplement)
}

/// [`hybrid_incremental_pdf`], with `declared` choosing the offset the trailer's `/XRefStm`
/// actually names — so a test can point it somewhere the supplement is not.
fn hybrid_incremental_pdf_declaring<F: FnOnce(usize) -> usize>(declared: F) -> Vec<u8> {
    let (mut pdf, base_offsets) = basic_body("hybrid");
    let base_entries = base_offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(&mut pdf, vec![(0, base_entries)], |_| {
        "<< /Size 5 /Root 1 0 R /Info 4 0 R /Revision (old) >>".to_string()
    });

    let member = b"7 0 << /Hybrid true >>";
    let object_stream = format!("<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n", member.len());
    let object_stream_offset = pdf.len();
    pdf.extend_from_slice(b"5 0 obj\n");
    pdf.extend_from_slice(object_stream.as_bytes());
    pdf.extend_from_slice(member);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_stream_offset = pdf.len();
    let mut supplement = Vec::new();
    encode_xref_entry(2, 5, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 8 /Index [7 1] /W [1 4 2] /Length {} >>\nstream\n",
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let declared_supplement = declared(xref_stream_offset);
    append_classic_revision(
        &mut pdf,
        vec![(5, vec![Some(object_stream_offset), Some(xref_stream_offset)])],
        |_| {
            format!(
                "<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /XRefStm {declared_supplement} /Revision (hybrid) >>"
            )
        },
    );
    pdf
}

/// The canonical hybrid layout of ISO 32000-1, 7.5.8.4, written the way a `/Index` run forces:
/// the classic section lists objects 1–6 and masks the compressed object 7 as free, while the
/// supplement's run is contiguous — `/Index [0 8]` — so it describes object 7 and **pads
/// every other number with a type-0 row**, including the six the classic section defines.
///
/// Both directions of the rule live in this one file. The supplement's definition of object 7
/// must lift the classic mask; its padding must not erase the catalog, the page tree, or the
/// object stream that holds object 7.
fn hybrid_padded_supplement_pdf() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("padded");

    let member = b"7 0 << /Hybrid true >>";
    let object_stream = format!("<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n", member.len());
    let object_stream_offset = pdf.len();
    pdf.extend_from_slice(b"5 0 obj\n");
    pdf.extend_from_slice(object_stream.as_bytes());
    pdf.extend_from_slice(member);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_stream_offset = pdf.len();
    let mut supplement = Vec::new();
    for _ in 0..7 {
        encode_xref_entry(0, 0, 0, &mut supplement);
    }
    encode_xref_entry(2, 5, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 8 /Index [0 8] /W [1 4 2] /Length {} >>\nstream\n",
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let mut entries: Vec<Option<usize>> = offsets.into_iter().map(Some).collect();
    entries[0] = None;
    entries.extend([Some(object_stream_offset), Some(xref_stream_offset), None]);
    append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        format!("<< /Size 8 /Root 1 0 R /Info 4 0 R /XRefStm {xref_stream_offset} /Revision (padded) >>")
    });
    pdf
}

fn xrefstm_without_prev_pdf() -> (Vec<u8>, usize) {
    let (mut pdf, offsets) = basic_body("no-prev");

    let member = b"7 0 << /Supplement true >>";
    let object_stream = format!("<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n", member.len());
    let object_stream_offset = pdf.len();
    pdf.extend_from_slice(b"5 0 obj\n");
    pdf.extend_from_slice(object_stream.as_bytes());
    pdf.extend_from_slice(member);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_stream_offset = pdf.len();
    let mut supplement = Vec::new();
    encode_xref_entry(2, 5, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 8 /Index [7 1] /W [1 4 2] /Length {} >>\nstream\n",
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let mut base_entries: Vec<_> = offsets.into_iter().map(Some).collect();
    base_entries.extend([Some(object_stream_offset), Some(xref_stream_offset)]);
    append_classic_revision(&mut pdf, vec![(0, base_entries)], |_| {
        format!(
            "<< /Size 8 /Root 1 0 R /Info 4 0 R /XRefStm {xref_stream_offset} \
             /Revision (no-prev) >>"
        )
    });
    (pdf, xref_stream_offset)
}

fn hybrid_collision_pdf() -> (Vec<u8>, usize) {
    let (mut pdf, offsets) = basic_body("collision");
    let previous_object_offset = push_object(&mut pdf, 7, b"<< /Winner (previous-table) >>");
    let base_entries = offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(
        &mut pdf,
        vec![(0, base_entries), (7, vec![Some(previous_object_offset)])],
        |_| "<< /Size 8 /Root 1 0 R /Info 4 0 R /Revision (previous) >>".to_string(),
    );

    let member = b"7 0 << /Winner (supplement) >>";
    let object_stream = format!("<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n", member.len());
    let object_stream_offset = pdf.len();
    pdf.extend_from_slice(b"8 0 obj\n");
    pdf.extend_from_slice(object_stream.as_bytes());
    pdf.extend_from_slice(member);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_stream_offset = pdf.len();
    let mut supplement = Vec::new();
    encode_xref_entry(2, 8, 0, &mut supplement);
    pdf.extend_from_slice(
        format!(
            "9 0 obj\n<< /Type /XRef /Size 10 /Index [7 1] /W [1 4 2] /Length {} >>\nstream\n",
            supplement.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&supplement);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    append_classic_revision(
        &mut pdf,
        vec![(8, vec![Some(object_stream_offset), Some(xref_stream_offset)])],
        |_| {
            format!(
                "<< /Size 10 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /XRefStm {xref_stream_offset} /Revision (newest) >>"
            )
        },
    );
    (pdf, previous_object_offset)
}

fn eager_title(document: &Document) -> String {
    let info_id = document.trailer.get(b"Info").unwrap().as_reference().unwrap();
    let title = document
        .get_object(info_id)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Title")
        .unwrap()
        .as_str()
        .unwrap();
    String::from_utf8_lossy(title).into_owned()
}

fn trailer_text(document: &Document, key: &[u8]) -> String {
    String::from_utf8_lossy(document.trailer.get(key).unwrap().as_str().unwrap()).into_owned()
}

fn assert_shared_fingerprint(pdf: &[u8], title: &str, max_id: u32, xref_stream: bool) -> Document {
    assert_shared_fingerprint_with_size(pdf, title, max_id, max_id + 1, xref_stream)
}

/// `size` is the id space the file declares and `max_id` the highest object it still defines.
/// They differ by exactly one thing: a revision that *deletes* its highest-numbered object
/// keeps `/Size` where it was (ISO 32000-1, 7.5.4 counts free entries) while there is one
/// less object to number from.
fn assert_shared_fingerprint_with_size(pdf: &[u8], title: &str, max_id: u32, size: u32, xref_stream: bool) -> Document {
    let eager = Document::load_mem(pdf).unwrap();
    let metadata = Document::load_metadata_mem(pdf).unwrap();

    assert_eq!(metadata.version, eager.version);
    assert_eq!(metadata.page_count, eager.get_pages().len() as u32);
    assert_eq!(metadata.title.as_deref(), Some(title));
    assert_eq!(eager_title(&eager), title);
    assert_eq!(eager.max_id, max_id);
    assert_eq!(eager.reference_table.size, size);
    assert_eq!(
        matches!(
            eager.reference_table.cross_reference_type,
            XrefType::CrossReferenceStream
        ),
        xref_stream
    );
    eager
}

#[test]
fn classic_and_xref_stream_bootstrap_match_metadata_and_eager_paths() {
    let (classic, _) = classic_pdf(5);
    let classic_doc = assert_shared_fingerprint(&classic, "classic", 4, false);
    assert_eq!(trailer_text(&classic_doc, b"Revision"), "classic");

    let stream = xref_stream_pdf(false);
    let stream_doc = assert_shared_fingerprint(&stream, "xref-stream", 5, true);
    assert_eq!(trailer_text(&stream_doc, b"Revision"), "xref-stream");
}

#[test]
fn incremental_merge_keeps_newest_object_and_trailer() {
    let (pdf, newest_info_offset) = incremental_pdf();
    let eager = assert_shared_fingerprint(&pdf, "newest", 5, false);

    assert_eq!(trailer_text(&eager, b"Revision"), "newest");
    assert!(eager.trailer.get(b"Prev").is_err(), "Prev is consumed during bootstrap");
    assert!(matches!(
        eager.reference_table.get(4),
        Some(XrefEntry::Normal { offset, generation: 0 }) if *offset == newest_info_offset as u32
    ));
}

#[test]
fn hybrid_supplement_is_merged_with_its_own_section() {
    let pdf = hybrid_incremental_pdf();
    let eager = assert_shared_fingerprint(&pdf, "hybrid", 7, false);

    assert_eq!(trailer_text(&eager, b"Revision"), "hybrid");
    assert!(
        eager.trailer.get(b"XRefStm").is_err(),
        "XRefStm is consumed with the section that declares it"
    );
    assert!(matches!(
        eager.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 5, index: 0 })
    ));
    assert!(
        eager
            .get_object((7, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Hybrid")
            .unwrap()
            .as_bool()
            .unwrap()
    );
}

/// Both halves of 7.5.8.4 on the file that states them at once: the supplement's **definition**
/// of the compressed object lifts the mask the classic section is required to write for legacy
/// readers, and the type-0 rows the supplement's contiguous `/Index` forces it to emit for
/// every other number are padding that must not erase the classic section's live entries.
#[test]
fn a_supplements_padding_free_rows_do_not_erase_the_classic_section() {
    let pdf = hybrid_padded_supplement_pdf();
    let eager = assert_shared_fingerprint(&pdf, "padded", 7, false);

    // The mask-lift direction: the classic section says object 7 is free, the supplement says
    // where it lives, and the supplement wins.
    assert!(matches!(
        eager.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 5, index: 0 })
    ));
    assert!(
        eager
            .get_object((7, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Hybrid")
            .unwrap()
            .as_bool()
            .unwrap()
    );

    // The padding direction: every object the classic section defines survives the overlay —
    // catalog, page tree, page, info, and the object stream object 7 lives in.
    for id in 1..=6 {
        assert!(
            matches!(eager.reference_table.get(id), Some(XrefEntry::Normal { .. })),
            "object {id} was erased by a padding row: {:?}",
            eager.reference_table.get(id)
        );
        assert!(eager.has_object((id, 0)), "object {id} was not loaded");
    }
    assert_eq!(eager.get_pages().len(), 1);

    // The indexed reader overlays the same supplement and must reach the same table.
    assert!(indexed_resolves(&pdf, 7));
    for id in 1..=6 {
        assert!(
            !matches!(indexed_object(&pdf, id), Object::Null),
            "the indexed reader lost object {id}"
        );
    }
}

/// The other direction of "a free entry masks": a hybrid revision records what it *deletes* in
/// its classic section — the part a legacy reader is guaranteed to read — and that entry still
/// masks the older revision's definition. Dropping the supplement's type-0 rows must not touch
/// this path.
#[test]
fn a_hybrid_revision_still_deletes_through_its_classic_section() {
    let mut pdf = hybrid_incremental_pdf();
    // A third revision, hybrid like the second, that frees the compressed object 7 and points
    // at the same supplement — whose type-0 padding row for 7 is now agreement, not news.
    let previous_xref = last_startxref(&pdf);
    let delete_xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 1\n0000000007 65535 f \n7 1\n0000000000 00001 f \n");
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {previous_xref} /Revision (deleted) >>\
             \nstartxref\n{delete_xref}\n%%EOF\n"
        )
        .as_bytes(),
    );

    let document = Document::load_mem(&pdf).unwrap();
    assert!(matches!(document.reference_table.get(7), Some(XrefEntry::Free)));
    assert_eq!(document.get_pages().len(), 1);
    // Read the way the table says: the slot is free, so the reference into it is null.
    // (The eager loader additionally keeps whatever it expanded out of the object stream it
    // could still see, which is why this reads the xref rather than `Document::objects`.)
    assert!(matches!(indexed_object(&pdf, 7), Object::Null));
}

#[test]
fn xrefstm_of_the_only_section_is_consumed_and_merged() {
    let (pdf, _) = xrefstm_without_prev_pdf();
    let eager = assert_shared_fingerprint(&pdf, "no-prev", 7, false);

    assert_eq!(trailer_text(&eager, b"Revision"), "no-prev");
    assert!(
        eager.trailer.get(b"XRefStm").is_err(),
        "the supplement of the newest section is consumed even without a Prev"
    );
    assert!(
        matches!(
            eager.reference_table.get(7),
            Some(XrefEntry::Compressed { container: 5, index: 0 })
        ),
        "a section's own supplement is read whether or not the chain continues"
    );
    assert!(
        eager
            .get_object((7, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Supplement")
            .unwrap()
            .as_bool()
            .unwrap()
    );
}

#[test]
fn hybrid_supplement_wins_collision_with_previous_table() {
    let (pdf, _) = hybrid_collision_pdf();
    let eager = assert_shared_fingerprint(&pdf, "collision", 9, false);

    assert_eq!(trailer_text(&eager, b"Revision"), "newest");
    // ISO 32000-1, 7.5.8.4: the newest section's supplement describes that
    // revision, so it supersedes an older section's entry for the same object.
    assert!(matches!(
        eager.reference_table.get(7),
        Some(XrefEntry::Compressed { container: 8, index: 0 })
    ));
    let winner = eager
        .get_object((7, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Winner")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(winner, b"supplement");
}

#[test]
fn leading_garbage_is_rebased_before_shared_bootstrap() {
    let (pdf, xref_start) = classic_pdf(5);
    let mut prefixed = b"ignored prefix bytes\n".to_vec();
    prefixed.extend_from_slice(&pdf);

    let eager = assert_shared_fingerprint(&prefixed, "classic", 4, false);
    assert_eq!(
        eager.xref_start, xref_start,
        "xref offset remains relative to the PDF header"
    );
}

#[test]
fn binary_mark_remains_an_eager_document_call_site_detail() {
    let (mut pdf, offsets) = basic_body_with_second_line("binary", b"%\xBB\xAD\xC0\xDE\n");
    let entries = offsets.into_iter().map(Some).collect();
    append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        "<< /Size 5 /Root 1 0 R /Info 4 0 R >>".to_string()
    });

    let eager = Document::load_mem(&pdf).unwrap();
    assert_eq!(eager.binary_mark, vec![0xBB, 0xAD, 0xC0, 0xDE]);
    let metadata = Document::load_metadata_mem(&pdf).unwrap();
    assert_eq!(metadata.title.as_deref(), Some("binary"));
}

#[test]
fn repeated_prev_offset_still_breaks_the_cycle_silently() {
    let (mut pdf, offsets) = basic_body("cycle");
    let entries = offsets.into_iter().map(Some).collect();
    let xref_start = append_classic_revision(&mut pdf, vec![(0, entries)], |self_offset| {
        format!(
            "<< /Size 5 /Root 1 0 R /Info 4 0 R /Prev {self_offset} \
             /Revision (cycle) >>"
        )
    });

    let eager = assert_shared_fingerprint(&pdf, "cycle", 4, false);
    assert_eq!(eager.xref_start, xref_start);
    assert!(eager.trailer.get(b"Prev").is_err());
}

fn pdf_with_incremental_trailer(prev: &str, extra: &str) -> Vec<u8> {
    let (mut pdf, offsets) = basic_body("old");
    let entries = offsets.into_iter().map(Some).collect();
    let base_xref = append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        "<< /Size 5 /Root 1 0 R /Info 4 0 R >>".to_string()
    });
    let info_offset = push_object(&mut pdf, 4, b"<< /Title (new) >>");
    append_classic_revision(&mut pdf, vec![(4, vec![Some(info_offset)])], |_| {
        let prev = if prev == "base" {
            base_xref.to_string()
        } else {
            prev.to_string()
        };
        format!("<< /Size 5 /Root 1 0 R /Info 4 0 R /Prev {prev} {extra} >>")
    });
    pdf
}

fn assert_same_bootstrap_error(pdf: &[u8], expected: &str) {
    let eager = format!("{:?}", Document::load_mem(pdf).unwrap_err());
    let metadata = format!("{:?}", Document::load_metadata_mem(pdf).unwrap_err());
    assert_eq!(eager, metadata);
    assert!(eager.contains(expected), "expected {expected:?} in {eager:?}");
}

#[test]
fn prev_bounds_errors_match_between_call_sites() {
    let bad_prev = pdf_with_incremental_trailer("-1", "");
    assert_same_bootstrap_error(&bad_prev, "Prev");

    let bad_prev_high = pdf_with_incremental_trailer("99999999", "");
    assert_same_bootstrap_error(&bad_prev_high, "Prev");
}

/// A `/Prev` that does not resolve costs the document a whole revision, so it is fatal. A
/// `/XRefStm` that does not resolve costs it the *compressed objects of one revision*, and the
/// classic section beside it is a complete cross-reference table by construction — a hybrid
/// file is designed to be read by readers that never look at `/XRefStm` at all. So a damaged
/// supplement is skipped with a warning and the document still opens, at both call sites.
#[test]
fn a_damaged_supplement_degrades_to_its_classic_section() {
    let _capture = capture_warnings();

    let out_of_bounds = pdf_with_incremental_trailer("base", "/XRefStm 99999999");
    let negative = pdf_with_incremental_trailer("base", "/XRefStm -1");
    // An offset inside the file that is not a cross-reference section at all: the overwritten
    // bytes an interrupted incremental update leaves behind.
    let garbage = pdf_with_incremental_trailer("base", "/XRefStm 9");

    for (pdf, label) in [
        (out_of_bounds, "out of bounds"),
        (negative, "negative"),
        (garbage, "garbage"),
    ] {
        captured_warnings().clear();
        let eager = Document::load_mem(&pdf).unwrap_or_else(|error| panic!("{label} /XRefStm: {error:?}"));
        let metadata = Document::load_metadata_mem(&pdf).unwrap_or_else(|error| panic!("{label} /XRefStm: {error:?}"));

        assert_eq!(eager_title(&eager), "new", "{label}");
        assert_eq!(eager.get_pages().len(), 1, "{label}");
        assert_eq!(metadata.page_count, 1, "{label}");
        assert!(eager.trailer.get(b"XRefStm").is_err(), "{label}");

        let warnings = captured_warnings().clone();
        let mentions = warnings.iter().filter(|message| message.contains("XRefStm")).count();
        assert_eq!(mentions, 2, "{label}: both call sites warn once: {warnings:?}");
    }
}

/// A supplement pointed at bytes that are not its own still loses only the compressed objects
/// it described: everything the classic section defines is untouched.
#[test]
fn a_damaged_supplement_keeps_the_rest_of_the_hybrid_file() {
    // Warns about `/XRefStm` like its sibling above, so it takes the same gate: the two must
    // not count each other's warnings.
    let _capture = capture_warnings();
    let pdf = hybrid_incremental_pdf_declaring(|supplement| supplement + 12);
    let eager = Document::load_mem(&pdf).unwrap();

    assert_eq!(eager_title(&eager), "hybrid");
    assert_eq!(eager.get_pages().len(), 1);
    for id in 1..=6 {
        assert!(eager.reference_table.get(id).is_some(), "the classic section lost {id}");
    }
    // Only the supplement's own contribution is gone: object 7 was described nowhere else, so
    // the merged table has no row for it at all.
    assert!(eager.reference_table.get(7).is_none());
}

#[test]
fn declared_size_is_normalized_from_the_merged_entries() {
    let _capture = capture_warnings();

    let (pdf, _) = classic_pdf(99);
    let eager = assert_shared_fingerprint(&pdf, "classic", 4, false);
    assert_eq!(eager.reference_table.size, 5);

    let expected = "Size entry of trailer dictionary is 99, correct value is 5.";
    let count = captured_warnings()
        .iter()
        .filter(|message| message.as_str() == expected)
        .count();
    assert_eq!(count, 2, "eager and metadata bootstrap should emit the same warning");
}

#[test]
fn xref_stream_decompression_limit_remains_a_full_load_call_site_option() {
    let pdf = xref_stream_pdf(true);
    let error = Document::load_mem_with_options(&pdf, LoadOptions::with_max_decompressed_size(8)).unwrap_err();
    assert!(matches!(
        error,
        Error::Decompress(DecompressError::MemoryLimitExceeded { limit: 8 })
    ));

    let metadata = Document::load_metadata_mem(&pdf).unwrap();
    assert_eq!(metadata.title.as_deref(), Some("xref-stream"));
    assert_eq!(metadata.page_count, 1);
}

fn encrypted_pdf() -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference((3, 0))],
            "Count" => 1,
        }),
    );
    document.objects.insert(
        (3, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference((2, 0)),
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        }),
    );
    document.objects.insert(
        (4, 0),
        Object::Dictionary(dictionary! {
            "Title" => Object::String(b"encrypted".to_vec(), StringFormat::Literal),
        }),
    );
    document.max_id = 4;
    document.trailer.set("Root", Object::Reference((1, 0)));
    document.trailer.set("Info", Object::Reference((4, 0)));
    document.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(vec![1; 16], StringFormat::Hexadecimal),
            Object::String(vec![2; 16], StringFormat::Hexadecimal),
        ]),
    );

    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &document,
        owner_password: "owner",
        user_password: "user",
        key_length: 128,
        permissions: Permissions::all(),
    })
    .unwrap();
    document.encrypt(&state).unwrap();

    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

#[test]
fn encryption_behavior_remains_owned_by_each_public_call_site() {
    let pdf = encrypted_pdf();

    let eager_without_password = Document::load_mem(&pdf).unwrap();
    assert!(eager_without_password.trailer.has(b"Encrypt"));

    let metadata_without_password = Document::load_metadata_mem(&pdf).unwrap();
    assert!(metadata_without_password.encrypted);
    assert_eq!(metadata_without_password.page_count, 0);
    assert_eq!(metadata_without_password.title, None);

    let eager = Document::load_mem_with_options(&pdf, LoadOptions::with_password("user")).unwrap();
    let metadata = Document::load_metadata_mem_with_password(&pdf, "user").unwrap();
    assert_eq!(metadata.page_count, eager.get_pages().len() as u32);
    assert_eq!(metadata.title.as_deref(), Some("encrypted"));
    assert!(!eager.trailer.has(b"Encrypt"));
}

/// Three revisions around one object: revision 1 defines object 5, revision 2 **frees** it,
/// revision 3 defines it again. `revisions` picks how many are written, so the same body reads
/// as live / deleted / redefined.
///
/// Freeing is how a PDF deletes: a redaction or a form flatten drops the object and leaves the
/// reference to it dangling, which ISO 32000-1 7.3.10 makes a reference to null. A reader that
/// discards free entries never sees the deletion and resurrects the object from the older
/// section instead.
fn deleted_object_pdf(revisions: usize) -> (Vec<u8>, usize) {
    assert!((1..=3).contains(&revisions));
    let (mut pdf, base_offsets) = basic_body("deletion");
    let doomed = push_object(&mut pdf, 5, b"<< /RevisionObject (doomed) >>");
    let mut entries: Vec<Option<usize>> = base_offsets.into_iter().map(Some).collect();
    entries.push(Some(doomed));
    let base_xref = append_classic_revision(&mut pdf, vec![(0, entries)], |_| {
        "<< /Size 6 /Root 1 0 R /Info 4 0 R /Revision (base) >>".to_string()
    });
    if revisions == 1 {
        return (pdf, base_xref);
    }

    // Revision 2 deletes object 5: the free-list head points at it, and its own entry links
    // back to the head carrying the generation a reuse would take. Written by hand because it
    // is the *non*-65535 free flavour, which `append_classic_revision` cannot spell.
    let delete_xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 1\n0000000005 65535 f \n5 1\n0000000000 00001 f \n");
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size 6 /Root 1 0 R /Info 4 0 R /Prev {base_xref} /Revision (deleted) >>\
             \nstartxref\n{delete_xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    if revisions == 2 {
        return (pdf, delete_xref);
    }

    let revived = push_object(&mut pdf, 5, b"<< /RevisionObject (revived) >>");
    let revive_xref = append_classic_revision(&mut pdf, vec![(0, vec![None]), (5, vec![Some(revived)])], |_| {
        format!("<< /Size 6 /Root 1 0 R /Info 4 0 R /Prev {delete_xref} /Revision (revived) >>")
    });
    (pdf, revive_xref)
}

fn revision_object(document: &Document, id: u32) -> Option<String> {
    let object = document.get_object((id, 0)).ok()?;
    let name = object.as_dict().ok()?.get(b"RevisionObject").ok()?.as_str().ok()?;
    Some(String::from_utf8_lossy(name).into_owned())
}

fn indexed_object(pdf: &[u8], id: u32) -> Object {
    IndexedReader::open(BytesSource::from(pdf.to_vec()))
        .unwrap()
        .resolve_object((id, 0))
        .unwrap()
}

fn indexed_resolves(pdf: &[u8], id: u32) -> bool {
    matches!(indexed_object(pdf, id), Object::Dictionary(_))
}

#[test]
fn a_newer_revision_free_entry_masks_the_older_definition() {
    let (live, _) = deleted_object_pdf(1);
    let (deleted, _) = deleted_object_pdf(2);

    let live_doc = Document::load_mem(&live).unwrap();
    assert_eq!(revision_object(&live_doc, 5).as_deref(), Some("doomed"));
    assert!(indexed_resolves(&live, 5));

    let deleted_doc = Document::load_mem(&deleted).unwrap();
    // The deletion is recorded rather than dropped, so the base section's `Normal` entry for
    // object 5 no longer wins the merge and nothing resurrects it.
    assert!(matches!(deleted_doc.reference_table.get(5), Some(XrefEntry::Free)));
    assert_eq!(revision_object(&deleted_doc, 5), None);
    assert!(!deleted_doc.has_object((5, 0)));
    // Both readers agree the object is gone — this is the last xref-layer disagreement
    // between them.
    assert!(!indexed_resolves(&deleted, 5));
}

/// A dictionary that still points at an object a later revision freed is ordinary
/// incremental-writer output — a redaction drops the object and leaves the `/Annots` entry
/// naming it. ISO 32000-1, 7.3.10 makes that reference the **null object**, not an error, so
/// a caller walking the array keeps working instead of failing an extraction that used to
/// succeed. An id the file never mentions is a different thing and stays an error.
#[test]
fn a_reference_into_a_freed_slot_reads_as_null() {
    let (deleted, _) = deleted_object_pdf(2);
    let document = Document::load_mem(&deleted).unwrap();

    assert!(matches!(document.get_object((5, 0)), Ok(Object::Null)));
    let (id, resolved) = document.dereference(&Object::Reference((5, 0))).unwrap();
    assert_eq!(id, Some((5, 0)));
    assert!(matches!(resolved, Object::Null));

    // Reached the way a document reaches it: through an array the freed object is still
    // named in. The array resolves, with a null where the deletion happened.
    let annots = Object::Array(vec![Object::Reference((4, 0)), Object::Reference((5, 0))]);
    let resolved: Vec<&Object> = annots
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| document.dereference(entry).unwrap().1)
        .collect();
    assert!(matches!(resolved[0], Object::Dictionary(_)));
    assert!(matches!(resolved[1], Object::Null));

    // Both engines answer the same. The indexed reader keeps its typed refusal for an id the
    // index does not mention at all, which is a file disagreeing with itself rather than a
    // deletion.
    assert!(matches!(indexed_object(&deleted, 5), Object::Null));
    assert!(matches!(
        document.get_object((99, 0)),
        Err(Error::ObjectNotFound((99, 0)))
    ));
    assert!(
        IndexedReader::open(BytesSource::from(deleted.clone()))
            .unwrap()
            .resolve_object((99, 0))
            .is_err()
    );
}

/// A file that frees its highest-numbered object declares the same `/Size` it did before:
/// 7.5.4 counts free entries, and the table still holds a row for the freed slot. The reader
/// used to derive the size from definitions alone, so it warned about — and shrank — a `/Size`
/// that was right as written.
#[test]
fn freeing_the_highest_object_leaves_the_declared_size_alone() {
    let _capture = capture_warnings();

    let (deleted, _) = deleted_object_pdf(2);
    let document = Document::load_mem(&deleted).unwrap();
    let metadata = Document::load_metadata_mem(&deleted).unwrap();
    assert_eq!(metadata.page_count, 1);

    assert_eq!(document.reference_table.size, 6, "the trailer's /Size 6 is correct");
    assert_eq!(document.max_id, 4, "…and the highest *definition* is still 4");

    let size_warnings: Vec<String> = captured_warnings()
        .iter()
        .filter(|message| message.contains("Size entry of trailer dictionary"))
        .cloned()
        .collect();
    assert!(size_warnings.is_empty(), "unexpected warnings: {size_warnings:?}");
}

#[test]
fn an_older_revision_free_entry_does_not_delete_a_newer_definition() {
    let (revived, _) = deleted_object_pdf(3);

    let document = Document::load_mem(&revived).unwrap();
    // Sections merge newest-first and the newest wins, so a free entry only ever masks what is
    // *older* than it. Recording free entries must not invert that.
    assert!(matches!(
        document.reference_table.get(5),
        Some(XrefEntry::Normal { generation: 0, .. })
    ));
    assert_eq!(revision_object(&document, 5).as_deref(), Some("revived"));
    assert!(indexed_resolves(&revived, 5));
}

#[test]
fn a_deleted_object_does_not_move_the_id_a_save_numbers_from() {
    let (deleted, _) = deleted_object_pdf(2);
    let mut document = assert_shared_fingerprint_with_size(&deleted, "deletion", 4, 6, false);

    // `Document::max_id` counts *definitions*: the deletion must leave the id space at 4 so
    // the save path keeps numbering new objects from 5 rather than stepping over a slot
    // nothing occupies. `/Size` is the other question and has the other answer — the file
    // declares 6, which is correct as written (7.5.4 counts the free entry), so the reader
    // leaves it alone.
    assert_eq!(document.max_id, 4);
    assert_eq!(document.reference_table.size, 6);
    assert_eq!(
        document.add_object(dictionary! { "RevisionObject" => Object::string_literal("added") }),
        (5, 0)
    );

    let mut saved = Vec::new();
    document.save_to(&mut saved).unwrap();

    // A full save writes a fresh table from the objects it holds, so the deletion does not
    // survive it — but neither does the object that was deleted.
    let reloaded = Document::load_mem(&saved).unwrap();
    assert_eq!(reloaded.max_id, 5);
    assert_eq!(reloaded.reference_table.size, 6);
    assert_eq!(revision_object(&reloaded, 5).as_deref(), Some("added"));
    assert_eq!(reloaded.get_pages().len(), 1);
    assert_eq!(eager_title(&reloaded), "deletion");
}
