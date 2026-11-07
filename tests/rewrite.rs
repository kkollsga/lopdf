//! The streaming rewrite: what's gone, what stays, where it goes, and what it refuses. Every test says in its doc comment what
//! failure it catches.

use lopdf::{
    AppendDestination, AppendError, BytesSource, Dictionary, FileSource, IndexedReader, IndexedSignatureKind, Object,
    RewriteChanges, RewriteError, RewriteForm, RewriteFormChoice, RewriteOptions, RewritePolicy, RewriteSummary, Stream,
    dictionary, rewrite_atomically,
};
use std::path::Path;

mod fixture {
    #![allow(dead_code)]
    include!("common/rewrite_fixture.rs");
}
mod property {
    include!("common/rewrite_property.rs");
}
use fixture::*;

// ---- A document with something to find in every place something can hide. ----

/// Everything that has to be gone once page 2 is.
const GONE: [&str; 27] = [
    "M_CONTENT",
    "M_IMAGE",
    "M_THUMB",
    "M_PAGEMETA",
    "M_LINK",
    "M_ANNOT",
    "M_AP",
    "M_POPUP",
    "M_FIELD_VALUE",
    "M_FIELD_PARENT",
    "M_WIDGET_AP",
    "M_XFA",
    "M_BOOK_TITLE",
    "M_BOOK_CHILD",
    "M_BOOK_NAMED",
    "M_HEADING",
    "M_STALE_OUTLINE",
    "M_DESTNAME",
    "M_DEST_ONLY",
    "M_OLDDEST",
    "M_ACTUAL",
    "M_ALT",
    "M_IDKEY",
    "M_LAYER",
    "M_THREAD",
    "M_LABEL",
    "M_ORDER_LABEL_NOT_THIS",
];

/// What only an old revision, an unreferenced object or the space around objects holds: gone from any rewrite.
const HIDDEN: [&str; 8] = [
    "M_OLD_INFO",
    "M_PREV_ONLY",
    "M_FREED",
    "M_UNREFERENCED",
    "M_UNREF_STREAM",
    "M_COMMENT",
    "M_GAP",
    "M_TRAILER",
];

/// What has to still be there.
const KEPT: [&str; 13] = [
    "K_PAGE_ONE",
    "K_PAGE_THREE",
    "K_FORM_KEEP",
    "K_FORM_SHARED",
    "K_LINK",
    "K_FIELD",
    "K_BOOK_ONE",
    "K_CHILD",
    "K_ACTUAL",
    "K_LAYER",
    "K_THREAD",
    "K_INFO",
    "K_FONT",
];

fn rich_objects() -> Vec<Obj> {
    let page = |n: u32, rest: &str| obj(n, &format!("<< /Type /Page /Parent 2 0 R {rest} >>"));
    let widget = |n: u32, rest: &str| {
        obj(
            n,
            &format!("<< /Type /Annot /Subtype /Widget /Rect [0 0 10 10] {rest} >>"),
        )
    };
    vec![
        obj(
            2,
            "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 /MediaBox [0 0 200 200] /Resources 6 0 R >>",
        ),
        obj(
            6,
            "<< /Font << /F1 7 0 R >> /XObject << /Keep 10 0 R /Shared 11 0 R /Gone 12 0 R >> \
             /Properties << /KeepOC 60 0 R /GoneOC 61 0 R >> >>",
        ),
        obj(7, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Note (K_FONT) >>"),
        page(3, "/Contents 13 0 R /Annots [14 0 R 15 0 R] /StructParents 0"),
        page(
            4,
            "/Contents 16 0 R /Thumb 17 0 R /Metadata 18 0 R /Annots [19 0 R 41 0 R 46 0 R 48 0 R] /StructParents 1 /B [72 0 R 74 0 R]",
        ),
        page(5, "/Contents 29 0 R /Annots [42 0 R]"),
        deflated(
            13,
            "",
            b"/Keep Do /Shared Do BT /F1 12 Tf 20 100 Td (K_PAGE_ONE) Tj ET /OC /KeepOC BDC EMC",
        ),
        deflated(
            16,
            "",
            b"BT /F1 12 Tf 20 100 Td (M_CONTENT) Tj ET /Shared Do /Gone Do /OC /GoneOC BDC EMC",
        ),
        deflated(29, "", b"BT /F1 12 Tf 20 100 Td (K_PAGE_THREE) Tj ET"),
        deflated(
            10,
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50]",
            b"0 0 1 rg 0 0 50 50 re f % K_FORM_KEEP",
        ),
        deflated(
            11,
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50]",
            b"1 0 0 rg 60 60 20 20 re f % K_FORM_SHARED",
        ),
        deflated(
            12,
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50] /Note 93 0 R",
            b"0 1 0 rg 0 0 50 50 re f % M_IMAGE",
        ),
        deflated(17, "", b"M_THUMB"),
        deflated(18, "/Type /Metadata /Subtype /XML", b"M_PAGEMETA"),
        obj(
            14,
            "<< /Type /Annot /Subtype /Link /Rect [0 0 9 9] /Dest [4 0 R /Fit] /Contents (M_LINK) >>",
        ),
        obj(
            15,
            "<< /Type /Annot /Subtype /Link /Rect [0 0 9 9] /Dest [5 0 R /Fit] /Contents (K_LINK) >>",
        ),
        obj(
            19,
            "<< /Type /Annot /Subtype /Text /Rect [0 0 9 9] /Contents (M_ANNOT) /P 4 0 R /AP << /N 21 0 R >> /Popup 22 0 R /StructParent 7 >>",
        ),
        deflated(21, "/Type /XObject /Subtype /Form /BBox [0 0 9 9]", b"% M_AP"),
        obj(
            22,
            "<< /Type /Annot /Subtype /Popup /Rect [0 0 9 9] /Parent 19 0 R /P 4 0 R /Contents (M_POPUP) >>",
        ),
        // A field that is its widget, on the removed page.
        widget(41, "/FT /Tx /T (only) /V (M_FIELD_VALUE) /P 4 0 R"),
        // A field with widgets on both.
        obj(43, "<< /FT /Tx /T (both) /V (K_FIELD) /Kids [42 0 R 46 0 R] >>"),
        widget(42, "/Parent 43 0 R /P 5 0 R"),
        widget(46, "/Parent 43 0 R /P 4 0 R /AP << /N 47 0 R >>"),
        deflated(47, "/Type /XObject /Subtype /Form /BBox [0 0 9 9]", b"% M_WIDGET_AP"),
        // A field with its only widget on the removed page.
        obj(44, "<< /FT /Tx /T (M_FIELD_PARENT) /Kids [48 0 R] >>"),
        widget(48, "/Parent 44 0 R /P 4 0 R"),
        obj(40, "<< /Fields [41 0 R 43 0 R 44 0 R] /CO [41 0 R] /XFA 45 0 R >>"),
        deflated(45, "", b"<xdp>M_XFA</xdp>"),
        // Bookmarks: one to a kept page, one to the removed page with a child each way, a heading over nothing that stays.
        obj(20, "<< /Type /Outlines /First 23 0 R /Last 25 0 R /Count 6 >>"),
        obj(
            23,
            "<< /Title (K_BOOK_ONE) /Parent 20 0 R /Next 24 0 R /Dest [3 0 R /Fit] >>",
        ),
        obj(
            24,
            "<< /Title (M_BOOK_TITLE) /Parent 20 0 R /Prev 23 0 R /Next 25 0 R /First 26 0 R /Last 27 0 R /Count 2 /Dest [4 0 R /Fit] /Custom 93 0 R >>",
        ),
        obj(93, "<< /Stale (M_STALE_OUTLINE) >>"),
        obj(
            26,
            "<< /Title (K_CHILD) /Parent 24 0 R /Next 27 0 R /Dest [5 0 R /Fit] >>",
        ),
        obj(
            27,
            "<< /Title (M_BOOK_NAMED) /Parent 24 0 R /Prev 26 0 R /Dest (M_DESTNAME) >>",
        ),
        obj(
            25,
            "<< /Title (M_HEADING) /Parent 20 0 R /Prev 24 0 R /First 28 0 R /Last 28 0 R /Count -1 >>",
        ),
        obj(
            28,
            "<< /Title (M_BOOK_CHILD) /Parent 25 0 R /A << /S /GoTo /D [4 0 R /XYZ 0 0 0] >> >>",
        ),
        // Named destinations, both ways.
        obj(30, "<< /Dests 31 0 R >>"),
        obj(31, "<< /Kids [32 0 R 34 0 R] >>"),
        obj(
            32,
            "<< /Limits [(A_keep) (M_DESTNAME)] /Names [(A_keep) [3 0 R /Fit] (M_DESTNAME) [4 0 R /Fit]] >>",
        ),
        obj(34, "<< /Limits [(M_DEST_ONLY) (M_DEST_ONLY)] /Names [(M_DEST_ONLY) 35 0 R] >>"),
        obj(35, "<< /D [4 0 R /Fit] >>"),
        obj(33, "<< /OldKeep [5 0 R /Fit] /M_OLDDEST [4 0 R /Fit] >>"),
        // Tagged content, on one page, on the other, and over both.
        obj(
            50,
            "<< /Type /StructTreeRoot /K [51 0 R 52 0 R 53 0 R] /ParentTree 54 0 R /IDTree 55 0 R /ParentTreeNextKey 8 >>",
        ),
        obj(51, "<< /S /P /P 50 0 R /Pg 3 0 R /K 0 /ActualText (K_ACTUAL) /ID (keepid) >>"),
        obj(52, "<< /S /P /P 50 0 R /Pg 4 0 R /K 0 /ActualText (M_ACTUAL) /ID (M_IDKEY) >>"),
        obj(
            53,
            "<< /S /Sect /P 50 0 R /K [ << /Type /MCR /Pg 5 0 R /MCID 0 >> << /Type /MCR /Pg 4 0 R /MCID 1 >> 56 0 R ] >>",
        ),
        obj(
            56,
            "<< /S /Figure /P 53 0 R /Pg 4 0 R /Alt (M_ALT) /K << /Type /OBJR /Obj 19 0 R /Pg 4 0 R >> >>",
        ),
        obj(
            54,
            "<< /Nums [0 [51 0 R] 1 [52 0 R 53 0 R] 2 [53 0 R] 7 56 0 R] >>",
        ),
        obj(55, "<< /Names [(M_IDKEY) 52 0 R (keepid) 51 0 R] >>"),
        // Layers: one used on a kept page, one only on the removed page.
        obj(60, "<< /Type /OCG /Name (K_LAYER) >>"),
        obj(61, "<< /Type /OCG /Name (M_LAYER) >>"),
        // Threads: one all on the removed page, one running through it.
        obj(70, "<< /Type /Thread /F 72 0 R /I << /Title (M_THREAD) >> >>"),
        obj(72, "<< /Type /Bead /T 70 0 R /N 72 0 R /V 72 0 R /P 4 0 R /R [0 0 9 9] >>"),
        obj(71, "<< /Type /Thread /F 73 0 R /I << /Title (K_THREAD) >> >>"),
        obj(73, "<< /Type /Bead /T 71 0 R /N 74 0 R /V 75 0 R /P 3 0 R /R [0 0 9 9] >>"),
        obj(74, "<< /Type /Bead /N 75 0 R /V 73 0 R /P 4 0 R /R [0 0 9 9] >>"),
        obj(75, "<< /Type /Bead /N 73 0 R /V 74 0 R /P 5 0 R /R [0 0 9 9] >>"),
        // Nothing refers to these, and in the compressed forms the first shares an object stream with what's live.
        obj(90, "<< /Secret (M_UNREFERENCED) >>"),
        deflated(91, "", b"M_UNREF_STREAM"),
    ]
}

