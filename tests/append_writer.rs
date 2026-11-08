//! The append writer, reopening after it, and the revision chain. Every test says in its doc comment what
//! failure it catches.

use lopdf::{
    AppendError, AppendSummary, AppendWriter, BytesSource, Dictionary, Document, IndexedReader, IndexedReaderError,
    IndexedReaderOptions, IndexedXrefKind, Object, ObjectId, RandomAccessSource, SourceError, SourceResult, Stream,
    StringFormat, dictionary,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

mod property {
    include!("common/append_property.rs");
}
use property::check_append_on_any_bytes;

// ---- Fixtures, built from their bytes so the expected values don't come from a reader. ----

struct Pdf {
    bytes: Vec<u8>,
    offsets: BTreeMap<u32, u64>,
}

impl Pdf {
    fn new() -> Self {
        Self {
            bytes: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            offsets: BTreeMap::new(),
        }
    }

    fn base() -> Self {
        let mut pdf = Self::new();
        pdf.obj(1, "<< /Type /Catalog /Pages 2 0 R >>");
        pdf.obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        pdf.obj(
            3,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 5 0 R >>",
        );
        pdf.obj(4, "<< /Title (base) >>");
        pdf.obj(5, "<< /Length 3 >>\nstream\nq Q\nendstream");
        pdf
    }

    fn obj(&mut self, id: u32, body: &str) -> u64 {
        let offset = self.bytes.len() as u64;
        self.offsets.insert(id, offset);
        self.bytes
            .extend_from_slice(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
        offset
    }

    /// A classic section listing `entries`, `(id, row)`, as one-row subsections.
    fn table_rows(&mut self, entries: &[(u32, String)], trailer: &str) -> u64 {
        let start = self.bytes.len() as u64;
        self.bytes.extend_from_slice(b"xref\n");
        for (id, row) in entries {
            self.bytes.extend_from_slice(format!("{id} 1\n{row}\n").as_bytes());
        }
        self.bytes.extend_from_slice(format!("trailer\n{trailer}\n").as_bytes());
        start
    }

    fn table(&mut self, trailer: &str) -> u64 {
        let mut rows = vec![(0, "0000000000 65535 f ".to_string())];
        rows.extend(
            self.offsets
                .iter()
                .map(|(id, offset)| (*id, format!("{offset:010} 00000 n "))),
        );
        self.table_rows(&rows, trailer)
    }

    fn finish(mut self, xref: u64) -> Vec<u8> {
        self.bytes
            .extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        self.bytes
    }

    /// A cross-reference stream at the next offset, as object `id`, naming every object so far.
    fn xref_stream(&mut self, id: u32, extras: &str) -> u64 {
        let start = self.bytes.len() as u64;
        self.offsets.insert(id, start);
        let mut content = Vec::new();
        let mut index = String::new();
        for (number, offset) in &self.offsets {
            index.push_str(&format!("{number} 1 "));
            content.push(1_u8);
            content.extend_from_slice(&(*offset as u32).to_be_bytes());
            content.extend_from_slice(&[0, 0]);
        }
        let size = self.offsets.keys().next_back().unwrap() + 1;
        self.bytes.extend_from_slice(
            format!(
                "{id} 0 obj\n<< /Type /XRef /Size {size} /W [1 4 2] /Index [{index}] /Length {} {extras} >>\nstream\n",
                content.len()
            )
            .as_bytes(),
        );
        self.bytes.extend_from_slice(&content);
        self.bytes.extend_from_slice(b"\nendstream\nendobj\n");
        start
    }
}

const TRAILER: &str = "<< /Size 6 /Root 1 0 R /Info 4 0 R /ID [<AABB> <CCDD>] >>";

fn classic_pdf() -> Vec<u8> {
    let mut pdf = Pdf::base();
    let xref = pdf.table(TRAILER);
    pdf.finish(xref)
}

fn xref_stream_pdf() -> Vec<u8> {
    let mut pdf = Pdf::base();
    let xref = pdf.xref_stream(6, "/Root 1 0 R /Info 4 0 R /ID [<AABB> <CCDD>]");
    pdf.finish(xref)
}

/// Written by the crate's own eager writer: object streams and a cross-reference stream.
fn object_stream_pdf() -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let page = document.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
    });
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_modern(&mut bytes).unwrap();
    bytes
}

/// Object 5 lives in object stream 6, and the table hides it as free, which is what a hybrid file is
/// for. With `shown`, the table also lists 5 as live, so the two kinds of reader disagree.
fn hybrid_pdf(shown: bool) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.obj(1, "<< /Type /Catalog /Pages 2 0 R >>");
    pdf.obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    pdf.obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>");
    pdf.obj(4, "<< /Title (base) >>");
    let member = "<< /Hidden true >>";
    let container = format!(
        "<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n5 0 {member}\nendstream",
        member.len() + 4
    );
    pdf.obj(6, &container);
    let xref_stream = pdf.bytes.len() as u64;
    let content = [2_u8, 0, 0, 0, 6, 0, 0];
    pdf.bytes
        .extend_from_slice(b"7 0 obj\n<< /Type /XRef /Size 8 /W [1 4 2] /Index [5 1] /Length 7 >>\nstream\n");
    pdf.bytes.extend_from_slice(&content);
    pdf.bytes.extend_from_slice(b"\nendstream\nendobj\n");
    pdf.offsets.insert(7, xref_stream);

    let mut rows = vec![(0, "0000000000 65535 f ".to_string())];
    rows.extend(
        pdf.offsets
            .iter()
            .map(|(id, offset)| (*id, format!("{offset:010} 00000 n "))),
    );
    rows.push((
        5,
        if shown {
            format!("{:010} 00000 n ", pdf.offsets[&4])
        } else {
            "0000000000 00001 f ".to_string()
        },
    ));
    rows.sort();
    let trailer = format!("<< /Size 8 /Root 1 0 R /Info 4 0 R /XRefStm {xref_stream} >>");
    let xref = pdf.table_rows(&rows, &trailer);
    pdf.finish(xref)
}

fn all_forms() -> Vec<(&'static str, Vec<u8>)> {
    let mut no_final_newline = classic_pdf();
    assert_eq!(no_final_newline.pop(), Some(b'\n'));
    let mut trailing_junk = classic_pdf();
    trailing_junk.extend_from_slice(&[0; 300]);
    vec![
        ("classic", classic_pdf()),
        ("xref stream", xref_stream_pdf()),
        ("object streams", object_stream_pdf()),
        ("hybrid", hybrid_pdf(false)),
        ("no final newline", no_final_newline),
        ("trailing junk", trailing_junk),
    ]
}

fn open(bytes: &[u8]) -> IndexedReader {
    IndexedReader::open(BytesSource::from(bytes.to_vec())).unwrap()
}

/// Add one object and change the catalog, on whichever file form.
fn small_update(reader: &IndexedReader) -> (Vec<u8>, AppendSummary, ObjectId) {
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(reader, &mut output).unwrap();
    let added = writer
        .add_object(&Object::Dictionary(dictionary! { "Added" => true }))
        .unwrap()
        .id;
    let root = reader.trailer_entry_raw_owned(b"Root").unwrap().as_reference().unwrap();
    let mut catalog = reader.resolve_object(root).unwrap().as_dict().unwrap().clone();
    catalog.set("Changed", true);
    writer.write_object(root, &Object::Dictionary(catalog)).unwrap();
    let summary = writer.finish().unwrap();
    (output, summary, added)
}

// ---- The promises. ----

/// The original bytes changing at all, which voids every signature in the file.
#[test]
fn the_original_bytes_are_a_prefix_of_the_output_for_every_form() {
    for (name, bytes) in all_forms() {
        let reader = open(&bytes);
        let (output, summary, _) = small_update(&reader);
        assert!(output.starts_with(&bytes), "{name}");
        assert_eq!(summary.previous_len, bytes.len() as u64, "{name}");
        assert_eq!(summary.new_len, output.len() as u64, "{name}");
    }
}

/// An update whose section or trailer a reader can't follow, or which leaves old objects behind.
#[test]
fn every_object_reads_back_through_both_readers_for_every_form() {
    for (name, bytes) in all_forms() {
        let before = open(&bytes);
        let (output, summary, added) = small_update(&before);
        let after = before
            .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(!after.index_stats().unwrap().recovered(), "{name} needed a rescan");
        assert_eq!(
            after
                .resolve_object(added)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Added")
                .unwrap(),
            &Object::Boolean(true)
        );

        let root = before.trailer_entry_raw_owned(b"Root").unwrap().as_reference().unwrap();
        assert_eq!(
            after.trailer_entry_raw_owned(b"Root").unwrap().as_reference().unwrap(),
            root,
            "{name}"
        );
        let new_catalog = after.resolve_object(root).unwrap();
        assert!(
            new_catalog.as_dict().unwrap().has(b"Changed"),
            "{name}: the new catalog isn't what's read"
        );
        for id in before.object_ids() {
            if id != root {
                assert_eq!(
                    format!("{:?}", before.resolve_object(id)),
                    format!("{:?}", after.resolve_object(id)),
                    "{name} {id:?}"
                );
            }
        }

        // The eager reader is a second, separate opinion on the same bytes.
        let eager = Document::load_mem(&output).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            eager.get_object(added).is_ok(),
            "{name}: the eager reader misses the new object"
        );
        assert!(
            eager.get_object(root).unwrap().as_dict().unwrap().has(b"Changed"),
            "{name}"
        );
    }
}

/// A new section in the wrong form, which readers that follow one kind of section fail on.
#[test]
fn the_new_section_matches_the_files_own_form() {
    for (form, expected) in [
        (classic_pdf(), IndexedXrefKind::Table),
        (hybrid_pdf(false), IndexedXrefKind::Table),
        (xref_stream_pdf(), IndexedXrefKind::Stream),
        (object_stream_pdf(), IndexedXrefKind::Stream),
    ] {
        let reader = open(&form);
        let (output, summary, _) = small_update(&reader);
        let chain = open(&output).revisions().unwrap();
        let newest = chain.revisions.last().unwrap();
        assert_eq!(newest.xref_kind, expected);
        assert_eq!(newest.startxref, summary.xref_offset);
        assert_eq!(newest.prev, Some(summary.prev));
    }
}

