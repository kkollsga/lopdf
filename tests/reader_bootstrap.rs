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

fn capture_warnings() {
    INIT_LOGGER.call_once(|| {
        log::set_logger(&CAPTURE_LOGGER).unwrap();
        log::set_max_level(LevelFilter::Warn);
    });
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

    append_classic_revision(
        &mut pdf,
        vec![(5, vec![Some(object_stream_offset), Some(xref_stream_offset)])],
        |_| {
            format!(
                "<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} \
                 /XRefStm {xref_stream_offset} /Revision (hybrid) >>"
            )
        },
    );
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
    let eager = Document::load_mem(pdf).unwrap();
    let metadata = Document::load_metadata_mem(pdf).unwrap();

    assert_eq!(metadata.version, eager.version);
    assert_eq!(metadata.page_count, eager.get_pages().len() as u32);
    assert_eq!(metadata.title.as_deref(), Some(title));
    assert_eq!(eager_title(&eager), title);
    assert_eq!(eager.max_id, max_id);
    assert_eq!(eager.reference_table.size, max_id + 1);
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
fn prev_and_xrefstm_bounds_errors_match_between_call_sites() {
    let bad_prev = pdf_with_incremental_trailer("-1", "");
    assert_same_bootstrap_error(&bad_prev, "Prev");

    let bad_prev_high = pdf_with_incremental_trailer("99999999", "");
    assert_same_bootstrap_error(&bad_prev_high, "Prev");

    let bad_xref_stream = pdf_with_incremental_trailer("base", "/XRefStm -1");
    assert_same_bootstrap_error(&bad_xref_stream, "StreamStart");
}

#[test]
fn declared_size_is_normalized_from_the_merged_entries() {
    capture_warnings();
    CAPTURED_WARNINGS.lock().unwrap().clear();

    let (pdf, _) = classic_pdf(99);
    let eager = assert_shared_fingerprint(&pdf, "classic", 4, false);
    assert_eq!(eager.reference_table.size, 5);

    let expected = "Size entry of trailer dictionary is 99, correct value is 5.";
    let count = CAPTURED_WARNINGS
        .lock()
        .unwrap()
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

fn indexed_resolves(pdf: &[u8], id: u32) -> bool {
    IndexedReader::open(BytesSource::from(pdf.to_vec()))
        .unwrap()
        .resolve_object((id, 0))
        .is_ok()
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
    assert!(deleted_doc.get_object((5, 0)).is_err());
    assert!(!deleted_doc.has_object((5, 0)));
    // Both readers agree the object is gone — this is the last xref-layer disagreement
    // between them.
    assert!(!indexed_resolves(&deleted, 5));
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
    let mut document = assert_shared_fingerprint(&deleted, "deletion", 4, false);

    // `Xref::size` — and `Document::max_id`, which is `size - 1` — count *definitions*. The
    // trailer still declares `/Size 6` because object 5 once existed; the deletion must leave
    // the id space at 4 so the save path keeps numbering new objects from 5 rather than
    // stepping over a slot nothing occupies.
    assert_eq!(document.max_id, 4);
    assert_eq!(document.reference_table.size, 5);
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