/// What the newest revision's catalog says. `/Hidden` is only in the oldest.
fn rich_catalog(extra: &str) -> Obj {
    obj(
        1,
        &format!(
            "<< /Type /Catalog /Pages 2 0 R /Outlines 20 0 R /Names 30 0 R /Dests 33 0 R /AcroForm 40 0 R /StructTreeRoot 50 0 R \
             /PageLabels << /Nums [0 << /S /r >> 1 << /P (M_LABEL-) /S /D /St 40 >> 2 << /S /D /St 7 >>] >> \
             /OCProperties << /OCGs [60 0 R 61 0 R] /D << /ON [60 0 R 61 0 R] /Order [60 0 R (K_LAYER_HEAD) [61 0 R]] >> >> \
             /Threads [70 0 R 71 0 R] /OpenAction [4 0 R /Fit] % M_COMMENT\n {extra} >>"
        ),
    )
}

/// The rich document as `form` holds it, with an older revision behind it holding things nothing can reach any more.
fn rich_document(form: Form) -> Vec<u8> {
    let mut builder = Builder::new(200);
    let mut first = rich_objects();
    first.push(rich_catalog("/Hidden 92 0 R"));
    first.push(obj(92, "<< /Secret (M_PREV_ONLY) >>"));
    first.push(obj(94, "<< /Secret (M_FREED) >>"));
    first.push(obj(80, "<< /Title (M_OLD_INFO) >>"));
    // Everything in one object stream where that's possible, so that dead and live objects are neighbours.
    builder.place(form, &first, 1_000);
    builder.bytes.extend_from_slice(b"% M_GAP between objects\n");
    builder.finish_revision(form, "/Root 1 0 R /Info 80 0 R /ID [<AA> <BB>] /Stash (M_TRAILER)");
    let second = vec![rich_catalog(""), obj(80, "<< /Title (K_INFO) >>")];
    builder.free.push(94);
    builder.place(form, &second, 1_000);
    builder.finish_revision(form, "/Root 1 0 R /Info 80 0 R /ID [<AA> <BB>] /Stash (M_TRAILER)");
    builder.bytes
}

fn open_bytes(bytes: &[u8]) -> IndexedReader {
    IndexedReader::open(BytesSource::from(bytes.to_vec())).expect("the fixture opens")
}

fn rewrite_with(
    bytes: &[u8], policy: RewritePolicy, change: impl FnOnce(&mut RewriteChanges<'_, 'static>),
) -> (Vec<u8>, RewriteSummary, IndexedReader) {
    let reader = open_bytes(bytes);
    let mut changes = RewriteChanges::new(&reader).expect("this file can be rewritten");
    change(&mut changes);
    let mut output = Vec::new();
    let summary = changes.write_to(policy, &mut output).expect("the rewrite is written");
    assert_eq!(summary.new_len, output.len() as u64);
    let after = IndexedReader::open(BytesSource::from(output.clone())).expect("the result opens");
    (output, summary, after)
}

fn rewrite(bytes: &[u8], change: impl FnOnce(&mut RewriteChanges<'_, 'static>)) -> (Vec<u8>, RewriteSummary, IndexedReader) {
    rewrite_with(bytes, RewritePolicy::new(), change)
}

fn dict(reader: &IndexedReader, number: u32) -> Dictionary {
    reader
        .resolve_object((number, 0))
        .unwrap_or_else(|error| panic!("object {number}: {error}"))
        .as_dict()
        .unwrap_or_else(|_| panic!("object {number} isn't a dictionary"))
        .clone()
}

fn gone(reader: &IndexedReader, number: u32) -> bool {
    reader.object_location((number, 0)).is_none()
}

fn assert_markers(output: &[u8], gone: &[&str], kept: &[&str]) {
    let streams = every_decoded_stream(output);
    for marker in gone {
        assert!(
            !holds_marker(output, &streams, marker),
            "{marker} is still in the file"
        );
    }
    for marker in kept {
        assert!(holds_marker(output, &streams, marker), "{marker} went missing");
    }
}

fn remove_second_page(changes: &mut RewriteChanges<'_, 'static>) {
    changes.remove_page(1);
}

/// Saving the outputs for the validators, which aren't test dependencies.
fn dump(name: &str, bytes: &[u8]) {
    if let Some(folder) = std::env::var_os("REWRITE_DUMP_DIR") {
        std::fs::write(Path::new(&folder).join(name), bytes).unwrap();
    }
}

// ---- The main promise. ----

/// A removed page leaving anything behind, anywhere it can hide: its content, its image, its thumbnail, its metadata, annotations,
/// appearances, field values, bookmarks, destination names, structure text, layer names, thread titles or its label's prefix. Also what
/// was already dead before the rewrite: old revisions, unreferenced objects, freed objects, comments, what's between objects and
/// unknown trailer entries. Everything kept has to be there too, for a scan that finds nothing to mean something.
#[test]
fn nothing_of_a_removed_page_or_an_old_revision_stays_in_the_file() {
    for form in ALL_FORMS {
        let bytes = rich_document(form);
        let streams = every_decoded_stream(&bytes);
        for marker in GONE.iter().filter(|marker| **marker != "M_ORDER_LABEL_NOT_THIS").chain(&HIDDEN).chain(&KEPT) {
            assert!(holds_marker(&bytes, &streams, marker), "{marker} isn't in the {form:?} fixture");
        }
        let (output, summary, after) = rewrite(&bytes, remove_second_page);
        dump(&format!("rich_{form:?}.pdf"), &output);
        dump(&format!("rich_{form:?}.orig.pdf"), &bytes);
        assert_markers(&output, &GONE, &KEPT);
        assert_markers(&output, &HIDDEN, &[]);
        assert_eq!(after.page_count().unwrap(), 2, "{form:?}");
        assert_eq!(summary.removal.pages, vec![(4, 0)]);
        // What a viewer starts from has to be whole too.
        assert!(after.revisions().unwrap().revisions.len() == 1, "{form:?}: more than one revision");
        assert!(!after.index_stats().unwrap().recovered(), "{form:?}: it needed a rescan");
    }
}

/// An identity rewrite keeping what an old revision or an unreferenced object holds, because "copy what's reachable" was really "copy
/// what's in the index", which is where old objects, free entries and dead neighbours live.
#[test]
fn a_rewrite_with_no_changes_still_leaves_only_what_is_reachable() {
    for form in ALL_FORMS {
        let bytes = rich_document(form);
        let (output, summary, after) = rewrite(&bytes, |_| {});
        dump(&format!("identity_{form:?}.pdf"), &output);
        assert_markers(&output, &HIDDEN, &KEPT);
        // The removed page's things are all still there, since no page was removed.
        assert_markers(&output, &[], &["M_CONTENT", "M_ANNOT", "M_LAYER"]);
        assert_eq!(after.page_count().unwrap(), 3);
        assert!(summary.objects_dropped >= 2, "{form:?}: {}", summary.objects_dropped);
        assert!(summary.new_len < bytes.len() as u64, "{form:?}: {} from {}", summary.new_len, bytes.len());
    }
}

/// A replaced object's old version staying behind, as it would in an append, including as a neighbour in an object stream that was copied
/// whole for the sake of the objects that stayed.
#[test]
fn a_replaced_object_is_gone_too() {
    for form in ALL_FORMS {
        let mut builder = Builder::new(100);
        let objects = vec![
            obj(1, "<< /Type /Catalog /Pages 2 0 R /Note 4 0 R /Other 5 0 R >>"),
            obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] /Contents 6 0 R >>"),
            obj(4, "<< /Text (M_REPLACED_DIRECT) >>"),
            obj(5, "<< /Text (K_NEIGHBOUR) >>"),
            deflated(6, "", b"(M_REPLACED_STREAM) Tj"),
        ];
        builder.place(form, &objects, 100);
        builder.finish_revision(form, "/Root 1 0 R");
        let (output, _, after) = rewrite(&builder.bytes, |changes| {
            changes
                .write_object((4, 0), Object::Dictionary(dictionary! { "Text" => Object::string_literal("K_NEW_TEXT") }))
                .unwrap();
            changes
                .write_object((6, 0), Object::Stream(Stream::new(Dictionary::new(), b"(K_NEW_STREAM) Tj".to_vec())))
                .unwrap();
        });
        assert_markers(
            &output,
            &["M_REPLACED_DIRECT", "M_REPLACED_STREAM"],
            &["K_NEW_TEXT", "K_NEW_STREAM", "K_NEIGHBOUR"],
        );
        assert_eq!(after.resolve_object((4, 0)).unwrap().as_dict().unwrap().get(b"Text").unwrap().as_str().unwrap(), b"K_NEW_TEXT");
    }
}

// ---- Each kind of reference. ----

/// A link to a removed page, or a bookmark with one, surviving and leading nowhere, or the link next to it going too.
#[test]
fn links_to_a_removed_page_go_and_others_stay() {
    let (_, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.links, 1);
    assert!(gone(&after, 14));
    let page = dict(&after, 3);
    assert_eq!(page.get(b"Annots").unwrap().as_array().unwrap(), &vec![Object::Reference((15, 0))]);
    assert_eq!(summary.removal.annotations, 4);
    for number in [19, 21, 22, 47] {
        assert!(gone(&after, number), "{number}");
    }
}

/// The bookmark rule: a bookmark to a removed page goes and its children stay; a heading with nothing left goes; the links between those
/// that are left agree with each other, or a viewer loses bookmarks or loops; /Count is what's visible.
#[test]
fn bookmarks_keep_their_shape_around_what_went() {
    let (_, summary, after) = rewrite(&rich_document(Form::Compressed), remove_second_page);
    assert_eq!(summary.removal.outline_items, 4, "the bookmark, its named child, the heading and its child");
    assert_eq!(summary.removal.outline_items_promoted, 1);
    for number in [24, 25, 27, 28, 93] {
        assert!(gone(&after, number), "{number}");
    }
    let root = dict(&after, 20);
    let reference = |dict: &Dictionary, key: &[u8]| dict.get(key).unwrap().as_reference().unwrap().0;
    assert_eq!(reference(&root, b"First"), 23);
    assert_eq!(reference(&root, b"Last"), 26);
    assert_eq!(root.get(b"Count").unwrap().as_i64().unwrap(), 2);
    let first = dict(&after, 23);
    assert_eq!(reference(&first, b"Next"), 26);
    assert!(!first.has(b"Prev"));
    let child = dict(&after, 26);
    assert_eq!(reference(&child, b"Parent"), 20);
    assert_eq!(reference(&child, b"Prev"), 23);
    assert!(!child.has(b"Next"), "the sibling it had went, and nothing took its place");
}