/// A trailer that loses `/Root` or `/Info`, shrinks `/Size`, repoints `/Prev`, changes the permanent
/// `/ID` or keeps a stale `/XRefStm`, each of which readers are known to get wrong.
#[test]
fn the_new_trailer_is_right() {
    let reader = open(&classic_pdf());
    let (output, summary, added) = small_update(&reader);
    let after = open(&output);
    assert_eq!(after.trailer_entry_raw_owned(b"Root"), Some(Object::Reference((1, 0))));
    assert_eq!(after.trailer_entry_raw_owned(b"Info"), Some(Object::Reference((4, 0))));
    assert_eq!(after.trailer_entry_raw_owned(b"Size"), Some(Object::Integer(7)));
    assert_eq!(added, (6, 0));
    assert_eq!(
        after.trailer_entry_raw_owned(b"Prev"),
        Some(Object::Integer(summary.prev as i64))
    );
    let id = after.trailer_entry_raw_owned(b"ID").unwrap();
    let id = id.as_array().unwrap();
    assert_eq!(
        id[0].as_str().unwrap(),
        [0xAA, 0xBB],
        "the permanent identifier changed"
    );
    assert_ne!(id[1].as_str().unwrap(), [0xCC, 0xDD], "the changing identifier didn't");

    let hybrid = open(&hybrid_pdf(false));
    let (output, _, _) = small_update(&hybrid);
    let after = open(&output);
    assert!(
        after.trailer_entry_raw_owned(b"XRefStm").is_none(),
        "a stale /XRefStm came along"
    );
    assert_eq!(
        after
            .resolve_object((5, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Hidden")
            .unwrap(),
        &Object::Boolean(true)
    );

    let stream = open(&xref_stream_pdf());
    let (output, summary, added) = small_update(&stream);
    let after = open(&output);
    assert!(after.trailer_entry_raw_owned(b"Length").is_none());
    assert_eq!(added, (7, 0));
    assert_eq!(summary.size, 9, "the section's own object has to count in /Size");
    assert_eq!(
        after.object_location((8, 0)),
        Some(lopdf::IndexedObjectLocation::Normal),
        "the section lists itself"
    );
    assert!(
        summary.objects.iter().all(|object| object.id != (8, 0)),
        "the section's object isn't the caller's"
    );
}

/// A file with no `/ID` staying without one, or an `/ID` of some other shape being rewritten on a guess.
#[test]
fn an_id_is_added_when_missing_and_left_alone_when_strange() {
    let mut pdf = Pdf::base();
    let xref = pdf.table("<< /Size 6 /Root 1 0 R >>");
    let reader = open(&pdf.finish(xref));
    let (output, _, _) = small_update(&reader);
    let id = open(&output).trailer_entry_raw_owned(b"ID").unwrap();
    let id = id.as_array().unwrap();
    assert_eq!(id.len(), 2);
    assert_eq!(id[0], id[1], "both strings start the same on a file that never had one");

    let mut pdf = Pdf::base();
    let xref = pdf.table("<< /Size 6 /Root 1 0 R /ID 4 0 R >>");
    let reader = open(&pdf.finish(xref));
    let (output, _, _) = small_update(&reader);
    assert_eq!(
        open(&output).trailer_entry_raw_owned(b"ID"),
        Some(Object::Reference((4, 0)))
    );
}

/// Two different updates to the same file ending up with the same file identifier.
#[test]
fn the_changing_identifier_depends_on_what_was_written() {
    let reader = open(&classic_pdf());
    let identifier = |value: i64| {
        let mut output = Vec::new();
        let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
        writer.add_object(&Object::Integer(value)).unwrap();
        writer.finish().unwrap();
        open(&output)
            .trailer_entry_raw_owned(b"ID")
            .unwrap()
            .as_array()
            .unwrap()[1]
            .clone()
    };
    assert_ne!(identifier(100), identifier(200));
    assert_eq!(
        identifier(100),
        identifier(100),
        "the same update should give the same bytes"
    );
}

/// Sections that stop chaining after a few rounds, new numbers colliding with old ones, or a revision
/// that can't be got back later. Random rounds across every form, each checked against the one before.
#[test]
fn repeated_appends_stay_valid_and_every_earlier_revision_comes_back_whole() {
    for (name, original) in all_forms() {
        let mut random = Random(0x5EED ^ original.len() as u64);
        let mut history = vec![original.clone()];
        let mut expected: BTreeMap<ObjectId, i64> = BTreeMap::new();
        for round in 0..24_i64 {
            let current = history.last().unwrap().clone();
            let reader = open(&current);
            let mut output = Vec::new();
            let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
            for _ in 0..1 + random.below(4) {
                let replaceable: Vec<ObjectId> = expected.keys().copied().collect();
                let replace = !replaceable.is_empty() && random.below(2) == 0;
                let value = round * 1_000 + random.below(1_000) as i64;
                if replace {
                    let id = replaceable[random.below(replaceable.len() as u64) as usize];
                    match writer.write_object(id, &Object::Integer(value)) {
                        Ok(_) => {
                            expected.insert(id, value);
                        }
                        Err(AppendError::AlreadyWritten { .. }) => {}
                        Err(error) => panic!("{name}: {error}"),
                    }
                } else {
                    let added = writer.add_object(&Object::Integer(value)).unwrap();
                    expected.insert(added.id, value);
                }
            }
            let summary = writer.finish().unwrap();
            let after = reader
                .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
                .unwrap_or_else(|error| panic!("{name} round {round}: {error}"));
            assert!(output.starts_with(&current));
            for (id, value) in &expected {
                assert_eq!(
                    after.resolve_object(*id).unwrap(),
                    Object::Integer(*value),
                    "{name} {id:?}"
                );
            }
            assert!(
                Document::load_mem(&output).is_ok(),
                "{name}: the eager reader gave up at round {round}"
            );
            history.push(output);
        }

        let newest = open(history.last().unwrap());
        let chain = newest.revisions().unwrap();
        let updates = chain.revisions.len() - open(&original).revisions().unwrap().revisions.len();
        assert_eq!(updates, 24, "{name}");
        assert!(
            chain.revisions.iter().skip(1).all(|revision| revision.range.is_some()),
            "{name}"
        );
        for (rounds_back, revision) in chain.revisions.iter().rev().take(24).enumerate() {
            let earlier = &history[history.len() - 1 - rounds_back];
            let mut copy = Vec::new();
            assert_eq!(
                newest.copy_revision_to(revision, &mut copy).unwrap(),
                earlier.len() as u64,
                "{name}"
            );
            assert_eq!(&copy, earlier, "{name}: revision {} isn't byte-exact", revision.index);
            let opened = newest.open_revision(revision, IndexedReaderOptions::default()).unwrap();
            assert_eq!(opened.object_ids(), open(earlier).object_ids(), "{name}");
        }
    }
}

/// A stream written with the wrong `/Length`, which a reader takes as the end of the data.
#[test]
fn a_stream_is_given_its_true_length() {
    let reader = open(&classic_pdf());
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let mut stream = Stream::new(
        dictionary! { "Length" => 1, "Extra" => 2 },
        b"endstream inside the data".to_vec(),
    );
    stream.dict.set("Length", 1);
    let added = writer.add_object(&Object::Stream(stream)).unwrap();
    let id = writer.allocate().unwrap();
    let body = vec![7_u8; 300_000];
    let piped = writer
        .write_stream_from(
            id,
            &dictionary! { "Length" => Object::Reference((1, 0)) },
            300_000,
            &mut body.as_slice(),
        )
        .unwrap();
    writer.finish().unwrap();
    let after = open(&output);
    let read = after.resolve_object(added.id).unwrap();
    assert_eq!(read.as_stream().unwrap().content, b"endstream inside the data");
    let read = after.resolve_object(piped.id).unwrap();
    assert_eq!(read.as_stream().unwrap().content, body);
    assert_eq!(
        read.as_stream().unwrap().dict.get(b"Length").unwrap(),
        &Object::Integer(300_000)
    );
}

/// A stream cut short or running long, which would otherwise end in a file whose offsets are wrong.
#[test]
fn a_stream_reader_that_lies_about_its_length_abandons_the_update() {
    let reader = open(&classic_pdf());
    for (declared, actual) in [(10_u64, 5_usize), (5, 10), (0, 1)] {
        let mut output = Vec::new();
        let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
        let id = writer.allocate().unwrap();
        let body = vec![1_u8; actual];
        let result = writer.write_stream_from(id, &Dictionary::new(), declared, &mut body.as_slice());
        assert!(
            matches!(result, Err(AppendError::StreamLengthMismatch { .. })),
            "{declared}/{actual}"
        );
        assert!(
            matches!(writer.finish(), Err(AppendError::Abandoned)),
            "{declared}/{actual}"
        );
    }
}

/// A string or name in document text escaping its token, which would turn data into PDF structure.
#[test]
fn hostile_text_stays_inside_its_token() {
    let reader = open(&classic_pdf());
    let nasty = b") /Root 99 0 R >> endobj 1 0 obj ( \\ \r\n %%EOF startxref".to_vec();
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let mut dict = Dictionary::new();
    dict.set("Literal", Object::String(nasty.clone(), StringFormat::Literal));
    dict.set("Hex", Object::String(nasty.clone(), StringFormat::Hexadecimal));
    dict.set("Name", Object::Name(nasty.clone()));
    dict.set(nasty.clone(), 1);
    let added = writer.add_object(&Object::Dictionary(dict)).unwrap();
    let summary = writer.finish().unwrap();
    let after = reader
        .verify_appended(Arc::new(BytesSource::from(output)), &summary)
        .unwrap();
    let read = after.resolve_object(added.id).unwrap();
    let read = read.as_dict().unwrap();
    assert_eq!(read.get(b"Literal").unwrap().as_str().unwrap(), nasty);
    assert_eq!(read.get(b"Hex").unwrap().as_str().unwrap(), nasty);
    assert_eq!(read.get(b"Name").unwrap().as_name().unwrap(), nasty);
    assert!(read.has(&nasty));
    assert_eq!(after.trailer_entry_raw_owned(b"Root"), Some(Object::Reference((1, 0))));
}

fn string_object(text: &[u8], format: StringFormat) -> Object {
    let mut dict = Dictionary::new();
    dict.set("S", Object::String(text.to_vec(), format));
    Object::Dictionary(dict)
}

/// A string nested past the parser's 100 levels being written bare: `finish`, the check and the rename all
/// passed, and the object could then never be read, which on a page or a form loses it for good.
#[test]
fn a_string_nested_past_the_parsers_limit_still_reads_back() {
    let reader = open(&classic_pdf());
    for depth in [99, 100, 101, 102, 1_000, 100_000] {
        let mut text = vec![b'('; depth];
        text.extend(std::iter::repeat_n(b')', depth));
        let mut output = Vec::new();
        let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
        let added = writer.add_object(&string_object(&text, StringFormat::Literal)).unwrap();
        let summary = writer.finish().unwrap();
        let after = reader
            .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
            .unwrap_or_else(|error| panic!("depth {depth}: {error}"));
        let read = after.resolve_object(added.id).unwrap();
        assert_eq!(
            read.as_dict().unwrap().get(b"S").unwrap().as_str().unwrap(),
            text,
            "depth {depth}"
        );
        let eager = Document::load_mem(&output).unwrap();
        let read = eager.get_object(added.id).unwrap().as_dict().unwrap();
        assert_eq!(
            read.get(b"S").unwrap().as_str().unwrap(),
            text,
            "depth {depth}, eager reader"
        );
    }
}

/// A hostile `/ID` that only parses because its parentheses are escaped. Carried into the new trailer bare,
/// it made a file no reader could open, and plain `finish` called that a success.
#[test]
fn a_hostile_id_doesnt_make_a_file_that_cant_be_opened() {
    let permanent = format!("({}{})", "\\(".repeat(101), "\\)".repeat(101));
    let mut pdf = Pdf::base();
    let xref = pdf.table(&format!("<< /Size 6 /Root 1 0 R /ID [{permanent} <CCDD>] >>"));
    let bytes = pdf.finish(xref);
    let reader = open(&bytes);
    let (output, summary, added) = small_update(&reader);
    let after = reader
        .verify_appended(Arc::new(BytesSource::from(output)), &summary)
        .unwrap();
    assert!(!after.index_stats().unwrap().recovered());
    assert!(after.resolve_object(added).is_ok());
    let id = after.trailer_entry_raw_owned(b"ID").unwrap();
    assert_eq!(
        id.as_array().unwrap()[0],
        reader.trailer_entry_raw_owned(b"ID").unwrap().as_array().unwrap()[0]
    );
}

/// The serializer and the parsers disagreeing on any string, which `verify_appended` can't see if the result
/// still parses. Seeded, and heavy on the bytes that need escaping.
#[test]
fn any_string_or_name_reads_back_as_it_was_written() {
    let mut random = Random(0xFACE);
    let alphabet = b"()()()\\\\\\\r\n\t %<>/#[]{}\0\xFFab0";
    let reader = open(&classic_pdf());
    for round in 0..400 {
        let length = random.below(if round % 16 == 0 { 2_000 } else { 64 }) as usize;
        let text: Vec<u8> = (0..length)
            .map(|_| alphabet[random.below(alphabet.len() as u64) as usize])
            .collect();
        let name: Vec<u8> = text.iter().copied().filter(|byte| *byte != 0).collect();
        let mut dict = Dictionary::new();
        dict.set("Literal", Object::String(text.clone(), StringFormat::Literal));
        dict.set("Hex", Object::String(text.clone(), StringFormat::Hexadecimal));
        dict.set("Name", Object::Name(name.clone()));
        let mut output = Vec::new();
        let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
        let added = writer.add_object(&Object::Dictionary(dict)).unwrap();
        let summary = writer.finish().unwrap();
        let after = reader
            .verify_appended(Arc::new(BytesSource::from(output)), &summary)
            .unwrap_or_else(|error| panic!("{text:?}: {error}"));
        let read = after.resolve_object(added.id).unwrap();
        let read = read.as_dict().unwrap();
        assert_eq!(read.get(b"Literal").unwrap().as_str().unwrap(), text);
        assert_eq!(read.get(b"Hex").unwrap().as_str().unwrap(), text);
        assert_eq!(read.get(b"Name").unwrap().as_name().unwrap(), name);
    }
}

/// An object nested just past what the parser reads being accepted, then lost. Every depth around the limit is
/// either refused before it reaches the file, or reads back.
#[test]
fn an_object_nested_near_the_limit_is_refused_or_reads_back() {
    let reader = open(&classic_pdf());
    for shape in ["mixed", "arrays", "dictionaries"] {
        let mut accepted = 0;
        let mut refused = 0;
        for depth in 90..=110 {
            let mut object = Object::Integer(1);
            for level in 0..depth {
                let array = match shape {
                    "mixed" => level % 2 == 0,
                    other => other == "arrays",
                };
                object = if array {
                    Object::Array(vec![object])
                } else {
                    Object::Dictionary(dictionary! { "K" => object })
                };
            }
            let mut output = Vec::new();
            let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
            match writer.add_object(&object) {
                Err(AppendError::InvalidObject { .. }) => refused += 1,
                Err(error) => panic!("{shape} {depth}: {error}"),
                Ok(added) => {
                    let summary = writer.finish().unwrap();
                    let after = reader
                        .verify_appended(Arc::new(BytesSource::from(output)), &summary)
                        .unwrap_or_else(|error| panic!("{shape} {depth}: {error}"));
                    assert!(after.resolve_object(added.id).is_ok(), "{shape} {depth}");
                    accepted += 1;
                }
            }
        }
        assert!(
            accepted > 0 && refused > 0,
            "{shape}: the limit isn't between 90 and 110, so this checked nothing"
        );
    }
}

/// A direct `/Info` nested just under the limit for an object, one level further down in the trailer than the check thought. The trailer then couldn't
/// be parsed and the file couldn't be opened, and a bare `finish` returned Ok.
#[test]
fn a_direct_info_near_the_limit_is_refused_or_reads_back() {
    for form in [classic_pdf(), xref_stream_pdf()] {
        let reader = open(&form);
        let (mut accepted, mut refused) = (0, 0);
        for depth in 90..=105 {
            let mut info = Object::Integer(1);
            for _ in 0..depth {
                info = Object::Dictionary(dictionary! { "K" => info });
            }
            let mut output = Vec::new();
            let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
            writer.add_object(&Object::Integer(1)).unwrap();
            if writer.set_info(info.clone()).is_err() {
                refused += 1;
                continue;
            }
            writer.finish().unwrap();
            let after = IndexedReader::open(BytesSource::from(output)).unwrap_or_else(|error| {
                panic!("depth {depth}: finish returned Ok, and the file doesn't open: {error}")
            });
            assert!(!after.index_stats().unwrap().recovered(), "depth {depth}");
            assert_eq!(after.trailer_entry_raw_owned(b"Info"), Some(info), "depth {depth}");
            accepted += 1;
        }
        assert!(
            accepted > 0 && refused > 0,
            "the limit isn't in the range tested, so this checked nothing"
        );
    }
}

/// A check that only looks at headers: an object whose header is fine and whose body can't be parsed has to fail
/// it, or a bad update replaces a good file.
#[test]
fn the_verifier_catches_an_object_that_doesnt_parse() {
    let reader = open(&classic_pdf());
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let added = writer
        .add_object(&Object::Dictionary(dictionary! { "Kept" => 1 }))
        .unwrap();
    let summary = writer.finish().unwrap();
    assert!(
        reader
            .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
            .is_ok()
    );

    let start = added.offset as usize;
    let at = start + output[start..].windows(2).position(|pair| pair == b">>").unwrap();
    output[at..at + 2].copy_from_slice(b"{{");
    let result = reader.verify_appended(Arc::new(BytesSource::from(output)), &summary);
    assert!(matches!(result.err(), Some(AppendError::VerificationFailed { reason }) if reason.contains("read back")),);
}

/// A large stream being held in memory to verify it: however long the update, it's read in small pieces.
#[test]
fn the_verifier_reads_a_stream_in_small_pieces() {
    let reader = open(&classic_pdf());
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let body = vec![7_u8; 8 * 1_024 * 1_024];
    let id = writer.allocate().unwrap();
    writer
        .write_stream_from(id, &Dictionary::new(), body.len() as u64, &mut body.as_slice())
        .unwrap();
    let summary = writer.finish().unwrap();
    let source = Arc::new(CountingSource {
        inner: BytesSource::from(output),
        largest_read: AtomicU64::new(0),
        total_read: AtomicU64::new(0),
    });
    reader.verify_appended(source.clone(), &summary).unwrap();
    let largest = source.largest_read.load(Ordering::Relaxed);
    assert!(
        largest <= 256 * 1_024,
        "the check read {largest} bytes at once from a file with an 8 MiB stream"
    );
}

/// A same-length change to a written stream's data passing the check, since it parses just as well. A disk or a prepare step can do that, and the
/// change would reach the document.
#[test]
fn the_verifier_catches_a_changed_byte_in_a_written_stream() {
    let reader = open(&classic_pdf());
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let added = writer
        .add_object(&Object::Stream(Stream::new(Dictionary::new(), b"the data".to_vec())))
        .unwrap();
    let summary = writer.finish().unwrap();
    assert!(
        reader
            .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
            .is_ok()
    );

    let start = added.offset as usize;
    let at = start
        + output[start..]
            .windows(8)
            .position(|window| window == b"the data")
            .unwrap();
    output[at] ^= 1;
    let result = reader.verify_appended(Arc::new(BytesSource::from(output)), &summary);
    assert!(matches!(result.err(), Some(AppendError::VerificationFailed { reason }) if reason.contains("added bytes")));
}

/// Objects with no valid PDF spelling reaching the file, or bad numbers quietly shadowing real objects.
#[test]
fn a_caller_mistake_is_refused_before_it_reaches_the_file() {
    let bytes = {
        let mut pdf = Pdf::base();
        let rows = [
            (0, "0000000000 65535 f ".to_string()),
            (1, format!("{:010} 00000 n ", pdf.offsets[&1])),
            (2, format!("{:010} 00000 n ", pdf.offsets[&2])),
            (3, format!("{:010} 00000 n ", pdf.offsets[&3])),
            (4, format!("{:010} 00000 n ", pdf.offsets[&4])),
            (5, "0000000000 00002 f ".to_string()),
        ];
        let xref = pdf.table_rows(&rows, "<< /Size 7 /Root 1 0 R >>");
        pdf.finish(xref)
    };
    let reader = open(&bytes);
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();

    let one = Object::Integer(1);
    assert!(matches!(
        writer.write_object((6, 0), &one),
        Err(AppendError::UnknownObject { .. })
    ));
    assert!(matches!(
        writer.write_object((0, 0), &one),
        Err(AppendError::UnknownObject { .. })
    ));
    assert!(matches!(
        writer.write_object((5, 2), &one),
        Err(AppendError::ObjectIsFree { .. })
    ));
    assert!(matches!(
        writer.write_object((4, 1), &one),
        Err(AppendError::GenerationMismatch { expected: 0, .. })
    ));

    let id = writer.allocate().unwrap();
    assert!(matches!(
        writer.write_object(id, &Object::Real(f32::NAN)),
        Err(AppendError::InvalidObject { .. })
    ));
    assert!(matches!(
        writer.write_object(id, &Object::Name(vec![b'a', 0])),
        Err(AppendError::InvalidObject { .. })
    ));
    let nested = Object::Array(vec![Object::Stream(Stream::new(Dictionary::new(), Vec::new()))]);
    assert!(matches!(
        writer.write_object(id, &nested),
        Err(AppendError::InvalidObject { .. })
    ));
    let mut deep = Object::Integer(0);
    for _ in 0..1_000 {
        deep = Object::Array(vec![deep]);
    }
    assert!(matches!(
        writer.write_object(id, &deep),
        Err(AppendError::InvalidObject { .. })
    ));
    assert!(matches!(
        writer.set_info(Object::Integer(1)),
        Err(AppendError::InvalidObject { .. })
    ));
    let position = writer.position();
    assert_eq!(position, bytes.len() as u64, "a refused object wrote something");

    writer.write_object(id, &one).unwrap();
    assert!(matches!(
        writer.write_object(id, &one),
        Err(AppendError::AlreadyWritten { .. })
    ));
    writer
        .write_object((4, 0), &Object::Dictionary(dictionary! { "Title" => "x" }))
        .unwrap();
    assert!(matches!(
        writer.write_object((4, 0), &one),
        Err(AppendError::AlreadyWritten { .. })
    ));

    let unwritten = writer.allocate().unwrap();
    assert!(matches!(writer.finish(), Err(AppendError::ReservedNotWritten { id }) if id == unwritten));

    let writer = AppendWriter::new(&reader, Vec::new()).unwrap();
    assert!(matches!(writer.finish(), Err(AppendError::EmptyUpdate)));

    let mut writer = AppendWriter::new(&reader, Vec::new()).unwrap();
    writer.add_object(&one).unwrap();
    writer.set_root((99, 0));
    assert!(matches!(writer.finish(), Err(AppendError::TrailerTargetMissing { .. })));
}

/// Replacing an object stream, which would take every object in it away with no sign of a problem.
#[test]
fn an_object_stream_with_live_members_cant_be_replaced() {
    let bytes = hybrid_pdf(false);
    let reader = open(&bytes);
    let mut writer = AppendWriter::new(&reader, Vec::new()).unwrap();
    assert!(matches!(
        writer.write_object((6, 0), &Object::Integer(1)),
        Err(AppendError::ObjectStreamContainer { .. })
    ));
    // A member can be replaced: the new version wins, and the container stays.
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    writer.write_object((5, 0), &Object::Integer(55)).unwrap();
    writer.finish().unwrap();
    assert_eq!(open(&output).resolve_object((5, 0)).unwrap(), Object::Integer(55));
}

/// A new `/Info` or `/Root` not reaching the trailer, which would make an `/Info` edit silently vanish.
#[test]
fn info_and_root_can_be_repointed() {
    let reader = open(&classic_pdf());
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    let info = writer
        .add_object(&Object::Dictionary(
            dictionary! { "Title" => Object::string_literal("new") },
        ))
        .unwrap();
    let catalog = writer
        .add_object(&Object::Dictionary(
            dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) },
        ))
        .unwrap();
    writer.set_info(Object::Reference(info.id)).unwrap();
    writer.set_root(catalog.id);
    writer.finish().unwrap();
    let after = open(&output);
    assert_eq!(after.trailer_entry_raw_owned(b"Info"), Some(Object::Reference(info.id)));
    assert_eq!(
        after.trailer_entry_raw_owned(b"Root"),
        Some(Object::Reference(catalog.id))
    );
    assert_eq!(after.page_count().unwrap(), 1);
}