/// All bookmarks on removed pages leaving an empty /Outlines behind, which some viewers show as an empty panel.
#[test]
fn no_bookmarks_left_means_no_outlines() {
    let bytes = two_pages(
        "/Outlines 10 0 R",
        &[
            obj(10, "<< /First 11 0 R /Last 11 0 R /Count 1 >>"),
            obj(11, "<< /Title (M_ONLY) /Parent 10 0 R /Dest [4 0 R /Fit] >>"),
        ],
        "",
        "",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert!(!dict(&after, 1).has(b"Outlines"));
    assert!(gone(&after, 10));
    assert_markers(&output, &["M_ONLY"], &[]);
}

/// Two pages, 3 and 4, with extra catalog entries and objects.
fn two_pages(catalog: &str, objects: &[Obj], first: &str, second: &str) -> Vec<u8> {
    let mut builder = Builder::new(500);
    let mut all = vec![
        obj(1, &format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>")),
        obj(2, "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 200] >>"),
        obj(3, &format!("<< /Type /Page /Parent 2 0 R {first} >>")),
        obj(4, &format!("<< /Type /Page /Parent 2 0 R {second} >>")),
    ];
    all.extend_from_slice(objects);
    builder.place(Form::Table, &all, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    builder.bytes
}

/// A destination name staying in the file: as a name, in the old dictionary, or in /Limits, which are names too and look harmless.
/// A name tree node left empty, which viewers can trip on.
#[test]
fn destination_names_go_with_their_page() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.named_destinations, 3);
    assert_markers(&output, &["M_DESTNAME", "M_DEST_ONLY", "M_OLDDEST"], &["A_keep", "OldKeep"]);
    assert!(gone(&after, 34), "the leaf with nothing left");
    let kept = dict(&after, 32);
    assert_eq!(
        kept.get(b"Limits").unwrap().as_array().unwrap(),
        &vec![Object::string_literal("A_keep"), Object::string_literal("A_keep")]
    );
    assert_eq!(dict(&after, 31).get(b"Kids").unwrap().as_array().unwrap().len(), 1);
    assert!(dict(&after, 33).has(b"OldKeep"));
}

/// /Dests entries reachable as a direct /Names tree, as an /OpenAction, or by an action that isn't direct.
#[test]
fn an_open_action_and_named_trees_to_a_removed_page_go() {
    let bytes = two_pages(
        "/OpenAction 10 0 R /Names << /Dests << /Names [(x) [4 0 R /Fit]] >> /Pages << /Names [(pagename) 4 0 R (pagekeep) 3 0 R] >> >> /AA << /WC << /S /GoTo /D [4 0 R /Fit] >> >>",
        &[obj(10, "<< /S /GoTo /D [4 0 R /Fit] /Next << /S /URI /URI (M_AFTER) >> >>")],
        "",
        "",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    let catalog = dict(&after, 1);
    assert!(!catalog.has(b"OpenAction"), "the action object has nowhere to go, so the reference to it goes");
    assert!(gone(&after, 10));
    let names = catalog.get(b"Names").unwrap().as_dict().unwrap();
    assert!(!names.has(b"Dests"));
    let pages = names.get(b"Pages").unwrap().as_dict().unwrap().get(b"Names").unwrap().as_array().unwrap();
    assert_eq!(pages.len(), 2);
    assert!(!catalog.get(b"AA").unwrap().as_dict().unwrap().has(b"WC"));
    assert_markers(&output, &["M_AFTER", "pagename"], &["pagekeep"]);
}

/// A form field on a removed page staying reachable through /Fields with everything typed in it; a field shared with a kept page losing its
/// kept widget or value; /CO naming what's gone; /XFA keeping a whole second copy of the form.
#[test]
fn form_fields_go_with_their_widgets() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.form_fields, 4, "two fields that went, and the widgets of two fields");
    assert!(summary.removal.xfa_dropped);
    let form = dict(&after, 40);
    assert_eq!(form.get(b"Fields").unwrap().as_array().unwrap(), &vec![Object::Reference((43, 0))]);
    assert!(form.get(b"CO").unwrap().as_array().unwrap().is_empty());
    assert!(!form.has(b"XFA"));
    assert_eq!(dict(&after, 43).get(b"Kids").unwrap().as_array().unwrap(), &vec![Object::Reference((42, 0))]);
    assert_markers(&output, &["M_FIELD_VALUE", "M_FIELD_PARENT", "M_WIDGET_AP", "M_XFA"], &["K_FIELD"]);
}

/// /AcroForm left in place with no fields, which makes viewers show a form bar for a document with no form.
#[test]
fn no_fields_left_means_no_form() {
    let bytes = two_pages(
        "/AcroForm << /Fields [10 0 R] >>",
        &[obj(10, "<< /Type /Annot /Subtype /Widget /Rect [0 0 9 9] /FT /Tx /V (M_V) /P 4 0 R >>")],
        "",
        "/Annots [10 0 R]",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert!(!dict(&after, 1).has(b"AcroForm"));
    assert_markers(&output, &["M_V"], &[]);
}

/// Structure elements keeping a removed page's text alive through /ActualText, /ParentTree or /IDTree, and an element over two pages losing
/// what's on the page that stays.
#[test]
fn the_structure_tree_keeps_only_what_is_on_kept_pages() {
    let (output, summary, after) = rewrite(&rich_document(Form::Compressed), remove_second_page);
    assert_eq!(summary.removal.structure_elements, 2);
    assert!(gone(&after, 52));
    assert!(gone(&after, 56));
    assert_eq!(dict(&after, 50).get(b"K").unwrap().as_array().unwrap().len(), 2);
    let mixed = dict(&after, 53);
    let kids = mixed.get(b"K").unwrap().as_array().unwrap();
    assert_eq!(kids.len(), 1, "only what's on the kept page is left");
    assert_eq!(kids[0].as_dict().unwrap().get(b"Pg").unwrap().as_reference().unwrap().0, 5);
    assert!(dict(&after, 51).has(b"Pg"));
    let nums = dict(&after, 54);
    let nums = nums.get(b"Nums").unwrap().as_array().unwrap();
    assert_eq!(summary.removal.parent_tree_entries, 2, "the page's own and its annotation's");
    assert_eq!(nums.len(), 4, "{nums:?}");
    assert_eq!(nums[0], Object::Integer(0));
    assert_markers(&output, &["M_ACTUAL", "M_ALT", "M_IDKEY"], &["K_ACTUAL", "keepid"]);
}

/// A reference in a kept /ParentTree array to an element that went shifting the rest along, which would change what every later
/// marked-content number means.
#[test]
fn a_parent_tree_array_keeps_its_positions() {
    let bytes = two_pages(
        "/StructTreeRoot << /K [10 0 R 11 0 R] /ParentTree << /Nums [0 [10 0 R 11 0 R 10 0 R]] >> >>",
        &[
            obj(10, "<< /S /P /Pg 3 0 R /K 0 >>"),
            obj(11, "<< /S /P /Pg 4 0 R /K 0 /ActualText (M_T) >>"),
        ],
        "/StructParents 0",
        "",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    let catalog = dict(&after, 1);
    let tree = catalog.get(b"StructTreeRoot").unwrap().as_dict().unwrap();
    let nums = tree.get(b"ParentTree").unwrap().as_dict().unwrap().get(b"Nums").unwrap().as_array().unwrap();
    assert_eq!(nums[1].as_array().unwrap(), &vec![Object::Reference((10, 0)), Object::Null, Object::Reference((10, 0))]);
    assert_markers(&output, &["M_T"], &[]);
}

/// A kept page's label changing because a page before it went, or the removed page's label prefix staying in /PageLabels.
#[test]
fn every_kept_page_keeps_its_label() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert!(summary.removal.page_labels_rebuilt);
    assert_markers(&output, &["M_LABEL"], &[]);
    let labels = dict(&after, 1);
    let nums = labels.get(b"PageLabels").unwrap().as_dict().unwrap().get(b"Nums").unwrap().as_array().unwrap().clone();
    assert_eq!(nums.len(), 4, "{nums:?}");
    assert_eq!(nums[0], Object::Integer(0));
    assert_eq!(nums[1].as_dict().unwrap().get(b"S").unwrap().as_name().unwrap(), b"r");
    assert_eq!(nums[2], Object::Integer(1));
    let third = nums[3].as_dict().unwrap();
    assert_eq!(third.get(b"S").unwrap().as_name().unwrap(), b"D");
    assert_eq!(third.get(b"St").unwrap().as_i64().unwrap(), 7);
}

/// A range's numbering restarting when a page out of its middle went: pages 1, 2, 3, 4 labelled by one range, without page 2, are 1, 3, 4.
#[test]
fn a_gap_inside_a_label_range_starts_a_new_one_at_the_right_number() {
    let mut builder = Builder::new(500);
    let mut objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [0 << /S /D /St 10 >>] >> >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 /MediaBox [0 0 9 9] >>"),
    ];
    for number in 3..=6 {
        objects.push(obj(number, "<< /Type /Page /Parent 2 0 R >>"));
    }
    builder.place(Form::Table, &objects, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let (_, _, after) = rewrite(&builder.bytes, remove_second_page);
    let catalog = dict(&after, 1);
    let nums = catalog.get(b"PageLabels").unwrap().as_dict().unwrap().get(b"Nums").unwrap().as_array().unwrap();
    assert_eq!(nums.len(), 4, "{nums:?}");
    assert_eq!(nums[1].as_dict().unwrap().get(b"St").unwrap().as_i64().unwrap(), 10);
    assert_eq!(nums[2], Object::Integer(1));
    assert_eq!(nums[3].as_dict().unwrap().get(b"St").unwrap().as_i64().unwrap(), 12);
}

/// A layer only a removed page used staying in the layer list with its name, while one a kept page uses disappears because the walk couldn't
/// see it was in use.
#[test]
fn a_layer_only_a_removed_page_used_goes() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.optional_content_groups, 1);
    assert!(gone(&after, 61));
    let catalog = dict(&after, 1);
    let layers = catalog.get(b"OCProperties").unwrap().as_dict().unwrap();
    assert_eq!(layers.get(b"OCGs").unwrap().as_array().unwrap(), &vec![Object::Reference((60, 0))]);
    let config = layers.get(b"D").unwrap().as_dict().unwrap();
    assert_eq!(config.get(b"ON").unwrap().as_array().unwrap(), &vec![Object::Reference((60, 0))]);
    assert_markers(&output, &["M_LAYER"], &["K_LAYER"]);
}

/// A thread's ring broken, or a bead left pointing nowhere, or an empty thread staying with its title.
#[test]
fn article_threads_close_up_around_what_went() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.thread_beads, 2);
    assert_eq!(summary.removal.threads, 1);
    assert_eq!(dict(&after, 1).get(b"Threads").unwrap().as_array().unwrap(), &vec![Object::Reference((71, 0))]);
    let first = dict(&after, 73);
    assert_eq!(first.get(b"N").unwrap().as_reference().unwrap().0, 75);
    assert_eq!(first.get(b"V").unwrap().as_reference().unwrap().0, 75);
    let last = dict(&after, 75);
    assert_eq!(last.get(b"N").unwrap().as_reference().unwrap().0, 73);
    assert_markers(&output, &["M_THREAD"], &["K_THREAD"]);
}

// ---- Shared resources. ----

/// A shared /Resources keeping a removed page's image in the file although nothing kept draws it, which is what reachability alone gives.
/// A name a kept page does use being taken out, which would break it.
#[test]
fn a_shared_resource_dictionary_keeps_only_what_kept_pages_can_name() {
    let (output, summary, after) = rewrite(&rich_document(Form::Table), remove_second_page);
    assert_eq!(summary.removal.resource_names_pruned, 2, "the image and the layer");
    assert!(summary.removal.resources_left_whole.is_empty());
    assert!(gone(&after, 12), "nothing kept can name it any more");
    let resources = dict(&after, 6);
    let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
    assert!(xobjects.has(b"Keep") && xobjects.has(b"Shared") && !xobjects.has(b"Gone"));
    assert!(resources.get(b"Font").unwrap().as_dict().unwrap().has(b"F1"));
    assert_markers(&output, &["M_IMAGE"], &["K_FORM_KEEP", "K_FORM_SHARED"]);
}

fn resource_sharing(objects: &[Obj], catalog_pages: &str, first: &str, second: &str) -> Vec<u8> {
    let mut builder = Builder::new(500);
    let mut all = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(
            2,
            &format!("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 200] {catalog_pages} >>"),
        ),
        obj(3, &format!("<< /Type /Page /Parent 2 0 R {first} >>")),
        obj(4, &format!("<< /Type /Page /Parent 2 0 R {second} >>")),
    ];
    all.extend_from_slice(objects);
    builder.place(Form::Table, &all, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    builder.bytes
}

/// A form XObject used by a kept and a removed page going with the removed one, or its own resources being pruned with no way of knowing
/// what it draws.
#[test]
fn a_form_on_both_pages_stays_whole() {
    let bytes = resource_sharing(
        &[
            deflated(10, "", b"/Both Do"),
            deflated(11, "", b"/Both Do (M_ONLY_ON_REMOVED) Tj"),
            deflated(12, "/Subtype /Form /BBox [0 0 9 9] /Resources << /XObject << /Inner 13 0 R >> >>", b"/Inner Do (K_FORM)"),
            deflated(13, "/Subtype /Form /BBox [0 0 9 9]", b"(K_INNER)"),
        ],
        "",
        "/Contents 10 0 R /Resources << /XObject << /Both 12 0 R >> >>",
        "/Contents 11 0 R /Resources << /XObject << /Both 12 0 R >> >>",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_ONLY_ON_REMOVED"], &["K_FORM", "K_INNER"]);
    assert!(!gone(&after, 12));
}

/// Resources inherited from the /Pages node and shared that way: a form with no /Resources of its own falls back on them, as viewers do, so
/// what it names has to stay although the page never names it.
#[test]
fn what_a_form_with_no_resources_names_stays() {
    let bytes = resource_sharing(
        &[
            deflated(10, "", b"/Outer Do"),
            deflated(11, "", b"/Secret Do"),
            deflated(12, "/Subtype /Form /BBox [0 0 9 9]", b"/Inner Do"),
            deflated(13, "/Subtype /Form /BBox [0 0 9 9]", b"K_INNER"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET"),
        ],
        "/Resources << /XObject << /Outer 12 0 R /Inner 13 0 R /Secret 14 0 R /Default 12 0 R >> /ColorSpace << /DefaultRGB 15 0 R /Unused 16 0 R >> >>",
        "/Contents 10 0 R",
        "/Contents 11 0 R",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_SECRET"], &["K_INNER"]);
    assert!(gone(&after, 14));
    assert!(!gone(&after, 13));
    let pages = dict(&after, 2);
    let resources = pages.get(b"Resources").unwrap().as_dict().unwrap();
    let spaces = resources.get(b"ColorSpace").unwrap().as_dict().unwrap();
    assert!(!spaces.has(b"Unused"));
    // Default color spaces are looked up by viewers, with no name in the content. These point at nothing here, so they're dropped as dangling.
    assert!(!spaces.has(b"DefaultRGB"));
}

/// A default color space being pruned because no content names it, which changes the kept page's colors.
#[test]
fn a_default_color_space_stays_though_nothing_names_it() {
    let bytes = resource_sharing(
        &[
            deflated(10, "", b"0 g"),
            deflated(11, "", b"/Secret Do"),
            obj(15, "[/ICCBased 16 0 R]"),
            deflated(16, "/N 3", b"K_PROFILE"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET"),
        ],
        "/Resources 20 0 R",
        "/Contents 10 0 R",
        "/Contents 11 0 R",
    );
    let mut builder = Builder::new(600);
    builder.bytes = bytes;
    builder.last_xref = lopdf_last_startxref(&builder.bytes);
    builder.place(
        Form::Table,
        &[obj(
            20,
            "<< /XObject << /Secret 14 0 R >> /ColorSpace << /DefaultRGB 15 0 R >> >>",
        )],
        100,
    );
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let (output, _, _) = rewrite(&builder.bytes, remove_second_page);
    assert_markers(&output, &["M_SECRET"], &["K_PROFILE"]);
}

fn lopdf_last_startxref(bytes: &[u8]) -> Option<u64> {
    let at = bytes.windows(9).rposition(|window| window == b"startxref")?;
    let text = std::str::from_utf8(&bytes[at + 9..]).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Resources that can't be searched being pruned anyway, which would break a page, or kept and not said.
#[test]
fn content_that_cannot_be_searched_leaves_the_dictionary_whole_and_says_so() {
    let bytes = resource_sharing(
        &[
            raw_stream(10, "/Filter /FlateDecode", b"not deflate at all"),
            deflated(11, "", b"/Secret Do"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET_LEFT"),
        ],
        "/Resources << /XObject << /Secret 14 0 R >> >>",
        "/Contents 10 0 R",
        "/Contents 11 0 R",
    );
    let (output, summary, after) = rewrite(&bytes, remove_second_page);
    assert_eq!(summary.removal.resources_left_whole, vec![(2, 0)]);
    assert_eq!(summary.removal.resource_names_pruned, 0);
    assert!(!gone(&after, 14));
    assert_markers(&output, &[], &["M_SECRET_LEFT"]);
}

/// A category dictionary shared by reference between pages with their own /Resources, which isn't the /Resources being shared, so a check
/// on that alone misses it.
#[test]
fn a_shared_category_dictionary_is_pruned_too() {
    let bytes = resource_sharing(
        &[
            obj(9, "<< /Keep 12 0 R /Secret 14 0 R >>"),
            deflated(10, "", b"/Keep Do"),
            deflated(11, "", b"/Secret Do"),
            deflated(12, "/Subtype /Form /BBox [0 0 9 9]", b"K_KEPT"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET"),
        ],
        "",
        "/Contents 10 0 R /Resources << /XObject 9 0 R >>",
        "/Contents 11 0 R /Resources << /XObject 9 0 R >>",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_SECRET"], &["K_KEPT"]);
    assert!(gone(&after, 14));
}

/// A kept form whose own /Resources is the dictionary a removed page used, which only the walk can find: that form names things too.
#[test]
fn a_form_that_uses_the_shared_dictionary_counts() {
    let bytes = resource_sharing(
        &[
            obj(9, "<< /XObject << /ViaForm 13 0 R /Form 12 0 R /Secret 14 0 R >> >>"),
            deflated(10, "", b"/Form Do"),
            deflated(11, "", b"/Secret Do"),
            deflated(12, "/Subtype /Form /BBox [0 0 9 9] /Resources 9 0 R", b"/ViaForm Do"),
            deflated(13, "/Subtype /Form /BBox [0 0 9 9]", b"K_VIA_FORM"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET"),
            obj(15, "<< /XObject << /Form 12 0 R >> >>"),
        ],
        "",
        "/Contents 10 0 R /Resources 15 0 R",
        "/Contents 11 0 R /Resources 9 0 R",
    );
    let (output, summary, after) = rewrite(&bytes, remove_second_page);
    assert!(summary.removal.resources_left_whole.is_empty());
    assert_markers(&output, &["M_SECRET"], &["K_VIA_FORM"]);
    assert!(gone(&after, 14));
}

/// The name search being fooled by an escaped name, a name run into the next token, or an image whose data looks like a string: any of those
/// would take out something a kept page uses.
#[test]
fn every_spelling_of_a_name_counts_as_using_it() {
    let bytes = resource_sharing(
        &[
            deflated(10, "", b"/Im#31 Do/Im2 Do(unterminated string BI ID \xFF( EI /Im3 Do"),
            deflated(11, "", b"/Secret Do"),
            deflated(12, "/Subtype /Form /BBox [0 0 9 9]", b"K_ONE"),
            deflated(13, "/Subtype /Form /BBox [0 0 9 9]", b"K_TWO"),
            deflated(15, "/Subtype /Form /BBox [0 0 9 9]", b"K_THREE"),
            deflated(14, "/Subtype /Form /BBox [0 0 9 9]", b"M_SECRET"),
        ],
        "/Resources << /XObject << /Im1 12 0 R /Im2 13 0 R /Im3 15 0 R /Secret 14 0 R >> >>",
        "/Contents 10 0 R",
        "/Contents 11 0 R",
    );
    let (output, _, _) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_SECRET"], &["K_ONE", "K_TWO", "K_THREE"]);
}

// ---- The page tree. ----

fn nested_tree(form: Form) -> Vec<u8> {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [10 0 R 11 0 R] /Count 3 >>"),
        obj(
            10,
            "<< /Type /Pages /Parent 2 0 R /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 100 100] /Resources << /Font << /F1 20 0 R >> /XObject << /Y 21 0 R >> >> >>",
        ),
        obj(
            11,
            "<< /Type /Pages /Parent 2 0 R /Kids [5 0 R] /Count 1 /MediaBox [0 0 300 300] /Resources << /Font << /F2 22 0 R >> >> >>",
        ),
        obj(3, "<< /Type /Page /Parent 10 0 R /Contents 30 0 R >>"),
        obj(4, "<< /Type /Page /Parent 10 0 R /Contents 31 0 R >>"),
        obj(5, "<< /Type /Page /Parent 11 0 R /Contents 32 0 R >>"),
        deflated(30, "", b"/F1 Tf (a)Tj"),
        deflated(31, "", b"/F1 Tf /Y Do (b)Tj"),
        deflated(32, "", b"/F2 Tf (c)Tj"),
        obj(20, "<< /Type /Font /Note (M_FONT_ONE) >>"),
        deflated(21, "/Subtype /Form /BBox [0 0 9 9]", b"K_FORM_ON_OTHER"),
        obj(22, "<< /Type /Font /Note (K_FONT_TWO) >>"),
    ];
    builder.place(form, &objects, 100);
    builder.finish_revision(form, "/Root 1 0 R");
    builder.bytes
}

/// /Count left stale up the tree, which PDFium and others trust, or a /Pages node left empty when both its pages went, while a page under another
/// parent keeps what it inherits from its own.
#[test]
fn the_page_tree_stays_right_when_a_parent_goes() {
    for form in [Form::Table, Form::Compressed] {
        let bytes = nested_tree(form);
        let (output, summary, after) = rewrite(&bytes, |changes| {
            changes.remove_page(0);
            changes.remove_page(1);
        });
        dump(&format!("nested_parent_gone_{form:?}.pdf"), &output);
        assert_eq!(summary.removal.page_tree_nodes, 1);
        assert!(gone(&after, 10));
        assert_markers(&output, &["M_FONT_ONE", "K_FORM_ON_OTHER"], &["K_FONT_TWO"]);
        let root = dict(&after, 2);
        assert_eq!(root.get(b"Kids").unwrap().as_array().unwrap(), &vec![Object::Reference((11, 0))]);
        assert_eq!(root.get(b"Count").unwrap().as_i64().unwrap(), 1);
        // The page that's left still inherits from the parent that's left.
        let map = after.page_map().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(0).unwrap().inherited().resources(), Some((11, 0)));
        assert_eq!(map.get(0).unwrap().inherited().media_box(), Some((11, 0)));
    }
}

/// One page of two going takes the parent's whole inherited /Resources with it, or leaves what only it named.
#[test]
fn an_inherited_dictionary_stays_for_the_page_that_stays() {
    let (output, summary, after) = rewrite(&nested_tree(Form::Table), remove_second_page);
    dump("nested_one_gone.pdf", &output);
    assert_eq!(summary.removal.page_tree_nodes, 0);
    assert_eq!(dict(&after, 10).get(b"Count").unwrap().as_i64().unwrap(), 1);
    assert_eq!(dict(&after, 2).get(b"Count").unwrap().as_i64().unwrap(), 2);
    let map = after.page_map().unwrap();
    assert_eq!(map.get(0).unwrap().inherited().resources(), Some((10, 0)));
    assert_markers(&output, &["K_FORM_ON_OTHER"], &["M_FONT_ONE", "K_FONT_TWO"]);
}

/// A kept page's /Parent naming a node that's gone from the tree, which PDFium reads its inherited attributes from, losing them.
#[test]
fn a_node_a_kept_page_inherits_from_by_parent_stays() {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [10 0 R 11 0 R] /Count 2 >>"),
        obj(10, "<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 /MediaBox [0 0 100 100] >>"),
        obj(11, "<< /Type /Pages /Parent 2 0 R /Kids [3 0 R] /Count 1 >>"),
        // Listed under 11, inherits from 10.
        obj(3, "<< /Type /Page /Parent 10 0 R >>"),
        obj(4, "<< /Type /Page /Parent 10 0 R >>"),
    ];
    builder.place(Form::Table, &objects, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let (_, _, after) = rewrite(&builder.bytes, |changes| changes.remove_page(0));
    assert!(!gone(&after, 10));
    assert_eq!(dict(&after, 3).get(b"Parent").unwrap().as_reference().unwrap().0, 10);
    assert_eq!(after.page_count().unwrap(), 1);
}

/// A page the tree lists twice coming back through its other place in /Kids.
#[test]
fn a_page_listed_twice_goes_from_both_places() {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R 4 0 R 3 0 R] /Count 3 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R /Note (M_TWICE) >>"),
        obj(4, "<< /Type /Page /Parent 2 0 R >>"),
    ];
    builder.place(Form::Table, &objects, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let (output, summary, after) = rewrite(&builder.bytes, |changes| changes.remove_page(2));
    assert_eq!(summary.removal.pages, vec![(3, 0)]);
    assert_eq!(after.page_count().unwrap(), 1);
    assert_markers(&output, &["M_TWICE"], &[]);
}

/// Removing every page writing a document nothing can open, or the index being checked one page late.
#[test]
fn pages_that_cannot_be_removed_are_refused() {
    let bytes = two_pages("", &[], "", "");
    let reader = open_bytes(&bytes);
    let mut changes = RewriteChanges::new(&reader).unwrap();
    changes.remove_page(0);
    changes.remove_page(1);
    assert!(matches!(changes.write_to(RewritePolicy::new(), std::io::sink()), Err(RewriteError::NoPagesLeft)));
    let mut changes = RewriteChanges::new(&reader).unwrap();
    changes.remove_page(2);
    assert!(matches!(
        changes.write_to(RewritePolicy::new(), std::io::sink()),
        Err(RewriteError::PageOutOfRange { index: 2, pages: 2 })
    ));
}

/// A removed page coming back by any of ten paths: through /Kids, a dictionary entry, a custom key in the catalog, an annotation's /P, a
/// bookmark's custom entry, a name tree value, /OpenAction, a structure element, a bead, an array nobody knows about.
#[test]
fn a_page_reached_by_ten_paths_is_gone_from_all_of_them() {
    let bytes = two_pages(
        "/Custom 4 0 R /CustomList [4 0 R 3 0 R] /Deep << /Inner << /Page 4 0 R >> >> /OpenAction [4 0 R /Fit] \
         /Outlines 10 0 R /Names << /Pages << /Names [(p) 4 0 R] >> >> /StructTreeRoot << /K [12 0 R] >> /Threads [13 0 R]",
        &[
            obj(10, "<< /First 11 0 R /Last 11 0 R /Count 1 >>"),
            obj(11, "<< /Title (K_KEPT_BOOK) /Parent 10 0 R /Dest [3 0 R /Fit] /Custom 4 0 R >>"),
            obj(12, "<< /S /P /Pg 3 0 R /K 0 /Ref 4 0 R >>"),
            obj(13, "<< /F 14 0 R >>"),
            obj(14, "<< /T 13 0 R /N 14 0 R /V 14 0 R /P 3 0 R /Other 4 0 R >>"),
            obj(15, "<< /Type /Annot /Subtype /Text /Rect [0 0 9 9] /P 4 0 R /Contents (K_NOTE) >>"),
            deflated(16, "", b"(M_ONLY_THERE) Tj"),
        ],
        "/Annots [15 0 R]",
        "/Contents 16 0 R /Note (M_PAGE_DICT)",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_PAGE_DICT", "M_ONLY_THERE"], &["K_KEPT_BOOK", "K_NOTE"]);
    assert!(gone(&after, 4));
    let catalog = dict(&after, 1);
    assert!(!catalog.has(b"Custom"));
    assert_eq!(catalog.get(b"CustomList").unwrap().as_array().unwrap(), &vec![Object::Null, Object::Reference((3, 0))]);
    assert!(!catalog.has(b"OpenAction"));
    assert!(!dict(&after, 15).has(b"P"));
    // Whatever's left refers to nothing that's gone, in any object.
    for id in after.object_ids() {
        let text = format!("{:?}", after.resolve_object(id).unwrap());
        assert!(!text.contains("Reference((4, 0))"), "object {id:?} still refers to the page: {text}");
    }
}

// ---- Copying. ----

/// A stream's bytes being decoded or changed on the way through, which a file with a filter this crate can't decode, or with data
/// that's simply broken, would show. The new file has to hold exactly the same bytes, and its /Length has to be right.
#[test]
fn stream_data_is_copied_byte_for_byte_without_being_decoded() {
    let garbage: Vec<u8> = (0..200_000_u32).map(|value| (value.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let bytes = {
        let mut builder = Builder::new(500);
        let objects = vec![
            obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
            obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] /Contents 4 0 R /Thumb 5 0 R >>"),
            raw_stream(4, "/Filter /FlateDecode", &garbage),
            raw_stream(5, "/Filter /JBIG2Decode /Subtype /Image", b"\x00\x01\x02endstrea"),
        ];
        builder.place(Form::Table, &objects, 100);
        builder.finish_revision(Form::Table, "/Root 1 0 R");
        builder.bytes
    };
    let (output, summary, after) = rewrite(&bytes, |_| {});
    let read = |number| {
        let descriptor = after.resolve_stream_descriptor((number, 0)).unwrap();
        let mut reader = descriptor.open_plain_encoded().unwrap();
        let mut content = Vec::new();
        let mut buffer = vec![0; 65_536];
        loop {
            let read = reader.read_chunk(&mut buffer).unwrap();
            if read == 0 {
                return content;
            }
            content.extend_from_slice(&buffer[..read]);
        }
    };
    assert_eq!(read(4), garbage);
    assert_eq!(read(5), b"\x00\x01\x02endstrea");
    assert_eq!(summary.stream_bytes_copied, garbage.len() as u64 + 11);
    assert!(output.windows(garbage.len()).any(|window| window == garbage));
}

/// A wrong /Length: PDFium finds the data by `endstream`, so what a reader that believed /Length would copy isn't what's drawn. An indirect
/// /Length's object staying as if something used it.
#[test]
fn a_wrong_or_indirect_length_copies_what_a_viewer_reads() {
    let bytes = {
        let mut builder = Builder::new(500);
        let mut too_long = b"<< /Length 99999 >>\nstream\n".to_vec();
        too_long.extend_from_slice(b"the real data\nendstream");
        let mut too_short = b"<< /Length 3 >>\nstream\n".to_vec();
        too_short.extend_from_slice(b"more real data\r\nendstream");
        let mut indirect = b"<< /Length 8 0 R >>\nstream\n".to_vec();
        indirect.extend_from_slice(b"indirect\nendstream");
        let as_stream = |number, body: Vec<u8>| Obj {
            number,
            generation: 0,
            body,
            is_stream: true,
        };
        let objects = vec![
            obj(1, "<< /Type /Catalog /Pages 2 0 R /A 4 0 R /B 5 0 R /C 6 0 R >>"),
            obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] >>"),
            as_stream(4, too_long),
            as_stream(5, too_short),
            as_stream(6, indirect),
            obj(8, "8"),
        ];
        builder.place(Form::Table, &objects, 100);
        builder.finish_revision(Form::Table, "/Root 1 0 R");
        builder.bytes
    };
    let (_, _, after) = rewrite(&bytes, |_| {});
    let content = |number| {
        after
            .resolve_object((number, 0))
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .clone()
    };
    assert_eq!(content(4), b"the real data");
    assert_eq!(content(5), b"more real data");
    assert_eq!(content(6), b"indirect");
    assert!(gone(&after, 8), "only /Length used it, and that's direct now");
}

/// A generation number being lost, which leaves every reference to the object pointing at nothing.
#[test]
fn an_object_keeps_its_generation() {
    let bytes = {
        let mut builder = Builder::new(500);
        let mut note = obj(4, "<< /Note (K_GEN) >>");
        note.generation = 3;
        let objects = vec![
            obj(1, "<< /Type /Catalog /Pages 2 0 R /Note 4 3 R >>"),
            obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] >>"),
            note,
        ];
        for object in &objects {
            builder.write_obj(object);
        }
        builder.finish_revision(Form::Table, "/Root 1 0 R");
        builder.bytes
    };
    let (_, _, after) = rewrite(&bytes, |_| {});
    assert!(after.resolve_object((4, 3)).unwrap().as_dict().unwrap().has(b"Note"));
    // The stream form can't compress it, but has to find it just the same.
    let (_, _, after) = rewrite_with(&bytes, RewritePolicy::new().form(RewriteFormChoice::Force(RewriteForm::Stream)), |_| {});
    assert!(after.resolve_object((4, 3)).unwrap().as_dict().unwrap().has(b"Note"));
}

/// A reference that names nothing being kept, where a new object could then get that number and give it a meaning it never had.
#[test]
fn a_reference_to_nothing_cannot_come_to_mean_something() {
    let bytes = two_pages("/Dangling 50 0 R /Freed 51 0 R", &[], "", "");
    let (_, _, after) = rewrite(&bytes, |changes| {
        // The next numbers, past the highest in the file.
        let id = changes.add_object(Object::Dictionary(dictionary! { "New" => 1 })).unwrap();
        assert_eq!(id, (5, 0));
    });
    let catalog = dict(&after, 1);
    assert!(!catalog.has(b"Dangling") && !catalog.has(b"Freed"));
}

/// A reference to an old object stream, in a file with one to spare, copying it with every neighbour it held as it was. Or to an old
/// cross-reference stream, with its stale offsets.
#[test]
fn a_reference_to_an_object_stream_does_not_bring_its_neighbours_back() {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Sneaky 500 0 R /Sneakier 501 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] >>"),
        obj(4, "<< /Secret (M_NEIGHBOUR) >>"),
    ];
    builder.place(Form::Compressed, &objects, 100);
    builder.finish_revision(Form::Compressed, "/Root 1 0 R");
    let (output, _, after) = rewrite(&builder.bytes, |_| {});
    assert_markers(&output, &["M_NEIGHBOUR"], &[]);
    let catalog = dict(&after, 1);
    assert!(!catalog.has(b"Sneaky") && !catalog.has(b"Sneakier"));
    assert_eq!(after.page_count().unwrap(), 1);
}

/// The form chosen not being the one promised: a table file growing a stream and needing a newer reader, a compressed one growing by
/// every object it had compressed.
#[test]
fn the_output_form_follows_the_original_unless_asked() {
    let table = rich_document(Form::Table);
    let compressed = rich_document(Form::Compressed);
    let hybrid = rich_document(Form::Hybrid);
    assert_eq!(rewrite(&table, |_| {}).1.form, RewriteForm::Table);
    assert_eq!(rewrite(&hybrid, |_| {}).1.form, RewriteForm::Stream);
    let (_, summary, after) = rewrite(&compressed, |_| {});
    assert_eq!(summary.form, RewriteForm::Stream);
    assert!(summary.objects_compressed > 40, "{}", summary.objects_compressed);
    assert!(summary.object_streams >= 1);
    assert_eq!(after.page_count().unwrap(), 3);

    let forced = RewritePolicy::new().form(RewriteFormChoice::Force(RewriteForm::Table));
    let (_, summary, after) = rewrite_with(&compressed, forced, |_| {});
    assert_eq!(summary.form, RewriteForm::Table);
    assert_eq!(summary.objects_compressed, 0);
    assert_eq!(after.page_count().unwrap(), 3);
    let forced = RewritePolicy::new().form(RewriteFormChoice::Force(RewriteForm::Stream));
    let (output, summary, after) = rewrite_with(&table, forced, |_| {});
    assert_eq!(summary.form, RewriteForm::Stream);
    assert_eq!(after.page_count().unwrap(), 3);
    assert!(output.starts_with(b"%PDF-1.7"));
}

/// A header older than what the output's form needs, which a strict reader would refuse.
#[test]
fn the_header_says_at_least_what_the_form_needs() {
    let bytes = {
        let mut bytes = two_pages("", &[], "", "");
        bytes[5..8].copy_from_slice(b"1.3");
        bytes
    };
    let (output, _, _) = rewrite(&bytes, |_| {});
    assert!(output.starts_with(b"%PDF-1.3\n"));
    let forced = RewritePolicy::new().form(RewriteFormChoice::Force(RewriteForm::Stream));
    let (output, _, _) = rewrite_with(&bytes, forced, |_| {});
    assert!(output.starts_with(b"%PDF-1.5\n"));
}

/// `/ID`'s first string changing, which ties a file to its past, or its second not changing with the content.
#[test]
fn the_permanent_id_stays_and_the_other_changes() {
    let bytes = rich_document(Form::Table);
    let reader = open_bytes(&bytes);
    let id = |reader: &IndexedReader| reader.trailer_entry_raw_owned(b"ID").unwrap();
    let (_, _, after) = rewrite(&bytes, |_| {});
    let before = id(&reader);
    let now = id(&after);
    let (before, now) = (before.as_array().unwrap(), now.as_array().unwrap());
    assert_eq!(before[0], now[0]);
    assert_ne!(before[1], now[1]);
    assert!(after.trailer_entry_raw_owned(b"Stash").is_none());
    assert!(after.trailer_entry_raw_owned(b"Prev").is_none());
}

/// A number the caller reserved and never gave leaving a reference to nothing; a stream reader giving a different number of bytes than it said,
/// which would write a /Length that lies; an old generation; a free number.
#[test]
fn bad_changes_are_refused() {
    let bytes = two_pages("", &[], "", "");
    let reader = open_bytes(&bytes);
    let mut changes = RewriteChanges::new(&reader).unwrap();
    let id = changes.allocate().unwrap();
    assert!(matches!(
        changes.write_to(RewritePolicy::new(), std::io::sink()),
        Err(RewriteError::ReservedNotWritten { id: reserved }) if reserved == id
    ));
    let mut changes = RewriteChanges::new(&reader).unwrap();
    assert!(matches!(
        changes.write_object((3, 7), Object::Integer(1)),
        Err(RewriteError::GenerationMismatch { expected: 0, .. })
    ));
    assert!(matches!(
        changes.write_object((99, 0), Object::Integer(1)),
        Err(RewriteError::UnknownObject { .. })
    ));
    assert!(matches!(
        changes.write_object((3, 0), Object::Real(f32::NAN)),
        Err(RewriteError::InvalidObject { .. })
    ));
    assert!(matches!(changes.set_info(Object::Integer(1)), Err(RewriteError::InvalidObject { .. })));

    for (declared, given) in [(10_u64, &b"short"[..]), (2, b"longer than that")] {
        let mut changes = RewriteChanges::new(&reader).unwrap();
        changes.write_stream_from((3, 0), Dictionary::new(), declared, given).unwrap();
        assert!(matches!(
            changes.write_to(RewritePolicy::new(), std::io::sink()),
            Err(RewriteError::StreamLengthMismatch { .. })
        ));
    }
}

/// A stream given from a reader being held in memory or written with the wrong length.
#[test]
fn a_stream_from_a_reader_is_written_as_given() {
    let bytes = two_pages("", &[deflated(5, "", b"(M_OLD_CONTENT) Tj")], "/Contents 5 0 R", "");
    let content = vec![b'x'; 300_000];
    let (output, summary, after) = rewrite(&bytes, |changes| {
        changes
            .write_stream_from((5, 0), Dictionary::new(), content.len() as u64, std::io::Cursor::new(content.clone()))
            .unwrap();
    });
    assert_markers(&output, &["M_OLD_CONTENT"], &[]);
    assert_eq!(after.resolve_object((5, 0)).unwrap().as_stream().unwrap().content, content);
    assert_eq!(summary.stream_bytes_copied, content.len() as u64);
}

/// New objects and a new /Info and /Root not reaching the file, or a removed object staying because something else still names it.
#[test]
fn new_objects_a_new_root_and_a_removed_object() {
    let bytes = two_pages("/Keep 5 0 R /Drop 6 0 R", &[obj(5, "<< /K (K_KEEP) >>"), obj(6, "<< /D (M_DROP) >>")], "", "");
    let (output, _, after) = rewrite(&bytes, |changes| {
        changes.remove_object((6, 0));
        let info = changes.add_object(Object::Dictionary(dictionary! { "Title" => Object::string_literal("K_NEW_INFO") })).unwrap();
        changes.set_info(Object::Reference(info)).unwrap();
        let root = changes
            .add_object(Object::Dictionary(dictionary! {
                "Type" => "Catalog",
                "Pages" => Object::Reference((2, 0)),
                "Mine" => Object::Reference((5, 0)),
                "Dropped" => Object::Reference((6, 0)),
            }))
            .unwrap();
        changes.set_root(root);
    });
    assert_markers(&output, &["M_DROP"], &["K_KEEP", "K_NEW_INFO"]);
    assert_eq!(after.page_count().unwrap(), 2);
    assert!(gone(&after, 1), "nothing reaches the old catalog");
    assert_eq!(after.trailer_entry_owned(b"Info").unwrap().unwrap().as_dict().unwrap().get(b"Title").unwrap().as_str().unwrap(), b"K_NEW_INFO");
}

// ---- Refusals. ----

fn signed_document(form: Form) -> Vec<u8> {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /AcroForm << /SigFlags 3 /Fields [5 0 R] >> >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 200] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R /Annots [5 0 R] >>"),
        obj(4, "<< /Type /Page /Parent 2 0 R >>"),
        obj(5, "<< /Type /Annot /Subtype /Widget /FT /Sig /T (Signature1) /Rect [0 0 0 0] /P 3 0 R /V 6 0 R >>"),
        Obj {
            number: 6,
            generation: 0,
            body: b"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /Name (Somebody) /ByteRange [0 100 200 300] /Contents <00AABB> >>".to_vec(),
            is_stream: true,
        },
    ];
    builder.place(form, &objects, 100);
    builder.finish_revision(form, "/Root 1 0 R");
    builder.bytes
}

/// A signed file being rewritten without anyone asking, which silently breaks its signature.
#[test]
fn a_signed_file_is_refused_unless_it_is_asked_for() {
    for form in ALL_FORMS {
        let bytes = signed_document(form);
        let reader = open_bytes(&bytes);
        let found = reader.signatures().unwrap();
        assert_eq!(found.len(), 1, "{form:?}: {found:?}");
        assert_eq!(found[0].object, Some((6, 0)));
        assert_eq!(found[0].byte_range, Some([0, 100, 200, 300]));
        assert_eq!(found[0].sub_filter.as_deref(), Some(&b"adbe.pkcs7.detached"[..]));
        assert_eq!(found[0].stated_name.as_deref(), Some(&b"Somebody"[..]));
        assert_eq!(found[0].field_name.as_deref(), Some(&b"Signature1"[..]));
        assert_eq!(found[0].kind, IndexedSignatureKind::Signature);

        let changes = RewriteChanges::new(&reader).unwrap();
        let refused = changes.write_to(RewritePolicy::new(), std::io::sink());
        assert!(
            matches!(&refused, Err(RewriteError::SignaturesWouldBreak { signatures }) if signatures.len() == 1),
            "{refused:?}"
        );

        let allowed = RewritePolicy::new().allow_breaking_signatures();
        let (output, summary, after) = rewrite_with(&bytes, allowed, |_| {});
        assert_eq!(summary.broken_signatures.len(), 1);
        assert_eq!(summary.broken_signatures[0].object, Some((6, 0)));
        assert_eq!(after.page_count().unwrap(), 2);
        // It stays, so that a viewer sees a broken signature rather than a drawn-on "signed by" with nothing behind it.
        assert!(output.windows(10).any(|window| window == b"/ByteRange"));
    }
}

/// A signature the newest revision no longer mentions being missed, so that its signed bytes are replaced without a word, as they would be if
/// only the current form fields were looked at. Or one in an object stream, which no real signer can write but a hostile file can.
#[test]
fn a_signature_only_an_old_revision_holds_counts() {
    let mut builder = Builder::new(500);
    let first = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R] >> >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(5, "<< /FT /Sig /T (Gone) /V 6 0 R >>"),
        obj(6, "<< /Type /Sig /ByteRange [0 10 20 30] /Contents <00> >>"),
    ];
    builder.place(Form::Table, &first, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    builder.free.push(6);
    builder.place(Form::Table, &[obj(1, "<< /Type /Catalog /Pages 2 0 R >>")], 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let reader = open_bytes(&builder.bytes);
    let found = reader.signatures().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].byte_range, Some([0, 10, 20, 30]));
    assert!(!found[0].in_newest_revision);

    let mut builder = Builder::new(500);
    let hidden = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Sig 6 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(6, "<< /Type /Sig /Contents <00> >>"),
    ];
    builder.place(Form::Compressed, &hidden, 100);
    builder.finish_revision(Form::Compressed, "/Root 1 0 R");
    let reader = open_bytes(&builder.bytes);
    let changes = RewriteChanges::new(&reader).unwrap();
    assert!(matches!(
        changes.write_to(RewritePolicy::new(), std::io::sink()),
        Err(RewriteError::SignaturesWouldBreak { .. })
    ));
}