// ---- What it refuses. ----

/// An update on a rebuilt index, which has no `/Prev`, so readers would lose every older object.
#[test]
fn a_rebuilt_index_is_refused() {
    let mut bytes = classic_pdf();
    let cut = bytes.windows(9).rposition(|window| window == b"startxref").unwrap();
    bytes.truncate(cut);
    let reader = open(&bytes);
    assert!(reader.index_stats().unwrap().recovered());
    assert!(matches!(reader.check_appendable(), Err(AppendError::RecoveredIndex)));
    assert!(matches!(
        AppendWriter::new(&reader, Vec::new()).err(),
        Some(AppendError::RecoveredIndex)
    ));
    assert!(matches!(
        reader.revisions(),
        Err(IndexedReaderError::RevisionsUnavailable)
    ));
}

/// New objects going into an encrypted file in the clear, where they'd read back as garbage.
#[test]
fn an_encrypted_file_is_refused() {
    let bytes = std::fs::read("assets/encrypted.pdf").unwrap();
    let reader = IndexedReader::open(BytesSource::from(bytes)).unwrap();
    assert!(reader.is_encrypted());
    assert!(matches!(reader.check_appendable(), Err(AppendError::Encrypted)));
}

/// Offsets written from byte 0 in a file whose readers count from its header, which lost the new
/// objects when measured in the signing research (E5).
#[test]
fn a_header_not_at_byte_zero_is_refused() {
    let mut bytes = b"junk before the header\n".to_vec();
    bytes.extend_from_slice(&classic_pdf());
    let reader = open(&bytes);
    assert!(matches!(
        reader.check_appendable(),
        Err(AppendError::HeaderNotAtStart { offset: 23 })
    ));
}