/// The refusals: an encrypted file written in the clear, a rebuilt index or a /Prev loop whose reachable objects aren't known for certain.
#[test]
fn files_it_cannot_be_sure_of_are_refused() {
    let encrypted = two_pages("", &[], "", "");
    let mut with_encrypt = encrypted.clone();
    let at = find(&with_encrypt, b"/Root 1 0 R").unwrap();
    with_encrypt.splice(at..at, b"/Encrypt << /Filter /Standard /V 1 /R 2 /O <00> /U <00> /P -4 >> ".iter().copied());
    if let Ok(reader) = IndexedReader::open(BytesSource::from(with_encrypt)) {
        assert!(matches!(RewriteChanges::new(&reader), Err(RewriteError::Encrypted)));
    }

    let mut damaged = two_pages("", &[], "", "");
    let at = find(&damaged, b"startxref").unwrap();
    damaged.truncate(at);
    damaged.extend_from_slice(b"startxref\n999999\n%%EOF\n");
    let reader = open_bytes(&damaged);
    assert!(matches!(RewriteChanges::new(&reader), Err(RewriteError::RecoveredIndex)));

    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
    ];
    builder.place(Form::Table, &objects, 100);
    let xref = builder.bytes.len();
    builder.finish_revision(Form::Table, &format!("/Root 1 0 R /Prev {xref}"));
    let reader = open_bytes(&builder.bytes);
    assert!(matches!(RewriteChanges::new(&reader), Err(RewriteError::PrevLoop)));
}

/// An object that can be reached and can't be read going missing without a word.
#[test]
fn an_object_that_can_be_reached_and_cannot_be_read_is_refused() {
    let mut builder = Builder::new(500);
    let mut bad = obj(4, "<< /Unterminated [ 1 2 ");
    bad.is_stream = true;
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Bad 4 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        bad,
    ];
    builder.place(Form::Table, &objects, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    let reader = open_bytes(&builder.bytes);
    let refused = RewriteChanges::new(&reader).unwrap().write_to(RewritePolicy::new(), std::io::sink());
    assert!(matches!(refused, Err(RewriteError::UnreadableObject { id: (4, 0), .. })), "{refused:?}");
}

// ---- Destinations. ----

fn on_disk(folder: &Path, bytes: &[u8]) -> (std::path::PathBuf, IndexedReader) {
    let path = folder.join("doc.pdf");
    std::fs::write(&path, bytes).unwrap();
    let reader = IndexedReader::open(FileSource::open(&path).unwrap()).unwrap();
    (path, reader)
}

fn no_temporary_names(folder: &Path) {
    let left: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A save over the same file, a new file or another file not doing what it says, not reading back through the reader it returns, or
/// leaving a temporary name behind.
#[test]
fn every_destination_writes_a_whole_file() {
    let bytes = rich_document(Form::Compressed);
    let folder = tempfile::tempdir().unwrap();
    let (path, reader) = on_disk(folder.path(), &bytes);

    let new_path = folder.path().join("new.pdf");
    let done = rewrite_atomically(&reader, AppendDestination::NewFile(&new_path), RewriteOptions::new(), |changes| {
        changes.remove_page(1);
        Ok(())
    })
    .unwrap();
    assert_eq!(done.reader.page_count().unwrap(), 2);
    assert_eq!(std::fs::metadata(&new_path).unwrap().len(), done.summary.new_len);
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "a Save As changed the original");

    // There's a file there now.
    let again = rewrite_atomically(&reader, AppendDestination::NewFile(&new_path), RewriteOptions::new(), |_| Ok(()));
    assert!(matches!(again, Err(RewriteError::Destination(AppendError::TargetExists { .. }))));

    let done = rewrite_atomically(&reader, AppendDestination::OverwriteOther(&new_path), RewriteOptions::new(), |_| Ok(())).unwrap();
    assert_eq!(done.reader.page_count().unwrap(), 3);

    let done = rewrite_atomically(&reader, AppendDestination::SameFile(&path), RewriteOptions::new(), |changes| {
        changes.remove_page(1);
        Ok(())
    })
    .unwrap();
    assert_eq!(done.reader.page_count().unwrap(), 2);
    let saved = std::fs::read(&path).unwrap();
    assert_markers(&saved, &GONE, &KEPT);
    // The old reader still reads what it opened, and the file it opened is gone from this path.
    assert_eq!(reader.page_count().unwrap(), 3);
    no_temporary_names(folder.path());

    // A save with the old reader finds the file has moved on.
    let stale = rewrite_atomically(&reader, AppendDestination::SameFile(&path), RewriteOptions::new(), |_| Ok(()));
    assert!(matches!(stale, Err(RewriteError::Destination(AppendError::TargetChanged { .. }))));
    // The new one can save again.
    let done_again = rewrite_atomically(&done.reader, AppendDestination::SameFile(&path), RewriteOptions::new(), |_| Ok(())).unwrap();
    assert_eq!(done_again.reader.page_count().unwrap(), 2);
    no_temporary_names(folder.path());
}