/// Chaining onto a `/Prev` loop, where older sections were never read and can't be vouched for.
#[test]
fn a_prev_loop_is_refused_and_reported() {
    let mut pdf = Pdf::base();
    let xref = pdf.bytes.len() as u64;
    let trailer = format!("<< /Size 6 /Root 1 0 R /Prev {xref} >>");
    let at = pdf.table(&trailer);
    assert_eq!(at, xref);
    let reader = open(&pdf.finish(xref));
    assert!(matches!(reader.check_appendable(), Err(AppendError::PrevLoop)));
    let chain = reader.revisions().unwrap();
    assert!(chain.has_loop);
    assert!(!chain.is_regular());
}

/// A hybrid whose table and supplement disagree: a later signature checker could read a different
/// document from the one this writer saw.
#[test]
fn an_ambiguous_hybrid_is_refused() {
    assert!(open(&hybrid_pdf(false)).check_appendable().is_ok());
    let reader = open(&hybrid_pdf(true));
    assert!(matches!(
        reader.check_appendable(),
        Err(AppendError::AmbiguousHybrid { object: 5 })
    ));
}

/// A `/Root` that goes nowhere, which would produce a well-formed file that isn't a document.
#[test]
fn a_root_that_goes_nowhere_is_refused() {
    for trailer in [
        "<< /Size 6 >>",
        "<< /Size 6 /Root 99 0 R >>",
        "<< /Size 6 /Root 5 0 R >>",
        "<< /Size 6 /Root (x) >>",
    ] {
        let mut pdf = Pdf::base();
        let xref = pdf.table(trailer);
        let bytes = pdf.finish(xref);
        let reader = open(&bytes);
        assert!(
            matches!(reader.check_appendable(), Err(AppendError::InvalidRoot)),
            "{trailer}"
        );
    }
}

/// Filling the chain to the reader's own limit, which would write a file that then can't be opened.
#[test]
fn a_full_chain_is_refused() {
    let bytes = classic_pdf();
    let options = IndexedReaderOptions {
        max_xref_sections: 1,
        ..IndexedReaderOptions::default()
    };
    let reader = IndexedReader::open_with_options(BytesSource::from(bytes), options).unwrap();
    assert!(matches!(
        reader.check_appendable(),
        Err(AppendError::ChainFull { sections: 1 })
    ));
}

/// Numbers past what PDFium reads, where the new object would exist for one engine and not the other.
#[test]
fn a_huge_size_refuses_new_numbers_and_still_allows_changes() {
    let mut pdf = Pdf::base();
    let xref = pdf.table("<< /Size 900000000000 /Root 1 0 R >>");
    let bytes = pdf.finish(xref);
    let reader = open(&bytes);
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    assert!(matches!(
        writer.allocate(),
        Err(AppendError::ObjectNumberTooLarge { .. })
    ));
    writer.write_object((4, 0), &Object::Integer(1)).unwrap();
    let summary = writer.finish().unwrap();
    assert_eq!(summary.size, 900_000_000_000, "/Size must not shrink");
    assert_eq!(open(&output).resolve_object((4, 0)).unwrap(), Object::Integer(1));
}

// ---- Truncated and hostile input. ----

/// A panic, or a wrong file, on any cut of a real structure. Every length of every form.
#[test]
fn every_truncation_of_every_form_is_safe() {
    let mut appended = 0;
    for (_, bytes) in all_forms() {
        for cut in 0..bytes.len() {
            appended += usize::from(check_append_on_any_bytes(&bytes[..cut]));
        }
    }
    assert!(appended > 0, "no cut reached the writer, so this checked nothing");
}

/// A panic, or a wrong file, on structure damaged at random. Seeded, so a failure reproduces.
#[test]
fn seeded_damage_is_safe() {
    let forms = all_forms();
    let mut random = Random(0xA11CE);
    let mut appended = 0;
    let rounds = 6_000;
    for _ in 0..rounds {
        let (_, form) = &forms[random.below(forms.len() as u64) as usize];
        let mut bytes = form.clone();
        // Mostly damage in place. A cut or an insert moves every offset after it, which only ever reaches the rescan.
        for _ in 0..1 + random.below(3) {
            let at = random.below(bytes.len() as u64) as usize;
            match random.below(16) {
                0..=4 => bytes[at] = random.below(256) as u8,
                5..=9 => bytes[at] ^= 1 << random.below(8),
                10..=13 => bytes[at] = b"0123456789 /<>[]()"[random.below(18) as usize],
                14 => {
                    let length = (random.below(64) as usize).min(bytes.len() - at);
                    bytes.drain(at..at + length);
                    if bytes.is_empty() {
                        bytes.push(0);
                    }
                }
                _ => {
                    let (_, other) = &forms[random.below(forms.len() as u64) as usize];
                    let from = random.below(other.len() as u64) as usize;
                    let length = (random.below(128) as usize).min(other.len() - from);
                    let tail = bytes.split_off(at);
                    bytes.extend_from_slice(&other[from..from + length]);
                    bytes.extend_from_slice(&tail);
                }
            }
        }
        appended += usize::from(check_append_on_any_bytes(&bytes));
    }
    assert!(
        appended > rounds / 5,
        "only {appended} of {rounds} reached the writer, so this checks too little"
    );
}

// ---- Memory: what it reads. ----

struct CountingSource {
    inner: BytesSource,
    largest_read: AtomicU64,
    total_read: AtomicU64,
}