/// A target's read-only state or hard links being ignored, since a rename only needs the folder: the other names would keep what was
/// meant to be gone, which is worse here than for an append.
#[cfg(unix)]
#[test]
fn hard_links_and_read_only_targets_are_refused_as_append_refuses_them() {
    let bytes = two_pages("", &[], "", "");
    let folder = tempfile::tempdir().unwrap();
    let (path, reader) = on_disk(folder.path(), &bytes);
    let link = folder.path().join("link.pdf");
    std::fs::hard_link(&path, &link).unwrap();
    let refused = rewrite_atomically(&reader, AppendDestination::SameFile(&path), RewriteOptions::new(), |_| Ok(()));
    assert!(matches!(refused, Err(RewriteError::Destination(AppendError::TargetHasHardLinks { .. }))));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let done = rewrite_atomically(
        &reader,
        AppendDestination::SameFile(&path),
        RewriteOptions::new().allow_breaking_hard_links(),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(std::fs::read(&link).unwrap(), bytes, "the other name keeps the old bytes, which is what was asked for");
    assert_eq!(done.reader.page_count().unwrap(), 2);

    use std::os::unix::fs::PermissionsExt;
    let other = folder.path().join("other.pdf");
    std::fs::write(&other, b"not yours").unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o444)).unwrap();
    let refused = rewrite_atomically(&reader, AppendDestination::OverwriteOther(&other), RewriteOptions::new(), |_| Ok(()));
    assert!(matches!(refused, Err(RewriteError::Destination(AppendError::TargetReadOnly { .. }))));
    assert_eq!(std::fs::read(&other).unwrap(), b"not yours");
    no_temporary_names(folder.path());
}

/// A prepare step not getting the finished file, its failure leaving something behind, or damage from it reaching the destination.
#[test]
fn a_prepare_step_works_as_it_does_for_an_append() {
    let bytes = two_pages("", &[], "", "");
    let folder = tempfile::tempdir().unwrap();
    let (path, reader) = on_disk(folder.path(), &bytes);

    let mut seen_target = None;
    rewrite_atomically(
        &reader,
        AppendDestination::SameFile(&path),
        RewriteOptions::new().prepare_replacement(|temporary, target| {
            assert!(std::fs::read(temporary)?.ends_with(b"%%EOF\n"));
            seen_target = Some(target.to_path_buf());
            Ok(())
        }),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(seen_target.as_deref(), Some(std::fs::canonicalize(&path).unwrap().as_path()));

    let (path, reader) = on_disk(folder.path(), &bytes);
    let failed = rewrite_atomically(
        &reader,
        AppendDestination::SameFile(&path),
        RewriteOptions::new().prepare_replacement(|_, _| Err(std::io::Error::other("no"))),
        |_| Ok(()),
    );
    assert!(matches!(failed, Err(RewriteError::PrepareReplacement(_))));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    no_temporary_names(folder.path());

    let damaged = rewrite_atomically(
        &reader,
        AppendDestination::SameFile(&path),
        RewriteOptions::new().prepare_replacement(|temporary, _| std::fs::write(temporary, b"%PDF-1.7\ndamaged")),
        |_| Ok(()),
    );
    assert!(matches!(damaged, Err(RewriteError::VerificationFailed { .. })));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    no_temporary_names(folder.path());
}

/// A rewrite over a signed file itself getting through because it was asked for, or by naming the same file another way. The signed bytes
/// never change, whatever is asked. A Save As, when asked for, has to work and say what broke.
#[test]
fn a_signed_file_is_never_replaced_and_a_save_as_says_what_broke() {
    let bytes = signed_document(Form::Table);
    let folder = tempfile::tempdir().unwrap();
    let (path, reader) = on_disk(folder.path(), &bytes);
    let allow = || RewriteOptions::new().policy(RewritePolicy::new().allow_breaking_signatures());
    let refused = rewrite_atomically(&reader, AppendDestination::SameFile(&path), allow(), |_| Ok(()));
    assert!(matches!(refused, Err(RewriteError::SignedFileCannotBeReplaced { .. })));
    let refused = rewrite_atomically(&reader, AppendDestination::OverwriteOther(&path), allow(), |_| Ok(()));
    assert!(matches!(refused, Err(RewriteError::SignedFileCannotBeReplaced { .. })));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let new = folder.path().join("copy.pdf");
    let refused = rewrite_atomically(&reader, AppendDestination::NewFile(&new), RewriteOptions::new(), |_| Ok(()));
    assert!(matches!(refused, Err(RewriteError::SignaturesWouldBreak { .. })));
    assert!(!new.exists());

    let done = rewrite_atomically(&reader, AppendDestination::NewFile(&new), allow(), |_| Ok(())).unwrap();
    assert_eq!(done.summary.broken_signatures.len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    no_temporary_names(folder.path());
}

/// A link that's a dictionary in /Annots, which no number can kill, staying with its text and nowhere to go.
#[test]
fn a_direct_link_to_a_removed_page_goes() {
    let bytes = two_pages(
        "",
        &[],
        "/Annots [<< /Type /Annot /Subtype /Link /Rect [0 0 9 9] /Dest [4 0 R /Fit] /Contents (M_DIRECT) >> << /Type /Annot /Subtype /Text /Rect [0 0 9 9] /P 4 0 R /Contents (K_TEXT) >>]",
        "",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert_markers(&output, &["M_DIRECT"], &["K_TEXT"]);
    assert_eq!(dict(&after, 3).get(b"Annots").unwrap().as_array().unwrap().len(), 1);
}

/// A file with junk before its header having its offsets miscounted, and the junk reaching the new file.
#[test]
fn junk_before_the_header_goes() {
    let mut bytes = b"M_JUNK not a pdf\n".to_vec();
    let body = two_pages("/Note 5 0 R", &[obj(5, "<< /K (K_NOTE) >>")], "", "");
    bytes.extend_from_slice(&body);
    let (output, _, after) = rewrite(&bytes, |_| {});
    assert!(output.starts_with(b"%PDF-"));
    assert_markers(&output, &["M_JUNK"], &["K_NOTE"]);
    assert_eq!(after.page_count().unwrap(), 2);
}

/// A rewritten file that can't take the next save, of either kind, because its cross-reference data or trailer isn't what they expect.
#[test]
fn a_rewritten_file_can_be_appended_to_and_rewritten_again() {
    for form in ALL_FORMS {
        let bytes = rich_document(form);
        let (output, _, after) = rewrite(&bytes, remove_second_page);
        after.check_appendable().unwrap();
        let mut appended = Vec::new();
        let mut writer = lopdf::AppendWriter::new(&after, &mut appended).unwrap();
        let added = writer.add_object(&Object::Integer(7)).unwrap();
        let summary = writer.finish().unwrap();
        let source: std::sync::Arc<dyn lopdf::RandomAccessSource> = std::sync::Arc::new(BytesSource::from(appended.clone()));
        let again = after.verify_appended(source, &summary).unwrap();
        assert_eq!(again.resolve_object(added.id).unwrap(), Object::Integer(7));
        assert!(appended.starts_with(&output));

        let (output_two, _, after_two) = rewrite(&appended, |changes| changes.remove_page(0));
        assert_eq!(after_two.page_count().unwrap(), 1);
        assert!(after_two.resolve_object(added.id).is_err(), "nothing refers to it, so it goes");
        assert_markers(&output_two, &["K_PAGE_ONE"], &["K_PAGE_THREE"]);
    }
}

/// A /XRefStm that says something else about an object its own table has live, where which one wins depends on the reader. A rewrite would settle
/// that silently one way, and what it copied might not be what a viewer showed.
#[test]
fn an_ambiguous_hybrid_is_refused() {
    let mut builder = Builder::new(500);
    let objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Note 4 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(4, "<< /Which (compressed) >>"),
    ];
    builder.place(Form::Hybrid, &objects, 100);
    builder.write_obj(&obj(4, "<< /Which (classic) >>"));
    builder.finish_revision(Form::Hybrid, "/Root 1 0 R");
    let reader = open_bytes(&builder.bytes);
    assert!(matches!(RewriteChanges::new(&reader), Err(RewriteError::AmbiguousHybrid { .. })));
}

/// A document opened through a symbolic link having the link replaced by a file, and the real document left with what should be gone.
#[cfg(unix)]
#[test]
fn a_symbolic_link_stays_a_link() {
    let bytes = two_pages("", &[deflated(5, "", b"M_GONE")], "", "/Contents 5 0 R");
    let folder = tempfile::tempdir().unwrap();
    let real = folder.path().join("real.pdf");
    std::fs::write(&real, &bytes).unwrap();
    let link = folder.path().join("link.pdf");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let reader = IndexedReader::open(FileSource::open(&link).unwrap()).unwrap();
    rewrite_atomically(&reader, AppendDestination::SameFile(&link), RewriteOptions::new(), |changes| {
        changes.remove_page(1);
        Ok(())
    })
    .unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_markers(&std::fs::read(&real).unwrap(), &["M_GONE"], &[]);
    no_temporary_names(folder.path());
}

// ---- Hostile files. ----

fn plain_file(objects: Vec<Obj>) -> Vec<u8> {
    let mut builder = Builder::new(2_000_000);
    builder.place(Form::Table, &objects, 100);
    builder.finish_revision(Form::Table, "/Root 1 0 R");
    builder.bytes
}

/// A rewrite that can't finish or falls over on references in a loop, deep or wide.
#[test]
fn reference_cycles_end() {
    let bytes = plain_file(vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Loop 4 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(4, "<< /Next 5 0 R /Self 4 0 R /Up 1 0 R >>"),
        obj(5, "<< /Next 4 0 R /Array [5 0 R 6 0 R] >>"),
        obj(6, "7 0 R"),
        obj(7, "6 0 R"),
    ]);
    let (_, _, after) = rewrite(&bytes, |_| {});
    assert_eq!(after.page_count().unwrap(), 1);
    for number in 4..=7 {
        assert!(!gone(&after, number), "{number}");
    }
}

/// A million references to one object making the walk visit it a million times, or copy it that often.
#[test]
fn a_million_references_to_one_object_copy_it_once() {
    let mut flood = String::with_capacity(8_000_000);
    flood.push('[');
    for _ in 0..1_000_000 {
        flood.push_str("5 0 R ");
    }
    flood.push(']');
    let bytes = plain_file(vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Flood 4 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(4, &flood),
        obj(5, "<< /Target (K_TARGET) >>"),
    ]);
    let reader = IndexedReader::open_with_options(
        BytesSource::from(bytes),
        lopdf::IndexedReaderOptions {
            object_bytes: 16 * 1_024 * 1_024,
            ..Default::default()
        },
    )
    .unwrap();
    let started = std::time::Instant::now();
    let mut output = Vec::new();
    let summary = RewriteChanges::new(&reader).unwrap().write_to(RewritePolicy::new(), &mut output).unwrap();
    assert_eq!(summary.objects_written, 5);
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
    let count = output.windows(8).filter(|window| window == b"K_TARGET").count();
    assert_eq!(count, 1);
}

/// A page tree that loops making page removal run forever or fall over. The reader refuses to walk such a tree, and so does the removal. A rewrite
/// without one has no need to walk it, and copies it as it is.
#[test]
fn a_page_tree_loop_is_handled() {
    let bytes = plain_file(vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R 10 0 R] /Count 2 /MediaBox [0 0 9 9] >>"),
        obj(3, "<< /Type /Page /Parent 2 0 R >>"),
        obj(10, "<< /Type /Pages /Parent 2 0 R /Kids [2 0 R 10 0 R 4 0 R] /Count 1 >>"),
        obj(4, "<< /Type /Page /Parent 10 0 R >>"),
    ]);
    let reader = open_bytes(&bytes);
    let mut output = Vec::new();
    RewriteChanges::new(&reader).unwrap().write_to(RewritePolicy::new(), &mut output).unwrap();
    assert!(IndexedReader::open(BytesSource::from(output)).is_ok());

    let mut changes = RewriteChanges::new(&reader).unwrap();
    changes.remove_page(0);
    match changes.write_to(RewritePolicy::new(), Vec::new()) {
        Ok(summary) => assert_eq!(summary.removal.pages, vec![(3, 0)]),
        // What the reader makes of a loop is its own business: this only has to end, and say something true.
        Err(error) => assert!(
            matches!(
                error,
                RewriteError::Reader(_) | RewriteError::PageOutOfRange { .. } | RewriteError::NoPagesLeft
            ),
            "{error:?}"
        ),
    }
}

/// A huge /Kids being looked at again for every page, which is quadratic, or stack frames being spent on it.
#[test]
fn a_huge_kids_array_is_quick() {
    const PAGES: u32 = 50_000;
    let mut kids = String::new();
    for number in 0..PAGES {
        kids.push_str(&format!("{} 0 R ", number + 10));
    }
    let mut objects = vec![
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, &format!("<< /Type /Pages /Kids [{kids}] /Count {PAGES} /MediaBox [0 0 9 9] >>")),
    ];
    for number in 0..PAGES {
        objects.push(obj(number + 10, "<< /Type /Page /Parent 2 0 R >>"));
    }
    let started = std::time::Instant::now();
    let (_, summary, after) = rewrite(&plain_file(objects), |changes| changes.remove_page(25_000));
    assert_eq!(after.page_count().unwrap(), PAGES as usize - 1);
    assert_eq!(summary.objects_written, u64::from(PAGES) + 1);
    assert!(started.elapsed().as_secs() < 120, "{:?}", started.elapsed());
}

/// Bookmarks in a loop through /Next or /First making the walk run forever, in a file where it has something to do.
#[test]
fn outline_loops_end() {
    let bytes = two_pages(
        "/Outlines 10 0 R",
        &[
            obj(10, "<< /First 11 0 R /Last 12 0 R /Count 2 >>"),
            obj(11, "<< /Title (a) /Parent 10 0 R /Next 12 0 R /First 11 0 R /Dest [3 0 R /Fit] >>"),
            obj(12, "<< /Title (M_LOOP) /Parent 10 0 R /Prev 11 0 R /Next 11 0 R /First 10 0 R /Dest [4 0 R /Fit] >>"),
        ],
        "",
        "",
    );
    let (output, _, after) = rewrite(&bytes, remove_second_page);
    assert!(gone(&after, 12));
    assert_markers(&output, &["M_LOOP"], &[]);
}

/// Structure elements, fields, name trees, number trees and threads in a loop, each with a walk of its own to get lost in.
#[test]
fn loops_in_every_other_tree_end() {
    let bytes = two_pages(
        "/StructTreeRoot << /K [10 0 R] /ParentTree 12 0 R >> /AcroForm << /Fields [14 0 R] >> /Names << /Dests 16 0 R >> \
         /PageLabels 18 0 R /Threads [20 0 R]",
        &[
            obj(10, "<< /S /P /K [11 0 R 10 0 R] /Pg 4 0 R >>"),
            obj(11, "<< /S /P /K [10 0 R] >>"),
            obj(12, "<< /Kids [13 0 R] >>"),
            obj(13, "<< /Kids [12 0 R] >>"),
            obj(14, "<< /FT /Tx /Kids [15 0 R] >>"),
            obj(15, "<< /Kids [14 0 R] >>"),
            obj(16, "<< /Kids [17 0 R] >>"),
            obj(17, "<< /Kids [16 0 R] >>"),
            obj(18, "<< /Kids [19 0 R] >>"),
            obj(19, "<< /Kids [18 0 R] >>"),
            obj(20, "<< /F 21 0 R >>"),
            obj(21, "<< /N 22 0 R /P 3 0 R >>"),
            obj(22, "<< /N 21 0 R /P 4 0 R >>"),
        ],
        "",
        "",
    );
    let (_, _, after) = rewrite(&bytes, remove_second_page);
    assert_eq!(after.page_count().unwrap(), 1);
}

/// A very deep chain of structure elements or fields overflowing the stack, which is how a recursive walk dies on a small hostile file.
#[test]
fn very_deep_trees_do_not_overflow_the_stack() {
    const DEPTH: u32 = 20_000;
    let mut objects = Vec::new();
    for number in 0..DEPTH {
        objects.push(obj(1_000 + number, &format!("<< /S /P /K [{} 0 R] >>", 1_001 + number)));
        objects.push(obj(100_000 + number, &format!("<< /FT /Tx /Kids [{} 0 R] >>", 100_001 + number)));
        objects.push(obj(200_000 + number, &format!("<< /Kids [{} 0 R] >>", 200_001 + number)));
        objects.push(obj(300_000 + number, &format!("<< /Title (x) /Parent 10 0 R /First {} 0 R >>", 300_001 + number)));
    }
    let bytes = two_pages(
        "/StructTreeRoot << /K [1000 0 R] >> /AcroForm << /Fields [100000 0 R] >> /Names << /Dests 200000 0 R >> /Outlines 10 0 R",
        &{
            objects.push(obj(10, "<< /First 300000 0 R /Last 300000 0 R >>"));
            objects
        },
        "",
        "",
    );
    let (_, _, after) = rewrite(&bytes, remove_second_page);
    assert_eq!(after.page_count().unwrap(), 1);
}

/// Arbitrary damage to good files falling over, or finishing with something that doesn't hold up: the same check the fuzz target runs.
#[test]
fn damaged_files_never_panic_and_never_give_a_bad_result() {
    let mut random = 0x1234_5678_9ABC_DEF0_u64;
    let mut next = move || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random
    };
    let seeds: Vec<Vec<u8>> = ALL_FORMS.iter().map(|form| rich_document(*form)).collect();
    let mut produced = 0;
    for seed in &seeds {
        assert!(property::check_rewrite_on_any_bytes(seed), "an undamaged file has to come out");
        for _ in 0..300 {
            let mut bytes = seed.clone();
            // Most damage that moves a byte breaks the cross-reference data, and that's refused. So half of it doesn't.
            let moves_bytes = next() % 2;
            for _ in 0..(next() % 8 + 1) {
                let at = (next() % bytes.len() as u64) as usize;
                match next() % (2 + moves_bytes) {
                    0 => bytes[at] = next() as u8,
                    1 => bytes[at] = b"0123456789 /<>[]R"[(next() % 17) as usize],
                    _ => {
                        let end = (at + (next() % 64) as usize).min(bytes.len());
                        bytes.drain(at..end);
                    }
                }
            }
            produced += usize::from(property::check_rewrite_on_any_bytes(&bytes));
        }
    }
    assert!(produced > 50, "only {produced} of 900 damaged files got through: the check isn't reaching the writer");
}