impl RandomAccessSource for CountingSource {
    fn len(&self) -> SourceResult<u64> {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
        self.largest_read.fetch_max(out.len() as u64, Ordering::Relaxed);
        let read = self.inner.read_at(offset, out)?;
        self.total_read.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

fn padded_pdf(padding: usize) -> Vec<u8> {
    let mut pdf = Pdf::base();
    pdf.bytes.extend_from_slice(b"%");
    pdf.bytes.resize(pdf.bytes.len() + padding, b'x');
    pdf.bytes.push(b'\n');
    let xref = pdf.table(TRAILER);
    pdf.finish(xref)
}

/// The writer loading the whole file, on a path where 330 MiB inputs exist. The copy has to read in small
/// pieces, and one read each, not more.
#[test]
fn the_original_is_read_in_small_pieces() {
    let bytes = padded_pdf(8 * 1_024 * 1_024);
    let source = Arc::new(CountingSource {
        inner: BytesSource::from(bytes.clone()),
        largest_read: AtomicU64::new(0),
        total_read: AtomicU64::new(0),
    });
    let reader = IndexedReader::open_shared(source.clone(), IndexedReaderOptions::default()).unwrap();
    let opened = source.total_read.load(Ordering::Relaxed);
    assert!(opened < 256 * 1_024, "open read {opened} bytes");

    source.largest_read.store(0, Ordering::Relaxed);
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    writer.add_object(&Object::Integer(1)).unwrap();
    writer.finish().unwrap();
    assert!(output.starts_with(&bytes));
    assert!(
        output.len() - bytes.len() < 1_024,
        "a one-object update wrote {} bytes",
        output.len() - bytes.len()
    );
    let largest = source.largest_read.load(Ordering::Relaxed);
    assert!(largest <= 64 * 1_024, "the prefix copy made a {largest}-byte read");
    let copied = source.total_read.load(Ordering::Relaxed) - opened;
    assert!(
        copied < bytes.len() as u64 + 256 * 1_024,
        "the copy read {copied} bytes of an {}-byte file",
        bytes.len()
    );
}

/// A cached reader's copy passing through its cache, which would empty it for nothing.
#[test]
fn a_copy_leaves_the_readers_cache_alone() {
    let bytes = padded_pdf(4 * 1_024 * 1_024);
    let reader = IndexedReader::open_cached(
        BytesSource::from(bytes.clone()),
        IndexedReaderOptions::default(),
        lopdf::IndexedReaderCacheOptions::new(16 * 1_024 * 1_024, 4_096),
    )
    .unwrap();
    let before = reader.cache_stats().source().loads();
    let mut output = Vec::new();
    let mut writer = AppendWriter::new(&reader, &mut output).unwrap();
    writer.add_object(&Object::Integer(1)).unwrap();
    writer.finish().unwrap();
    assert!(output.starts_with(&bytes));
    let loads = reader.cache_stats().source().loads() - before;
    assert!(loads <= 2, "the copy loaded {loads} chunks through the cache");
}

// ---- Revisions. ----

/// Revision ranges that don't meet end to end, so a signature's coverage can't be read off them.
#[test]
fn revisions_list_their_byte_ranges() {
    let original = classic_pdf();
    let first = open(&original);
    let (second, _, _) = small_update(&first);
    let (third, summary, _) = small_update(&open(&second));
    let chain = open(&third).revisions().unwrap();
    assert!(chain.is_regular());
    assert_eq!(chain.trailing_bytes, Some(0));
    let ranges: Vec<_> = chain
        .revisions
        .iter()
        .map(|revision| revision.range.clone().unwrap())
        .collect();
    assert_eq!(
        ranges,
        [
            0..original.len() as u64,
            original.len() as u64..second.len() as u64,
            second.len() as u64..third.len() as u64
        ]
    );
    assert_eq!(chain.revisions[2].startxref, summary.xref_offset);
    assert_eq!(chain.revisions[0].prev, None);
    assert_eq!(chain.revisions[0].entry_count, 6);
    assert_eq!(chain.revisions[1].entry_count, 2);
    assert_eq!(
        chain
            .revisions
            .iter()
            .map(|revision| revision.index)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

/// Trailing junk being counted as part of the last revision, which would hide bytes added after a
/// signature.
#[test]
fn bytes_after_the_last_marker_are_reported() {
    let mut bytes = classic_pdf();
    let end = bytes.len() as u64;
    bytes.extend_from_slice(b"\n\n\0\0trailing");
    let chain = open(&bytes).revisions().unwrap();
    assert_eq!(chain.revisions[0].range, Some(0..end));
    assert_eq!(chain.trailing_bytes, Some(bytes.len() as u64 - end));
}

/// The revision list ignoring the cap the reader was opened with, which would give a hostile file back the long walk
/// the cap exists to prevent. The list is only asked for after an open that passed, so the same cap gives the same chain.
#[test]
fn the_revision_list_walks_the_same_chain_under_the_same_cap() {
    let options = IndexedReaderOptions {
        max_xref_sections: 3,
        ..IndexedReaderOptions::default()
    };
    let (once, _, _) = small_update(&open(&classic_pdf()));
    let (twice, _, _) = small_update(&open(&once));
    let reader = IndexedReader::open_with_options(BytesSource::from(twice), options).unwrap();
    assert_eq!(reader.revisions().unwrap().revisions.len(), 3);
}

/// A CRLF or a missing line ending landing on the wrong side of a revision's end, or a signature that
/// ends at `%%EOF` not matching one that ends after its newline.
#[test]
fn a_revision_ends_after_its_line_ending_and_can_be_found_either_way() {
    for (eol, length) in [(&b"\r\n"[..], 2_u64), (b"\n", 1), (b"\r", 1), (b"", 0)] {
        let mut bytes = classic_pdf();
        bytes.pop();
        let marker_end = bytes.len() as u64;
        bytes.extend_from_slice(eol);
        let reader = open(&bytes);
        let chain = reader.revisions().unwrap();
        let revision = &chain.revisions[0];
        assert_eq!(revision.eof_marker_end, Some(marker_end));
        assert_eq!(revision.range, Some(0..marker_end + length));
        assert_eq!(chain.revision_ending_at(marker_end).map(|found| found.index), Some(0));
        assert_eq!(
            chain.revision_ending_at(marker_end + length).map(|found| found.index),
            Some(0)
        );
        assert!(chain.revision_ending_at(marker_end - 1).is_none());
    }
}

/// The line ending the writer adds to a file that has none coming back as part of the original, which
/// made an extracted revision one byte longer than the file it was. Found on 5 of 141 real files.
#[test]
fn a_file_without_a_final_line_ending_comes_back_exact() {
    let mut original = classic_pdf();
    assert_eq!(original.pop(), Some(b'\n'));
    let (once, _, _) = small_update(&open(&original));
    let (twice, _, _) = small_update(&open(&once));
    let reader = open(&twice);
    let chain = reader.revisions().unwrap();
    assert!(chain.is_regular());
    assert_eq!(chain.revisions[0].range, Some(0..original.len() as u64));
    assert_eq!(chain.revisions[1].range, Some(original.len() as u64..once.len() as u64));
    let mut copy = Vec::new();
    reader.copy_revision_to(&chain.revisions[0], &mut copy).unwrap();
    assert_eq!(copy, original);
    let mut copy = Vec::new();
    reader.copy_revision_to(&chain.revisions[1], &mut copy).unwrap();
    assert_eq!(copy, once);
}

/// The added line ending crossing the end of the window the marker is read from, which made it look like the old file's
/// own, and the old revision came back one byte too long. Tried at every offset around the edge.
#[test]
fn an_added_line_ending_at_the_edge_of_the_window_is_still_recognised() {
    let mut proved = 0;
    for padding in 4_000..4_090 {
        let mut pdf = Pdf::base();
        let xref = pdf.table(TRAILER);
        // A comment between the trailer and its marker pushes the marker towards the window's edge.
        pdf.bytes.push(b'%');
        pdf.bytes.resize(pdf.bytes.len() + padding, b'x');
        pdf.bytes.push(b'\n');
        let mut original = pdf.finish(xref);
        assert_eq!(original.pop(), Some(b'\n'));
        let (output, _, _) = small_update(&open(&original));
        let chain = open(&output).revisions().unwrap();
        match &chain.revisions[0].range {
            Some(range) => {
                proved += 1;
                assert_eq!(range.end, original.len() as u64, "padding {padding}");
            }
            // A marker past the window proves nothing, which is fine. Coming out a byte long isn't.
            None => assert!(
                padding > 4_070,
                "padding {padding}: this marker is well inside the window"
            ),
        }
    }
    assert!(proved > 60, "only {proved} markers were proved, so this checked little");
}

/// A linearized file reported as two revisions, the first of which would be a prefix that isn't a
/// document, or an update on one chaining to the wrong section.
#[test]
fn a_linearized_files_two_sections_are_one_revision() {
    // Laid out as a linearizer does: the first-page section comes first, says `startxref 0`, and points
    // ahead with /Prev. The main section is last, and its startxref names the first-page section.
    let mut pdf = Pdf::new();
    pdf.obj(7, "<< /Linearized 1 /L 0 /O 3 /E 0 /N 1 /T 0 >>");
    let head = pdf.bytes.len() as u64;
    let reserve = "0000000000";
    let head_section = format!(
        "xref\n7 1\n{:010} 00000 n \ntrailer\n<< /Size 8 /Root 1 0 R /Prev {reserve} >>\nstartxref\n0\n%%EOF\n",
        pdf.offsets[&7]
    );
    pdf.bytes.extend_from_slice(head_section.as_bytes());
    pdf.obj(1, "<< /Type /Catalog /Pages 2 0 R >>");
    pdf.obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    pdf.obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>");
    let main = pdf.bytes.len() as u64;
    let rows = [
        (0, "0000000000 65535 f ".to_string()),
        (1, format!("{:010} 00000 n ", pdf.offsets[&1])),
        (2, format!("{:010} 00000 n ", pdf.offsets[&2])),
        (3, format!("{:010} 00000 n ", pdf.offsets[&3])),
    ];
    pdf.table_rows(&rows, "<< /Size 8 /Root 1 0 R >>");
    let mut bytes = pdf.finish(head);
    let located = head as usize + head_section.find(reserve).unwrap();
    bytes[located..located + 10].copy_from_slice(format!("{main:010}").as_bytes());

    let reader = open(&bytes);
    assert!(!reader.index_stats().unwrap().recovered());
    let chain = reader.revisions().unwrap();
    assert_eq!(chain.revisions.len(), 1);
    let revision = &chain.revisions[0];
    assert!(revision.linearized);
    assert_eq!(revision.range, Some(0..bytes.len() as u64));
    assert_eq!(revision.startxref, head);
    assert_eq!(revision.entry_count, 5);
    assert_eq!(
        revision.prev, None,
        "the main section is part of this revision, not the one before it"
    );

    let (output, summary, added) = small_update(&reader);
    assert_eq!(summary.prev, head, "an update chains to what startxref named");
    let chain = open(&output).revisions().unwrap();
    assert!(chain.is_regular());
    assert_eq!(chain.revisions.len(), 2);
    assert!(chain.revisions[0].linearized);
    assert_eq!(chain.revisions[0].range, Some(0..bytes.len() as u64));
    assert_eq!(
        open(&output)
            .resolve_object(added)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Added")
            .unwrap(),
        &Object::Boolean(true)
    );
    assert_eq!(open(&output).page_count().unwrap(), 1);
}

/// Two sections closed by one `startxref`, as one real file in the corpus has, being reported as a revision with no
/// provable end. The older section's first marker names the newer section, so they end together.
#[test]
fn sections_closed_by_one_marker_are_one_revision() {
    let mut pdf = Pdf::base();
    let first = pdf.table(TRAILER);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{first}\n%%EOF\n").as_bytes());
    let boundary = pdf.bytes.len() as u64;
    pdf.obj(6, "<< /Newer 1 >>");
    let rows = [(6, format!("{:010} 00000 n ", pdf.offsets[&6]))];
    let older = pdf.table_rows(&rows, &format!("<< /Size 8 /Root 1 0 R /Prev {first} >>"));
    pdf.obj(7, "<< /Newer 2 >>");
    let rows = [(7, format!("{:010} 00000 n ", pdf.offsets[&7]))];
    let newer = pdf.table_rows(&rows, &format!("<< /Size 8 /Root 1 0 R /Prev {older} >>"));
    let bytes = pdf.finish(newer);
    let reader = open(&bytes);
    let chain = reader.revisions().unwrap();
    assert!(chain.is_regular());
    assert_eq!(chain.revisions.len(), 2);
    assert_eq!(chain.revisions[0].range, Some(0..boundary));
    assert_eq!(chain.revisions[1].range, Some(boundary..bytes.len() as u64));
    assert_eq!(chain.revisions[1].startxref, newer);
    assert_eq!(chain.revisions[1].prev, Some(first));
    assert_eq!(chain.revisions[1].entry_count, 2);
    assert_eq!(chain.revisions[1].index, 1);
    assert_eq!(
        reader
            .open_revision(&chain.revisions[0], IndexedReaderOptions::default())
            .unwrap()
            .object_ids()
            .len(),
        5
    );
}

/// A guessed range: when an older section has no marker of its own, or the chain runs backwards, a
/// guess would claim bytes for a revision that a reader of that prefix wouldn't see.
#[test]
fn a_revision_with_no_marker_or_out_of_order_has_no_range() {
    // An older section with no startxref after it within the window.
    let mut pdf = Pdf::base();
    let older = pdf.table(TRAILER);
    pdf.bytes.extend_from_slice(b"%");
    pdf.bytes.resize(pdf.bytes.len() + 8 * 1_024, b'x');
    pdf.bytes.push(b'\n');
    pdf.obj(6, "<< /Newer true >>");
    let rows = [(6, format!("{:010} 00000 n ", pdf.offsets[&6]))];
    let newer = pdf.table_rows(&rows, &format!("<< /Size 7 /Root 1 0 R /Prev {older} >>"));
    let reader = open(&pdf.finish(newer));
    let chain = reader.revisions().unwrap();
    assert_eq!(chain.revisions.len(), 2);
    assert!(!chain.is_regular());
    assert_eq!(chain.revisions[0].range, None);
    assert!(chain.revisions[1].range.is_some());
    let oldest = &chain.revisions[0];
    assert!(matches!(
        reader.copy_revision_to(oldest, &mut Vec::new()),
        Err(IndexedReaderError::RevisionRangeUnavailable { index: 0 })
    ));
    assert!(matches!(
        reader.open_revision(oldest, IndexedReaderOptions::default()),
        Err(IndexedReaderError::RevisionRangeUnavailable { index: 0 })
    ));

    // The file's last startxref names a section whose /Prev points forward, to a section that a prefix
    // ending at the newer section's own marker can't contain.
    let mut pdf = Pdf::base();
    let newest = pdf.table(&format!("<< /Size 6 /Root 1 0 R /Prev {:010} >>", 0));
    pdf.bytes
        .extend_from_slice(format!("startxref\n{newest}\n%%EOF\n").as_bytes());
    let older = pdf.table(TRAILER);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{older}\n%%EOF\n").as_bytes());
    let mut bytes = pdf.finish(newest);
    let prev_at = find_last(&bytes[..older as usize], b"/Prev ") + b"/Prev ".len();
    bytes[prev_at..prev_at + 10].copy_from_slice(format!("{older:010}").as_bytes());
    let chain = open(&bytes).revisions().unwrap();
    assert_eq!(chain.revisions.len(), 2);
    assert!(!chain.is_regular());
    assert_eq!(chain.revisions[1].startxref, newest);
    assert_eq!(
        chain.revisions[1].range, None,
        "the newest section sits inside the older one's bytes"
    );
}

/// A `/Prev` pointing past a revision's own marker. The newest revision used to get a range that its own chain runs out
/// of, `revision_ending_at` called it whole, and `open_revision` answered with a rescan and Ok. A signature report built on
/// that would say a signature covers a whole revision when no such revision exists.
#[test]
fn a_prev_past_the_revisions_end_gives_it_no_range() {
    let mut pdf = Pdf::base();
    let newest = pdf.table(&format!("<< /Size 6 /Root 1 0 R /Prev {:010} >>", 0));
    pdf.bytes
        .extend_from_slice(format!("startxref\n{newest}\n%%EOF\n").as_bytes());
    let end = pdf.bytes.len() as u64;
    let beyond = pdf.table(TRAILER);
    // No marker of its own, and the file's last startxref names the first section.
    let mut bytes = pdf.finish(newest);
    let prev_at = find_last(&bytes[..beyond as usize], b"/Prev ") + b"/Prev ".len();
    bytes[prev_at..prev_at + 10].copy_from_slice(format!("{beyond:010}").as_bytes());
    let reader = open(&bytes);
    assert!(!reader.index_stats().unwrap().recovered());
    let chain = reader.revisions().unwrap();
    assert!(!chain.is_regular());
    assert!(
        chain.revisions.iter().all(|revision| revision.range.is_none()),
        "{chain:?}"
    );
    assert!(
        chain.revision_ending_at(end).is_none(),
        "a prefix that can't be read was called a whole revision"
    );
}

/// A `/XRefStm` past the revision's end, the same trap by the other pointer.
#[test]
fn an_xref_supplement_past_the_revisions_end_gives_it_no_range() {
    let mut pdf = Pdf::base();
    let older = pdf.table(TRAILER);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{older}\n%%EOF\n").as_bytes());
    let boundary = pdf.bytes.len();
    let section = pdf.table(&format!("<< /Size 6 /Root 1 0 R /Prev {older} /XRefStm {:010} >>", 0));
    pdf.bytes
        .extend_from_slice(format!("startxref\n{section}\n%%EOF\n").as_bytes());
    let end = pdf.bytes.len() as u64;
    let supplement = pdf.bytes.len() as u64;
    pdf.bytes
        .extend_from_slice(b"9 0 obj\n<< /Type /XRef /Size 10 /W [1 4 2] /Index [9 1] /Length 7 >>\nstream\n");
    pdf.bytes.extend_from_slice(&[1, 0, 0, 0, 9, 0, 0]);
    pdf.bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let mut bytes = pdf.finish(section);
    let at = boundary + find_last(&bytes[boundary..end as usize], b"/XRefStm ") + b"/XRefStm ".len();
    bytes[at..at + 10].copy_from_slice(format!("{supplement:010}").as_bytes());
    let chain = open(&bytes).revisions().unwrap();
    assert!(
        chain.revisions[0].range.is_some(),
        "the older revision doesn't depend on it"
    );
    assert_eq!(chain.revisions[1].startxref, section);
    assert_eq!(chain.revisions[1].range, None);
    assert!(chain.revision_ending_at(end).is_none());
}

/// Patch a fixed-width number, written as ten zeros after `label` at or after `from`.
fn patch_number(bytes: &mut [u8], from: usize, label: &[u8], value: u64) {
    let at = from
        + bytes[from..]
            .windows(label.len())
            .position(|window| window == label)
            .unwrap()
        + label.len();
    bytes[at..at + 10].copy_from_slice(format!("{value:010}").as_bytes());
}

/// A shadow attack's activation: `S` names `O`, a section sitting unnoticed inside bytes an earlier revision `R` already holds, and `O`'s
/// entries go live. `O` was merged into `S` because its first marker names `S`, so the chain read as regular, with `R` signed and the update
/// showing only new bytes. A signature report relies on this: `O`'s marker isn't `S`'s, so it's not one revision, and the chain isn't regular.
#[test]
fn a_section_hidden_inside_an_earlier_revision_isnt_merged_into_a_later_one() {
    let mut pdf = Pdf::base();
    let shadow_body = pdf.bytes.len() as u64;
    pdf.obj(6, "<< /Shadow true >>");
    let rows = [(6, format!("{shadow_body:010} 00000 n "))];
    let o = pdf.table_rows(&rows, "<< /Size 7 /Root 1 0 R /Prev 0000000000 >>");
    let o_marker = pdf.bytes.len();
    pdf.bytes.extend_from_slice(b"startxref\n0000000000\n%%EOF\n");
    let r = pdf.table(TRAILER);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{r}\n%%EOF\n").as_bytes());
    let signed_end = pdf.bytes.len() as u64;
    let s = pdf.bytes.len() as u64;
    let rows = [(4, format!("{:010} 00000 n ", pdf.offsets[&4]))];
    pdf.table_rows(&rows, &format!("<< /Size 7 /Root 1 0 R /Prev {o} >>"));
    let mut bytes = pdf.finish(s);
    patch_number(&mut bytes, o as usize, b"/Prev ", r);
    patch_number(&mut bytes, o_marker, b"startxref\n", s);

    let reader = open(&bytes);
    assert!(!reader.index_stats().unwrap().recovered());
    assert!(
        reader.resolve_object((6, 0)).is_ok(),
        "the layout has to really activate the hidden object"
    );
    let chain = reader.revisions().unwrap();
    assert_eq!(chain.revisions.len(), 3, "{chain:?}");
    assert!(
        !chain.is_regular(),
        "a hidden section went live and the chain was called regular"
    );
    assert_eq!(chain.revisions[0].startxref, r);
    assert_eq!(chain.revisions[0].range, Some(0..signed_end));
    assert_eq!(chain.revisions[1].startxref, o);
    assert_eq!(chain.revisions[1].range, None);
    assert_eq!(chain.revisions[2].prev, Some(o));
}

/// A section and its marker written inside a string in an older section's trailer: `revision_ending_at` named a prefix that cuts the older
/// section's trailer in half, so a report would call bytes a whole revision that no reader could open as one.
#[test]
fn a_section_inside_an_older_trailers_string_has_no_range() {
    let mut pdf = Pdf::base();
    let r = pdf.table(TRAILER);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{r}\n%%EOF\n").as_bytes());
    let o = pdf.bytes.len() as u64;
    let rows = [(4, format!("{:010} 00000 n ", pdf.offsets[&4]))];
    let head = pdf.table_rows(&rows, &format!("<< /Size 6 /Root 1 0 R /Prev {r} /Note (X)"));
    assert_eq!(head, o);
    // Take the placeholder back out, so that the inner section goes inside the string.
    let cut = pdf.bytes.len() - b"X)\n".len();
    pdf.bytes.truncate(cut);
    let s = pdf.bytes.len() as u64;
    let inner = format!(
        "xref\n4 1\n{:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /Prev {o} >>\nstartxref\n{s}\n%%EOF\n",
        pdf.offsets[&4]
    );
    pdf.bytes.extend_from_slice(inner.as_bytes());
    let inner_end = pdf.bytes.len() as u64;
    pdf.bytes.extend_from_slice(b") >>\n");
    let bytes = pdf.finish(s);

    let reader = open(&bytes);
    assert!(!reader.index_stats().unwrap().recovered());
    let chain = reader.revisions().unwrap();
    assert!(!chain.is_regular(), "{chain:?}");
    assert!(
        chain.revision_ending_at(inner_end).is_none(),
        "a prefix cutting a trailer in half was called a whole revision"
    );
}

/// `open_revision` answering with a rescan and Ok, on bytes that aren't the revision they were asked to be.
#[test]
fn open_revision_refuses_bytes_that_need_a_rescan() {
    let original = classic_pdf();
    let (output, _, _) = small_update(&open(&original));
    let reader = open(&output);
    let chain = reader.revisions().unwrap();
    assert!(
        reader
            .open_revision(&chain.revisions[0], IndexedReaderOptions::default())
            .is_ok()
    );
    let mut made_up = chain.revisions[0].clone();
    let cut = find_last(&original, b"startxref") as u64;
    made_up.range = Some(0..cut);
    assert!(matches!(
        reader.open_revision(&made_up, IndexedReaderOptions::default()),
        Err(IndexedReaderError::RevisionRangeUnavailable { index: 0 })
    ));
}

fn find_last(bytes: &[u8], pattern: &[u8]) -> usize {
    bytes
        .windows(pattern.len())
        .rposition(|window| window == pattern)
        .unwrap()
}

/// A signed range changing, or the signature no longer being traceable to a whole revision. The
/// signature here is a stand-in with the real layout: nothing is checked but the bytes.
#[test]
fn a_signed_range_is_unchanged_and_named_by_its_revision() {
    let mut pdf = Pdf::base();
    let placeholder = "0".repeat(64);
    let dict_at = pdf.bytes.len();
    pdf.obj(
        6,
        &format!("<< /Type /Sig /ByteRange [0 0000000000 0000000000 0000000000] /Contents <{placeholder}> >>"),
    );
    let trailer = "<< /Size 7 /Root 1 0 R /Info 4 0 R >>";
    let xref = pdf.table(trailer);
    let mut bytes = pdf.finish(xref);
    let hole_start = dict_at + find_last(&bytes[dict_at..], b"/Contents <") + b"/Contents ".len();
    let hole_end = hole_start + placeholder.len() + 2;
    let byte_range = format!("[0 {hole_start:010} {hole_end:010} {:010}]", bytes.len() - hole_end);
    let range_at = dict_at + find_last(&bytes[dict_at..], b"/ByteRange ") + b"/ByteRange ".len();
    bytes[range_at..range_at + byte_range.len()].copy_from_slice(byte_range.as_bytes());

    let signed_first = bytes[..hole_start].to_vec();
    let signed_second = bytes[hole_end..].to_vec();
    let reader = open(&bytes);
    let (output, summary, _) = small_update(&reader);
    assert_eq!(output[..hole_start], signed_first);
    assert_eq!(output[hole_end..bytes.len()], signed_second);
    assert_eq!(output[hole_start..hole_end], bytes[hole_start..hole_end]);
    assert_eq!(summary.previous_len, bytes.len() as u64);

    let chain = open(&output).revisions().unwrap();
    let covered = chain
        .revision_ending_at(bytes.len() as u64)
        .expect("the signature ends at a revision");
    assert_eq!(covered.index, 0);
    assert_eq!(chain.revisions.len(), 2);
    let mut copy = Vec::new();
    open(&output).copy_revision_to(covered, &mut copy).unwrap();
    assert_eq!(copy, bytes);
}

// ---- The atomic path. ----

#[cfg(any(unix, windows))]
mod atomic {
    use super::*;
    use lopdf::{AppendDestination, AppendOptions, AppendedFile, FileSource, append_atomically};
    use std::path::Path;

    fn folder() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn entries(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn opened(path: &Path) -> IndexedReader {
        IndexedReader::open(FileSource::open(path).unwrap()).unwrap()
    }

    fn add_one(writer: &mut AppendWriter<'_, lopdf::TemporaryOutput<'_>>) -> lopdf::AppendResult<()> {
        writer.add_object(&Object::Integer(9))?;
        Ok(())
    }

    /// A file in `folder` holding `bytes`.
    fn file(folder: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = folder.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// An update replacing the file it read, with a reader still open on the old one: the classic case of
    /// saving over yourself. The old reader has to keep answering, and the new file has to be whole.
    #[test]
    fn it_replaces_the_file_the_reader_has_open() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);
        let AppendedFile {
            summary,
            reader: reopened,
            ..
        } = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.starts_with(&bytes));
        assert_eq!(on_disk.len() as u64, summary.new_len);
        assert_eq!(entries(dir.path()), ["doc.pdf"], "the temporary file was left behind");
        assert_eq!(
            reopened.resolve_object(summary.objects[0].id).unwrap(),
            Object::Integer(9)
        );
        assert_eq!(reader.object_ids().len(), 5, "the old reader lost its view");
        assert!(reader.resolve_object((1, 0)).is_ok(), "the old reader stopped reading");
        assert!(reader.resolve_object(summary.objects[0].id).is_err());
    }

    /// The returned reader not being usable for the next save, which would turn every second Save into a
    /// false "the file changed".
    #[test]
    fn the_returned_reader_makes_the_next_save() {
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        let mut reader = opened(&path);
        for round in 1..=3 {
            let next = append_atomically(
                &reader,
                AppendDestination::SameFile(&path),
                AppendOptions::new(),
                add_one,
            )
            .unwrap_or_else(|error| panic!("save {round}: {error}"))
            .reader;
            reader = next;
        }
        assert_eq!(opened(&path).revisions().unwrap().revisions.len(), 4);
    }

    /// A failed save leaving the target changed, or a partial temporary file behind. "Save never leaves
    /// a partly written file" is a promise from the spec.
    #[test]
    fn a_failed_build_leaves_everything_as_it_was() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);

        let refused = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            |writer| {
                writer.add_object(&Object::Integer(1))?;
                writer.write_object((99, 0), &Object::Integer(2))?;
                Ok(())
            },
        );
        assert!(matches!(refused, Err(AppendError::UnknownObject { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(dir.path()), ["doc.pdf"]);

        let empty = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            |_| Ok(()),
        );
        assert!(matches!(empty, Err(AppendError::EmptyUpdate)));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(dir.path()), ["doc.pdf"]);
    }

    /// Saving as a new file touching the original.
    #[test]
    fn save_as_leaves_the_original_alone() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let other = dir.path().join("copy.pdf");
        let reader = opened(&path);
        append_atomically(
            &reader,
            AppendDestination::NewFile(&other),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(std::fs::read(&other).unwrap().starts_with(&bytes));
        assert_eq!(
            entries(dir.path()),
            ["copy.pdf", "doc.pdf"],
            "the temporary name stayed behind"
        );
    }

    /// Another app saving by rename between open and Save. The save used to replace its newer file with old
    /// bytes plus the update and return Ok, which silently loses that app's work.
    #[test]
    fn a_save_is_refused_if_another_app_replaced_the_file() {
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        let reader = opened(&path);
        let mut newer = classic_pdf();
        newer.extend_from_slice(b"% another app's newer save\n");
        let side = file(dir.path(), "side.tmp", &newer);
        std::fs::rename(&side, &path).unwrap();

        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetChanged { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), newer, "the newer save was lost");
        assert_eq!(entries(dir.path()), ["doc.pdf"]);
    }

    /// A second save from the same old reader, which would throw away everything the first one added.
    #[test]
    fn a_second_save_from_the_old_reader_is_refused() {
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        let reader = opened(&path);
        append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        let after_first = std::fs::read(&path).unwrap();
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetChanged { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), after_first, "the first save was lost");
    }

    /// The file being replaced while the update is being written: only a check just before the rename sees it.
    #[test]
    fn a_file_replaced_during_the_write_is_refused() {
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        let reader = opened(&path);
        let newer = b"written while the save ran".to_vec();
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            |writer| {
                let side = file(path.parent().unwrap(), "side.tmp", &newer);
                std::fs::rename(&side, &path).unwrap();
                add_one(writer)
            },
        );
        assert!(matches!(result.err(), Some(AppendError::TargetChanged { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), newer);
        assert_eq!(entries(dir.path()), ["doc.pdf"]);
    }

    /// One document's save landing on an unrelated file, as passing another path used to do, with Ok. Replacing
    /// it has to be asked for by name.
    #[test]
    fn another_file_is_only_replaced_when_named_as_such() {
        let dir = folder();
        let bytes = classic_pdf();
        let a = file(dir.path(), "a.pdf", &bytes);
        let b = file(dir.path(), "b.pdf", b"an unrelated file");
        let reader = opened(&a);

        let same = append_atomically(&reader, AppendDestination::SameFile(&b), AppendOptions::new(), add_one);
        assert!(matches!(same.err(), Some(AppendError::TargetChanged { .. })));
        let new = append_atomically(&reader, AppendDestination::NewFile(&b), AppendOptions::new(), add_one);
        assert!(matches!(new.err(), Some(AppendError::TargetExists { .. })));
        assert_eq!(std::fs::read(&b).unwrap(), b"an unrelated file");

        append_atomically(
            &reader,
            AppendDestination::OverwriteOther(&b),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert!(std::fs::read(&b).unwrap().starts_with(&bytes));
        assert_eq!(std::fs::read(&a).unwrap(), bytes);
    }

    /// The file a person agreed to overwrite changing during the write: what they agreed to replace is gone.
    #[test]
    fn an_overwrite_target_changing_during_the_write_is_refused() {
        let dir = folder();
        let a = file(dir.path(), "a.pdf", &classic_pdf());
        let b = file(dir.path(), "b.pdf", b"the file they agreed to replace");
        let reader = opened(&a);
        let result = append_atomically(
            &reader,
            AppendDestination::OverwriteOther(&b),
            AppendOptions::new(),
            |writer| {
                std::fs::write(&b, b"something newer and longer than before").unwrap();
                add_one(writer)
            },
        );
        assert!(matches!(result.err(), Some(AppendError::TargetChanged { .. })));
        assert_eq!(std::fs::read(&b).unwrap(), b"something newer and longer than before");
    }

    /// A Save As landing on a file that appeared during the write, which a rename would replace.
    #[test]
    fn a_new_file_that_appears_during_the_write_is_kept() {
        let dir = folder();
        let a = file(dir.path(), "a.pdf", &classic_pdf());
        let b = dir.path().join("b.pdf");
        let reader = opened(&a);
        let result = append_atomically(
            &reader,
            AppendDestination::NewFile(&b),
            AppendOptions::new(),
            |writer| {
                std::fs::write(&b, b"appeared meanwhile").unwrap();
                add_one(writer)
            },
        );
        assert!(matches!(result.err(), Some(AppendError::TargetExists { .. })));
        assert_eq!(std::fs::read(&b).unwrap(), b"appeared meanwhile");
        assert_eq!(entries(dir.path()), ["a.pdf", "b.pdf"]);
    }

    /// A save to a path whose file has gone: the person may have moved it on purpose, so nothing is recreated.
    #[test]
    fn a_save_to_a_deleted_file_is_refused() {
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        let reader = opened(&path);
        std::fs::remove_file(&path).unwrap();
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetChanged { .. })));
        assert!(entries(dir.path()).is_empty());
    }

    /// A reader of bytes in memory, or of an earlier revision, overwriting a file it can't prove it came from.
    /// From an earlier revision, that would throw away every later one.
    #[test]
    fn a_reader_with_no_file_of_its_own_cant_save_in_place() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let in_memory = open(&bytes);
        let result = append_atomically(
            &in_memory,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetIdentityUnknown)));

        let newer = append_atomically(
            &opened(&path),
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap()
        .reader;
        let chain = newer.revisions().unwrap();
        let earlier = newer
            .open_revision(&chain.revisions[0], IndexedReaderOptions::default())
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let result = append_atomically(
            &earlier,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetIdentityUnknown)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A save through a symbolic link replacing the link: the real file stayed old, the link pointed at nothing of
    /// the document's history, and the save said Ok.
    #[cfg(unix)]
    #[test]
    fn a_save_through_a_symbolic_link_updates_the_real_file() {
        let dir = folder();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let bytes = classic_pdf();
        let real = file(&elsewhere, "real.pdf", &bytes);
        let link = dir.path().join("link.pdf");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let reader = opened(&link);

        let summary = append_atomically(
            &reader,
            AppendDestination::SameFile(&link),
            AppendOptions::new(),
            add_one,
        )
        .unwrap()
        .summary;
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link was replaced"
        );
        let on_disk = std::fs::read(&real).unwrap();
        assert_eq!(
            on_disk.len() as u64,
            summary.new_len,
            "the real file didn't get the update"
        );
        assert_eq!(entries(&elsewhere), ["real.pdf"]);
        assert_eq!(entries(dir.path()), ["elsewhere", "link.pdf"]);
    }

    /// A link to nothing being replaced by a file, or followed to a location nobody chose.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symbolic_link_is_refused_every_way() {
        let dir = folder();
        let source = file(dir.path(), "doc.pdf", &classic_pdf());
        let link = dir.path().join("link.pdf");
        std::os::unix::fs::symlink(dir.path().join("missing.pdf"), &link).unwrap();
        let reader = opened(&source);

        let new = append_atomically(
            &reader,
            AppendDestination::NewFile(&link),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(new.err(), Some(AppendError::TargetExists { .. })));
        let same = append_atomically(
            &reader,
            AppendDestination::SameFile(&link),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(same.err(), Some(AppendError::TargetChanged { .. })));
        let other = append_atomically(
            &reader,
            AppendDestination::OverwriteOther(&link),
            AppendOptions::new(),
            add_one,
        );
        assert!(other.is_err());
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(entries(dir.path()), ["doc.pdf", "link.pdf"]);
    }

    /// A read-only file being replaced as readily as any other, since a rename needs only write access to its folder.
    #[cfg(unix)]
    #[test]
    fn a_read_only_target_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let reader = opened(&path);
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetReadOnly { .. })));

        let source = file(dir.path(), "other.pdf", &bytes);
        let reader = opened(&source);
        let result = append_atomically(
            &reader,
            AppendDestination::OverwriteOther(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetReadOnly { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(dir.path()), ["doc.pdf", "other.pdf"]);
    }

    /// A hard link silently splitting from the document: its other name would keep the old bytes for good.
    #[cfg(unix)]
    #[test]
    fn a_target_with_hard_links_is_refused_unless_asked_for() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let twin = dir.path().join("twin.pdf");
        std::fs::hard_link(&path, &twin).unwrap();
        let reader = opened(&path);

        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(
            result.err(),
            Some(AppendError::TargetHasHardLinks { links: 2, .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        let options = AppendOptions::new().allow_breaking_hard_links();
        append_atomically(&reader, AppendDestination::SameFile(&path), options, add_one).unwrap();
        assert!(std::fs::read(&path).unwrap().len() > bytes.len());
        assert_eq!(std::fs::read(&twin).unwrap(), bytes);
    }

    /// A crash after a Save As leaving our temporary name as a second link. Every later save failed with `TargetHasHardLinks`, naming a hidden
    /// file the person never made, and nothing would ever clear it. Other links, including another document's temporary name, still count.
    #[cfg(unix)]
    #[test]
    fn our_own_leftover_link_doesnt_count_as_a_hard_link() {
        use std::os::unix::fs::MetadataExt;
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);

        let seen = std::cell::RefCell::new(String::new());
        let options = AppendOptions::new().prepare_replacement(|temporary, _| {
            *seen.borrow_mut() = temporary_name(temporary);
            Ok(())
        });
        let reader = append_atomically(&opened(&path), AppendDestination::SameFile(&path), options, add_one)
            .unwrap()
            .reader;
        let name = seen.into_inner();
        let split = name[".append-".len()..].find('-').unwrap();
        let prefix = name[..".append-".len() + split + 1].to_string();

        // What a crash leaves, fresh and from a live process: nothing about its age or owner may matter.
        let leftover = dir.path().join(format!("{prefix}{}-0.tmp", std::process::id()));
        std::fs::hard_link(&path, &leftover).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);
        let reader = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .expect("our own leftover blocked the save")
        .reader;
        assert!(!leftover.exists());

        // Beside a link somebody did make: the leftover goes, and the real one still refuses.
        std::fs::hard_link(&path, &leftover).unwrap();
        let theirs = dir.path().join("theirs.pdf");
        std::fs::hard_link(&path, &theirs).unwrap();
        let other_document = dir
            .path()
            .join(format!(".append-0123456789abcdef-{}-0.tmp", std::process::id()));
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(
            result.err(),
            Some(AppendError::TargetHasHardLinks { links: 2, .. })
        ));
        assert!(!leftover.exists());
        assert!(theirs.exists());
        std::fs::remove_file(&theirs).unwrap();
        std::fs::hard_link(&path, &other_document).unwrap();
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(
            result.err(),
            Some(AppendError::TargetHasHardLinks { links: 2, .. })
        ));
        assert!(
            other_document.exists(),
            "another name's temporary file isn't ours to remove"
        );
    }

    /// A folder, or anything else that isn't a file, reaching the rename.
    #[test]
    fn a_target_that_isnt_a_file_is_refused() {
        let dir = folder();
        let source = file(dir.path(), "doc.pdf", &classic_pdf());
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let reader = opened(&source);
        let result = append_atomically(
            &reader,
            AppendDestination::OverwriteOther(&inner),
            AppendOptions::new(),
            add_one,
        );
        assert!(matches!(result.err(), Some(AppendError::TargetNotAFile { .. })));
    }

    /// The step running too early, on an unfinished file, or after the replacement, when it's too late to fail.
    #[test]
    fn the_prepare_step_gets_the_finished_file_before_it_replaces_anything() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);
        let seen = std::cell::Cell::new(0_u64);
        let options = AppendOptions::new().prepare_replacement(|temporary, target| {
            let finished = std::fs::read(temporary)?;
            assert!(finished.starts_with(&bytes));
            assert!(
                finished.windows(5).rev().take(8).any(|window| window == b"%%EOF"),
                "the file isn't finished"
            );
            assert_eq!(std::fs::read(target)?, bytes, "the target was replaced before the step");
            seen.set(finished.len() as u64);
            Ok(())
        });
        let summary = append_atomically(&reader, AppendDestination::SameFile(&path), options, add_one)
            .unwrap()
            .summary;
        assert_eq!(seen.get(), summary.new_len);
    }

    /// A failed attribute copy replacing the file anyway: the person would lose its tags with no warning.
    #[test]
    fn a_failing_prepare_step_leaves_everything_as_it_was() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);
        let options = AppendOptions::new().prepare_replacement(|_, _| Err(std::io::Error::other("no attributes")));
        let result = append_atomically(&reader, AppendDestination::SameFile(&path), options, add_one);
        assert!(matches!(result.err(), Some(AppendError::PrepareReplacement(_))));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(dir.path()), ["doc.pdf"]);
    }

    /// A prepare step that damages the bytes passing, because the check ran before it.
    #[test]
    fn a_prepare_step_that_damages_the_file_fails_the_save() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);
        let options = AppendOptions::new().prepare_replacement(|temporary, _| {
            let mut damaged = std::fs::read(temporary)?;
            damaged[20] ^= 1;
            std::fs::write(temporary, damaged)
        });
        let result = append_atomically(&reader, AppendDestination::SameFile(&path), options, add_one);
        assert!(matches!(result.err(), Some(AppendError::VerificationFailed { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// A prepare step changing a stream's data without changing its length, which the doc says fails the save and didn't.
    #[test]
    fn a_prepare_step_that_changes_a_written_stream_fails_the_save() {
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let reader = opened(&path);
        let options = AppendOptions::new().prepare_replacement(|temporary, _| {
            let mut changed = std::fs::read(temporary)?;
            let at = changed.windows(8).rposition(|window| window == b"the data").unwrap();
            changed[at] ^= 1;
            std::fs::write(temporary, changed)
        });
        let result = append_atomically(&reader, AppendDestination::SameFile(&path), options, |writer| {
            writer.add_object(&Object::Stream(Stream::new(Dictionary::new(), b"the data".to_vec())))?;
            Ok(())
        });
        assert!(matches!(result.err(), Some(AppendError::VerificationFailed { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// The docs being wrong in either direction: a Save keeps attributes, as it starts from a clone; a new file loses them without the step; and the step can keep them.
    /// Uses the real `xattr` tool and a real quarantine-shaped attribute, on the real file system.
    #[cfg(target_os = "macos")]
    #[test]
    fn extended_attributes_go_without_the_step_and_stay_with_it() {
        use std::process::Command;
        let attribute = "com.example.append-test";
        let get = |path: &Path| {
            let output = Command::new("/usr/bin/xattr")
                .args(["-p", attribute])
                .arg(path)
                .output()
                .unwrap();
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        };
        let set = |path: &Path, value: &str| {
            let status = Command::new("/usr/bin/xattr")
                .args(["-w", attribute, value])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        };
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        set(&path, "tagged");
        let reader = opened(&path);

        let reader = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap()
        .reader;
        assert_eq!(
            get(&path).as_deref(),
            Some("tagged"),
            "a Save starts from a clone of the file, which keeps its attributes"
        );
        let copy = dir.path().join("copy.pdf");
        append_atomically(
            &reader,
            AppendDestination::NewFile(&copy),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert_eq!(
            get(&copy),
            None,
            "a Save As is a new file, and the docs say an attribute is lost without the step"
        );

        set(&path, "tagged again");
        let options = AppendOptions::new().prepare_replacement(|temporary, target| {
            let value = get(target).expect("the target has the attribute");
            set(temporary, &value);
            Ok(())
        });
        let result = append_atomically(&reader, AppendDestination::SameFile(&path), options, add_one);
        // Setting an attribute changes the file's status time and not its modification time, so the save holds.
        result.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(get(&path).as_deref(), Some("tagged again"));
    }

    /// The temporary file being written: a Save's is the clone in its own private folder, and anything else's is a file beside the target.
    fn temporary_in(folder: &Path) -> Option<std::path::PathBuf> {
        let found = std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(".append-"))?;
        Some(if found.is_dir() { found.join("clone") } else { found })
    }

    /// The name that says whose a temporary file is: a Save's clone goes by its folder's.
    fn temporary_name(temporary: &Path) -> String {
        let named = if temporary.file_name().is_some_and(|name| name == "clone") {
            temporary.parent().unwrap()
        } else {
            temporary
        };
        named.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// A private document's full copy sitting readable by others during the write, and after a crash. It used to be created
    /// with the default mode, 0644 under the usual umask, and only tightened just before the rename.
    #[cfg(unix)]
    #[test]
    fn the_temporary_file_is_never_more_readable_than_the_document() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let reader = opened(&path);
        let reader = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            |writer| {
                let temporary = temporary_in(path.parent().unwrap()).expect("a temporary file while writing");
                assert_eq!(mode_of(&temporary), 0o600, "readable by others during the write");
                add_one(writer)
            },
        )
        .unwrap()
        .reader;
        assert_eq!(mode_of(&path), 0o600);

        // A Save As of a private document stays private, with nothing at the target to take a mode from.
        let copy = dir.path().join("copy.pdf");
        append_atomically(
            &reader,
            AppendDestination::NewFile(&copy),
            AppendOptions::new(),
            |writer| {
                let temporary = temporary_in(copy.parent().unwrap()).expect("a temporary file while writing");
                assert_eq!(
                    mode_of(&temporary),
                    0o600,
                    "a private document's copy was readable during the write"
                );
                add_one(writer)
            },
        )
        .unwrap();
        assert_eq!(mode_of(&copy), 0o600, "a private document's copy came out readable");

        // With no file to take a mode from at all, private is the safe answer.
        let memory = dir.path().join("from-memory.pdf");
        append_atomically(
            &open(&classic_pdf()),
            AppendDestination::NewFile(&memory),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert_eq!(mode_of(&memory), 0o600);
    }

    /// A mode the umask would narrow, group write here, being lost on save. Creation is narrowed by the umask, and the file
    /// has to end with exactly the mode it had.
    #[cfg(unix)]
    #[test]
    fn the_exact_mode_comes_back_after_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder();
        let path = file(dir.path(), "doc.pdf", &classic_pdf());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let reader = opened(&path);
        append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o666);
    }

    /// A long file name failing to save: the temporary name was the file's name and more, so a name near the limit of 255
    /// bytes gave ENAMETOOLONG.
    #[test]
    fn a_very_long_file_name_saves() {
        let dir = folder();
        let name = format!("{}.pdf", "n".repeat(250));
        let path = file(dir.path(), &name, &classic_pdf());
        let reader = opened(&path);
        let reader = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap()
        .reader;
        let other = dir.path().join(format!("{}.pdf", "m".repeat(250)));
        append_atomically(
            &reader,
            AppendDestination::NewFile(&other),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        assert_eq!(entries(dir.path()).len(), 2);
    }

    /// A crash's leftover staying forever, or, far worse, the sweep touching any other file. Every near miss has to survive:
    /// another document's temporary, a process still alive, a fresh file, a wrong suffix, a folder and a link.
    #[cfg(unix)]
    #[test]
    fn a_dead_processs_temporary_is_swept_and_nothing_else_is() {
        use std::time::{Duration, SystemTime};
        let dir = folder();
        let bytes = classic_pdf();
        let path = file(dir.path(), "doc.pdf", &bytes);
        let other_doc = file(dir.path(), "other.pdf", &bytes);

        // The real prefixes, from real saves.
        let prefix_of = |target: &Path| {
            let seen = std::cell::RefCell::new(String::new());
            let options = AppendOptions::new().prepare_replacement(|temporary, _| {
                *seen.borrow_mut() = temporary_name(temporary);
                Ok(())
            });
            append_atomically(&opened(target), AppendDestination::SameFile(target), options, add_one).unwrap();
            let name = seen.into_inner();
            let split = name[".append-".len()..].find('-').unwrap();
            name[..".append-".len() + split + 1].to_string()
        };
        let prefix = prefix_of(&path);
        let other_prefix = prefix_of(&other_doc);
        assert_ne!(prefix, other_prefix);
        assert!(prefix.len() < 32, "a temporary name has to be short: {prefix}");

        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let dead = child.id();
        // A process certainly alive, and pid 1, which is alive but belongs to another user.
        let mut sleeper = std::process::Command::new("/bin/sleep").arg("120").spawn().unwrap();
        let alive = sleeper.id();
        let old = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        let put = |name: String, aged: bool| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"leftover").unwrap();
            if aged {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        };

        let stale = format!("{prefix}{dead}-0.tmp");
        put(stale.clone(), true);
        let mut kept = vec![
            format!("{other_prefix}{dead}-0.tmp"),
            format!("{prefix}{alive}-0.tmp"),
            format!("{prefix}1-0.tmp"),
            format!("{prefix}0-0.tmp"),
            format!("{prefix}4294967297-0.tmp"),
            format!("{prefix}{dead}-0.tmp2"),
            format!("{prefix}{dead}-x.tmp"),
            format!("{prefix}x-0.tmp"),
            format!("{prefix}{dead}.tmp"),
            format!("{prefix}{dead}-0-0.tmp"),
            format!("x{prefix}{dead}-0.tmp"),
            format!(".append-{dead}-0.tmp"),
            "doc.pdf.tmp".to_string(),
        ];
        for name in &kept {
            put(name.clone(), true);
        }
        let fresh = format!("{prefix}{dead}-1.tmp");
        put(fresh.clone(), false);
        kept.push(fresh);
        let inner = format!("{prefix}{dead}-2.tmp");
        std::fs::create_dir(dir.path().join(&inner)).unwrap();
        kept.push(inner);
        let link = format!("{prefix}{dead}-3.tmp");
        std::os::unix::fs::symlink(&other_doc, dir.path().join(&link)).unwrap();
        kept.push(link);

        append_atomically(
            &opened(&path),
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        )
        .unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();

        let now = entries(dir.path());
        assert!(!now.contains(&stale), "the dead process's temporary is still there");
        for name in &kept {
            assert!(now.contains(name), "the sweep removed {name}");
        }
        assert!(now.contains(&"doc.pdf".to_string()) && now.contains(&"other.pdf".to_string()));
        assert!(std::fs::read(&other_doc).unwrap().starts_with(&bytes));
    }

    /// A failed folder sync reporting a failed save, after the rename already replaced the file. The person would think
    /// their save was lost and might redo it. A folder that can be written and entered but not read makes it real.
    #[cfg(unix)]
    #[test]
    fn a_failed_folder_sync_after_the_rename_is_a_warning_not_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let bytes = classic_pdf();
        let path = file(&inner, "doc.pdf", &bytes);
        let reader = opened(&path);
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o300)).unwrap();
        let unreadable = std::fs::File::open(&inner).is_err();
        let result = append_atomically(
            &reader,
            AppendDestination::SameFile(&path),
            AppendOptions::new(),
            add_one,
        );
        let on_disk = std::fs::read(&path);
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let saved = result.unwrap_or_else(|error| panic!("the file was replaced and this says it failed: {error}"));
        assert!(
            on_disk.unwrap().len() as u64 == saved.summary.new_len,
            "the file wasn't replaced"
        );
        // Running as root, the folder can be read after all, and there's nothing to warn about.
        assert_eq!(saved.durability_warning.is_some(), unreadable);
    }

    /// A Save As of a read-only document failing in the attribute step, because the temporary file was read-only from the moment it was made, and
    /// macOS refuses `setxattr` on one. The step gets a file it can change, and the document's own mode comes back afterwards.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_prepare_step_works_on_a_copy_of_a_read_only_document() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let dir = folder();
        let source = file(dir.path(), "read-only.pdf", &classic_pdf());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
        let copy = dir.path().join("copy.pdf");
        let attribute = "com.example.append-test";
        let options = AppendOptions::new().prepare_replacement(|temporary, _| {
            let output = Command::new("/usr/bin/xattr")
                .args(["-w", attribute, "kept"])
                .arg(temporary)
                .output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ))
            }
        });
        let reader = opened(&source);
        append_atomically(&reader, AppendDestination::NewFile(&copy), options, add_one)
            .unwrap_or_else(|error| panic!("{error}: {:?}", std::error::Error::source(&error)));
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o444,
            "the document's mode didn't come back"
        );
        let read = Command::new("/usr/bin/xattr")
            .args(["-p", attribute])
            .arg(&copy)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "kept");
    }

    /// An update starting from a stale view: the file changed after the reader opened it, so the
    /// offsets it would write are wrong for what's there now.
    #[test]
    fn a_file_changed_under_the_reader_is_refused() {
        let dir = folder();
        let path = dir.path().join("doc.pdf");
        std::fs::write(&path, classic_pdf()).unwrap();
        let reader = IndexedReader::open(FileSource::open(&path).unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"more\n").unwrap();
        drop(file);
        let result = AppendWriter::new(&reader, Vec::new());
        assert!(matches!(
            result.err(),
            Some(AppendError::Source(SourceError::SourceChanged))
        ));
    }

    /// The verifier passing a file it should fail: one flipped byte in the original part, a bad length, or a
    /// lying summary.
    #[test]
    fn the_verifier_catches_a_damaged_result() {
        let bytes = classic_pdf();
        let reader = open(&bytes);
        let (output, summary, _) = small_update(&reader);
        assert!(
            reader
                .verify_appended(Arc::new(BytesSource::from(output.clone())), &summary)
                .is_ok()
        );

        let mut flipped = output.clone();
        flipped[20] ^= 1;
        let result = reader.verify_appended(Arc::new(BytesSource::from(flipped)), &summary);
        assert!(
            matches!(result.err(), Some(AppendError::VerificationFailed { reason }) if reason.contains("original"))
        );

        let mut longer = output.clone();
        longer.push(b'\n');
        assert!(matches!(
            reader
                .verify_appended(Arc::new(BytesSource::from(longer)), &summary)
                .err(),
            Some(AppendError::VerificationFailed { .. })
        ));

        let mut lying = summary.clone();
        lying.objects[0].offset += 1;
        assert!(matches!(
            reader
                .verify_appended(Arc::new(BytesSource::from(output)), &lying)
                .err(),
            Some(AppendError::VerificationFailed { .. })
        ));
    }
}

/// A small generator, so a failing seed fails the same way everywhere.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}
