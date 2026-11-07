use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, StringFormat, dictionary};

// Taken from a real PDF. The `0 beginbfrange ... endbfrange` sections triggered parse failures
// and broke text extraction before v0.38.0.
const FONT1_TOUNICODE: &str = r#"/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo
<< /Registry (Adobe)
/Ordering (UCS)
/Supplement 0
>> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
0 beginbfrange
endbfrange
2 beginbfchar
<0003> <0020>
<0006> <0023>
endbfchar
3 beginbfrange
<0008> <0009> <0025>
<000b> <000c> <0028>
<000f> <0016> <002c>
endbfrange
3 beginbfchar
<001a> <0037>
<001d> <003a>
<0022> <003f>
endbfchar
3 beginbfrange
<0024> <002a> <0041>
<002c> <002d> <0049>
<002f> <0031> <004c>
endbfrange
1 beginbfchar
<0033> <0050>
endbfchar
1 beginbfrange
<0035> <0038> <0052>
endbfrange
2 beginbfchar
<003a> <0057>
<003c> <0059>
endbfchar
2 beginbfrange
<0044> <004c> <0061>
<004e> <005d> <006b>
endbfrange
endcmap
CMapName currentdict /CMap defineresource pop
end
end
"#;

const FONT2_TOUNICODE: &str = r#"/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo
<< /Registry (Adobe)
/Ordering (UCS)
/Supplement 0
>> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
0 beginbfrange
endbfrange
2 beginbfchar
<0003> <0020>
<0007> <0024>
endbfchar
2 beginbfrange
<000a> <000c> <0027>
<000e> <0019> <002b>
endbfrange
1 beginbfchar
<001b> <0038>
endbfchar
5 beginbfrange
<001d> <001f> <003a>
<0021> <0022> <003e>
<0024> <002c> <0041>
<002e> <003b> <004b>
<0044> <005d> <0061>
endbfrange
2 beginbfchar
<0061> <007e>
<00bc> <20ac>
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end
"#;

fn build_doc_with_tounicode(tounicode: &str, encoded_text: Vec<u8>) -> Document {
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();

    let tounicode_bytes = tounicode.as_bytes().to_vec();
    let tounicode_stream_id = doc.add_object(Stream::new(
        dictionary! { "Length" => tounicode_bytes.len() as i64 },
        tounicode_bytes,
    ));

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => "DummyFont",
        "Encoding" => "Identity-H",
        "ToUnicode" => Object::Reference(tounicode_stream_id),
    });

    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });

    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![50.into(), 700.into()]),
            Operation::new("Tj", vec![Object::String(encoded_text, StringFormat::Hexadecimal)]),
            Operation::new("ET", vec![]),
        ],
    };

    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Resources" => resources_id,
        "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    });

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    doc
}

#[test]
fn extract_text_does_not_error_with_empty_bfrange_font2() {
    // 2-byte codes (Identity-H): space, $, ' (bfrange), 8, ~ and € (bfchar).
    let encoded_text = vec![
        0x00, 0x03, // space
        0x00, 0x07, // $
        0x00, 0x0a, // '
        0x00, 0x1b, // 8
        0x00, 0x61, // ~
        0x00, 0xbc, // €
    ];

    let doc = build_doc_with_tounicode(FONT2_TOUNICODE, encoded_text);

    let text = doc.extract_text(&[1]).expect("extract_text should not error");
    assert_eq!(text.trim_end(), " $'8~€");
}

#[test]
fn extract_text_does_not_error_with_empty_bfrange_font1() {
    // Exercises both bfchar and bfrange plus an initial empty bfrange: space, #, % (bfrange
    // <0008> <0009> <0025>), 7 (bfchar), a (bfrange <0044> <004c> <0061>).
    let encoded_text = vec![
        0x00, 0x03, // space
        0x00, 0x06, // #
        0x00, 0x08, // %
        0x00, 0x1a, // 7
        0x00, 0x44, // a
    ];

    let doc = build_doc_with_tounicode(FONT1_TOUNICODE, encoded_text);

    let text = doc.extract_text(&[1]).expect("extract_text should not error");
    assert_eq!(text.trim_end(), " #%7a");
}

fn tounicode_with_codespaces(codespaces: &str, mappings: &str) -> String {
    format!(
        "/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Test def
/CMapType 2 def
{} begincodespacerange
{codespaces}
endcodespacerange
{} beginbfchar
{mappings}
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end",
        codespaces.lines().count(),
        mappings.lines().count(),
    )
}

#[test]
fn extract_text_preserves_characters_after_unmapped_codes() {
    let cmap = tounicode_with_codespaces("<0000> <FFFF>", "<0001> <0041>\n<0002> <0042>");
    for (codes, expected) in [
        (vec![1, 2], "AB"),
        (vec![0, 1, 2], "\u{fffd}AB"),
        (vec![1, 0, 2], "A\u{fffd}B"),
        (vec![1, 3, 2], "A\u{fffd}B"),
        (vec![1, 0, 0, 2], "A\u{fffd}\u{fffd}B"),
        (vec![1, 2, 0], "AB\u{fffd}"),
    ] {
        let encoded_text = codes.into_iter().flat_map(u16::to_be_bytes).collect();
        let mut doc = build_doc_with_tounicode(&cmap, encoded_text);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let loaded = Document::load_mem(&bytes).unwrap();
        assert_eq!(loaded.extract_text(&[1]).unwrap(), format!("{expected}\n"));
    }
}

#[test]
fn decode_text_preserves_mixed_length_codes_after_unmapped_codes() {
    let cmap = tounicode_with_codespaces(
        "<00> <7F>\n<8140> <82FE>\n<820000> <8200FF>\n<83000000> <83FFFFFF>",
        "<41> <0041>\n<8140> <0042>\n<820001> <0043>\n<83000001> <0044>",
    );
    let mut doc = Document::new();
    let cmap_id = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
    let font = dictionary! { "Type" => "Font", "ToUnicode" => cmap_id };
    let encoding = font.get_font_encoding(&doc).unwrap();
    let encoded_text = [
        0x00, 0x41, 0x81, 0x41, 0x81, 0x40, 0x82, 0x00, 0x00, 0x82, 0x00, 0x01, 0x83, 0x00, 0x00, 0x00, 0x83, 0x00,
        0x00, 0x01,
    ];
    assert_eq!(
        Document::decode_text(&encoding, &encoded_text).unwrap(),
        "\u{fffd}A\u{fffd}B\u{fffd}C\u{fffd}D"
    );
}

#[test]
fn encoding_dictionary_entries_are_optional() {
    // ISO 32000-1, Table 114: /Type, /BaseEncoding and /Differences may all be omitted.
    let doc = Document::new();
    for (encoding, expected) in [
        (dictionary! { "Differences" => vec![65.into(), "B".into()] }, "B\u{d8}"),
        (dictionary! { "BaseEncoding" => "WinAnsiEncoding" }, "A\u{e9}"),
        (
            dictionary! { "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding" },
            "A\u{e9}",
        ),
    ] {
        let font = dictionary! { "Type" => "Font", "Encoding" => encoding };
        let encoding = font.get_font_encoding(&doc).unwrap();
        assert_eq!(Document::decode_text(&encoding, b"A\xe9").unwrap(), expected);
    }
}

/// Regression from the #580 codespace check: a code the CMap maps must decode whatever width the
/// codespace declares. Both shapes occur in real files: 20 of 170 files in distillPDF's corpus
/// declare `<0000> <FFFF>` over one-byte bfchars.
#[test]
fn decode_text_keeps_mapped_codes_outside_the_declared_codespace_width() {
    let decode = |codespaces: &str, mappings: &str, bytes: &[u8]| {
        let cmap = tounicode_with_codespaces(codespaces, mappings);
        let mut doc = Document::new();
        let cmap_id = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
        let font = dictionary! { "Type" => "Font", "ToUnicode" => cmap_id };
        let encoding = font.get_font_encoding(&doc).unwrap();
        Document::decode_text(&encoding, bytes).unwrap()
    };
    // A two-byte codespace over one-byte mappings.
    assert_eq!(decode("<0000> <FFFF>", "<41> <0058>\n<42> <0059>", b"AB"), "XY");
    // Overlapping widths: an unmapped first byte can still begin a wider mapped code.
    assert_eq!(
        decode("<00> <FF>\n<0000> <FFFF>", "<0102> <0041>\n<03> <0043>", &[1, 2, 3]),
        "AC"
    );
    // Overlapping widths, nothing mapped: one replacement per widest code, nothing lost after it.
    assert_eq!(
        decode("<00> <FF>\n<0000> <FFFF>", "<03> <0043>", &[0xFF, 0xFF, 3]),
        "\u{fffd}C"
    );
}

/// The codespace of a CMap that takes codes of any width, as a mixed-width font's does.
const ANY_WIDTH: &str = "<00> <FF>\n<0000> <FFFF>\n<000000> <FFFFFF>\n<00000000> <FFFFFFFF>";

/// Decode through `Document`'s route for a simple font with this CMap, with or without an `/Encoding`.
fn decode_simple_font(codespaces: &str, mappings: &str, encoding: Option<&str>, bytes: &[u8]) -> String {
    let cmap = tounicode_with_codespaces(codespaces, mappings);
    let mut doc = Document::new();
    let cmap_id = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
    let mut font = dictionary! { "Type" => "Font", "Subtype" => "Type1", "ToUnicode" => cmap_id };
    if let Some(encoding) = encoding {
        font.set("Encoding", encoding);
    }
    let encoding = font.get_font_encoding(&doc).unwrap();
    Document::decode_text(&encoding, bytes).unwrap()
}

/// Upstream's #661 narrowed a simple font's CMap to one-byte codes, which threw away every mapping wider than a byte. A font with a real mixed-width
/// CMap then read as replacement characters: `ABC😀` became `A` and a string of them. Found by distillPDF on moving to this merge.
#[test]
fn a_simple_fonts_multi_byte_mappings_survive() {
    let mappings = "<01> <0041>\n<0203> <0042>\n<040506> <0043>\n<0708090A> <D83DDE00>\n<0B> <0044>";
    let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0xA, 0xFF, 0xFF, 0xFF, 0xFF, 0xC];
    assert_eq!(
        decode_simple_font(ANY_WIDTH, mappings, None, &bytes),
        "ABC😀\u{fffd}\u{fffd}"
    );

    // A two-byte code to two characters, then a one-byte code, which used to read as two replacements and `C`.
    assert_eq!(
        decode_simple_font(ANY_WIDTH, "<0102> <00410042>\n<03> <0043>", None, &[1, 2, 3]),
        "ABC"
    );
}

/// `Document` losing what `OwnedEncoding` keeps. With no `/Encoding` in the way, both routes read the same CMap, so they have to agree.
#[test]
fn both_routes_read_a_simple_fonts_multi_byte_cmap_alike() {
    for (mappings, bytes) in [
        (
            "<01> <0041>\n<0203> <0042>\n<040506> <0043>\n<0708090A> <D83DDE00>\n<0B> <0044>",
            &[1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 0xA, 0xFF, 0xFF, 0xFF, 0xFF, 0xC][..],
        ),
        ("<0102> <00410042>\n<03> <0043>", &[1, 2, 3][..]),
    ] {
        let cmap = tounicode_with_codespaces(ANY_WIDTH, mappings);
        let owned = lopdf::OwnedEncoding::from_to_unicode_for_extraction(cmap.into_bytes()).unwrap();
        let mut from_owned = String::new();
        owned.write_to_string(bytes, &mut from_owned).unwrap();
        assert_eq!(decode_simple_font(ANY_WIDTH, mappings, None, bytes), from_owned);
    }
}

/// Keeping wide mappings costing what #661 fixed: a key like `<0041>` still means the one-byte code 0x41, beside a real two-byte key in the same CMap. Each
/// has to work, and neither may take the other's bytes.
#[test]
fn padded_keys_and_real_wide_keys_work_side_by_side() {
    let mappings = "<0041> <0391>\n<0203> <0392>\n<43> <0393>";
    assert_eq!(
        decode_simple_font(ANY_WIDTH, mappings, None, &[0x41, 2, 3, 0x43]),
        "\u{0391}\u{0392}\u{0393}"
    );
}

/// `/Encoding` taking a byte that begins a wide code, which would make that code unreachable, or wide mappings switching off the per-code fallback that
/// ISO 32000-1 9.10.2 calls for.
#[test]
fn encoding_fills_only_the_codes_no_mapping_can_use() {
    let mappings = "<41> <0391>\n<0203> <0392>";
    assert_eq!(
        decode_simple_font(ANY_WIDTH, mappings, Some("WinAnsiEncoding"), &[0x41, 0x42, 2, 3]),
        "\u{0391}B\u{0392}"
    );
}

/// A one-byte CMap, the ordinary case, staying exactly as #661 made it: the codespace is one byte and the rest comes from `/Encoding`.
#[test]
fn an_all_one_byte_cmap_still_falls_back_per_code() {
    assert_eq!(
        decode_simple_font(
            "<0000> <FFFF>",
            "<0041> <0391>\n<42> <0392>",
            Some("WinAnsiEncoding"),
            b"ABC"
        ),
        "\u{0391}\u{0392}C"
    );
}

/// The owned route for a simple font, given what the font's `/Encoding` is, built the way a caller builds it.
fn owned_simple_font(
    codespaces: &str, mappings: &str, encoding: Option<&lopdf::OwnedEncoding>, bytes: &[u8],
) -> String {
    let cmap = tounicode_with_codespaces(codespaces, mappings);
    let owned = lopdf::OwnedEncoding::for_simple_font_text(cmap.into_bytes(), encoding).unwrap();
    let mut text = String::new();
    owned.write_to_string(bytes, &mut text).unwrap();
    text
}

/// A key like `<0041>` on one-byte content, as many producers write: the owned route took 0x41 as the start of a two-byte code and lost it, where
/// `Document` reads it.
#[test]
fn the_owned_route_reads_a_padded_key_on_one_byte_content() {
    let text = owned_simple_font("<0000> <FFFF>", "<0041> <0391>\n<42> <0392>", None, b"AB");
    assert_eq!(text, "\u{0391}\u{0392}");
}

/// A code the CMap leaves out coming back as a replacement character, when ISO 32000-1 9.10.2 goes on to `/Encoding` for it.
#[test]
fn the_owned_route_takes_codes_the_cmap_leaves_out_from_encoding() {
    let winansi = lopdf::OwnedEncoding::from_named(b"WinAnsiEncoding", None).unwrap();
    let text = owned_simple_font("<00> <FF>", "<41> <0391>", Some(&winansi), b"AB");
    assert_eq!(text, "\u{0391}B");
}

/// The rule the caller is asked to rely on: on a file whose `/Encoding` and `/ToUnicode` disagree, the text is the CMap's, on both routes, per code.
#[test]
fn the_cmap_decides_the_text_where_it_disagrees_with_encoding() {
    let winansi = lopdf::OwnedEncoding::from_named(b"WinAnsiEncoding", None).unwrap();
    assert_eq!(
        owned_simple_font("<00> <FF>", "<41> <005A>", Some(&winansi), b"AB"),
        "ZB"
    );
    assert_eq!(
        decode_simple_font("<00> <FF>", "<41> <005A>", Some("WinAnsiEncoding"), b"AB"),
        "ZB"
    );
}

/// A CMap that can't be parsed being taken for an empty one, when a caller has to know to use `/Encoding` alone, as `Document` does.
#[test]
fn the_owned_route_reports_a_cmap_it_cant_parse() {
    let broken = b"not a cmap <<<".to_vec();
    assert!(lopdf::OwnedEncoding::for_simple_font_text(broken, None).is_err());
}

/// A compact range over every four-byte code being enumerated, which a reverse map would do: a few bytes from a hostile file would take the machine. This route
/// only ever reads text, so it mustn't build one. On its own thread with a limit, so a regression fails instead of hanging the run.
#[test]
fn the_owned_route_doesnt_enumerate_a_huge_range() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let cmap = tounicode_with_codespaces("<00000000> <FFFFFFFF>", "").replace(
            "endbfchar",
            "endbfchar\n1 beginbfrange\n<00000000> <FFFFFFFF> <0041>\nendbfrange",
        );
        let owned = lopdf::OwnedEncoding::for_simple_font_text(cmap.into_bytes(), None).unwrap();
        let mut text = String::new();
        owned.write_to_string(&[0, 0, 0, 1], &mut text).unwrap();
        done.send(text).unwrap();
    });
    let text = finished
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("building the map took over a minute, or panicked");
    assert_eq!(text, "B");
}

/// What a caller passes for each kind of `/Encoding`, beside the object a font holds for it.
fn encoding_fixtures() -> Vec<(&'static str, Option<Object>, Option<lopdf::OwnedEncoding>)> {
    let differences = vec![
        Object::Integer(65),
        Object::Name(b"B".to_vec()),
        Object::Name(b"eacute".to_vec()),
    ];
    let named = |name: &'static str| {
        (
            name,
            Some(Object::Name(name.as_bytes().to_vec())),
            Some(lopdf::OwnedEncoding::from_named(name.as_bytes(), None).unwrap()),
        )
    };
    vec![
        ("none", None, None),
        named("WinAnsiEncoding"),
        named("MacRomanEncoding"),
        named("MacExpertEncoding"),
        named("StandardEncoding"),
        named("PDFDocEncoding"),
        named("UniGB-UTF16-H"),
        named("AnUnknownName"),
        (
            "differences over standard",
            Some(Object::Dictionary(
                dictionary! { "Type" => "Encoding", "Differences" => differences.clone() },
            )),
            Some(lopdf::OwnedEncoding::standard().with_differences(&differences).unwrap()),
        ),
        (
            "differences over winansi",
            Some(Object::Dictionary(dictionary! {
                "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding", "Differences" => differences.clone(),
            })),
            Some(
                lopdf::OwnedEncoding::from_named(b"WinAnsiEncoding", None)
                    .unwrap()
                    .with_differences(&differences)
                    .unwrap(),
            ),
        ),
    ]
}

/// The two routes drifting apart on any shape of CMap, `/Encoding` or content, which is what made one engine read `A` and the other `Z`. Every combination, over every
/// one-byte code and some wider runs, against `Document` as the reference.
#[test]
fn both_routes_read_every_fixture_alike() {
    let cmaps = [
        ("<00> <FF>", "<41> <005A>"),
        ("<00> <FF>", "<41> <0391>\n<42> <D83DDE00>\n<43> <00410042>"),
        ("<0000> <FFFF>", "<0041> <0391>\n<42> <0392>"),
        ("<0000> <FFFF>", "<41> <0058>\n<42> <0059>"),
        ("<0000> <FFFF>", "<0102> <00410042>\n<03> <0043>"),
        ("<00> <FF>\n<0000> <FFFF>", "<0102> <0041>\n<03> <0043>"),
        (
            ANY_WIDTH,
            "<01> <0041>\n<0203> <0042>\n<040506> <0043>\n<0708090A> <D83DDE00>\n<0B> <0044>",
        ),
        (ANY_WIDTH, "<0041> <0391>\n<0203> <0392>\n<43> <0393>"),
        (ANY_WIDTH, "<41> <0391>\n<0203> <0392>"),
        ("<00> <FF>", ""),
    ];
    let every_code: Vec<u8> = (0..=255).collect();
    let runs: [&[u8]; 4] = [
        &every_code,
        b"ABC",
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 0xA, 0xFF, 0xFF, 0xFF, 0xFF, 0xC],
        &[0x41, 0x42, 2, 3, 0x43, 0, 0x41],
    ];
    let mut compared = 0;
    for subtype in ["Type1", "TrueType", "MMType1", "Type3"] {
        for (codespaces, mappings) in cmaps {
            for (label, object, owned) in encoding_fixtures() {
                let cmap = tounicode_with_codespaces(codespaces, mappings);
                let mut doc = Document::new();
                let cmap_id = doc.add_object(Stream::new(dictionary! {}, cmap.clone().into_bytes()));
                let mut font = dictionary! { "Type" => "Font", "Subtype" => subtype, "ToUnicode" => cmap_id };
                if let Some(object) = object {
                    font.set("Encoding", object);
                }
                let from_document = font.get_font_encoding(&doc).unwrap();
                let from_owned = lopdf::OwnedEncoding::for_simple_font_text(cmap.into_bytes(), owned.as_ref()).unwrap();
                for bytes in runs {
                    let mut text = String::new();
                    from_owned.write_to_string(bytes, &mut text).unwrap();
                    assert_eq!(
                        text,
                        Document::decode_text(&from_document, bytes).unwrap(),
                        "{subtype}, {codespaces:?}, {mappings:?}, {label}, {bytes:?}"
                    );
                    compared += 1;
                }
            }
        }
    }
    assert_eq!(compared, 4 * 10 * 10 * 4, "a fixture was skipped");
}

/// The owned encoding a caller builds for a real font, by the rules `Document` follows: an `/Encoding` that can't be read is StandardEncoding, and a CMap that can't be read
/// leaves `/Encoding` alone.
fn owned_for_real_font(font: &lopdf::Dictionary, doc: &Document) -> lopdf::OwnedEncoding {
    let deref = |object: &Object| match object {
        Object::Reference(id) => doc.get_object(*id).ok().cloned(),
        other => Some(other.clone()),
    };
    let encoding = font.get(b"Encoding").ok().map(|object| {
        let built = deref(object).and_then(|object| match object {
            Object::Name(name) => lopdf::OwnedEncoding::from_named(&name, None).ok(),
            Object::Dictionary(dict) => {
                let base = match dict.get(b"BaseEncoding").ok().and_then(|base| base.as_name().ok()) {
                    Some(name) => lopdf::OwnedEncoding::from_named(name, None).ok()?,
                    None => lopdf::OwnedEncoding::standard(),
                };
                match dict.get(b"Differences").ok().and_then(deref) {
                    Some(Object::Array(differences)) => base.with_differences(&differences).ok(),
                    Some(_) => None,
                    None => Some(base),
                }
            }
            _ => None,
        });
        built.unwrap_or_else(lopdf::OwnedEncoding::standard)
    });
    let content = font.get(b"ToUnicode").ok().and_then(deref).and_then(|object| {
        object
            .as_stream()
            .ok()
            .and_then(|stream| stream.get_plain_content().ok())
    });
    let from_cmap =
        content.and_then(|content| lopdf::OwnedEncoding::for_simple_font_text(content, encoding.as_ref()).ok());
    from_cmap.or(encoding).unwrap_or_else(lopdf::OwnedEncoding::standard)
}

/// Compare every simple font with a `/ToUnicode` in one document, over every code. Returns how many fonts there were.
fn compare_real_fonts(path: &std::path::Path, doc: &Document) -> usize {
    let every_code: Vec<u8> = (0..=255).collect();
    let mut fonts = 0;
    for (id, object) in &doc.objects {
        let Ok(font) = object.as_dict() else {
            continue;
        };
        let simple = matches!(
            font.get(b"Subtype").and_then(Object::as_name),
            Ok(b"Type1" | b"MMType1" | b"TrueType" | b"Type3")
        );
        if !font.has_type(b"Font") || !simple || !font.has(b"ToUnicode") {
            continue;
        }
        let Ok(from_document) = font.get_font_encoding(doc) else {
            continue;
        };
        let owned = owned_for_real_font(font, doc);
        for bytes in [&every_code[..], b"The quick brown fox"] {
            let mut text = String::new();
            let owned_result = owned.write_to_string(bytes, &mut text).map(|()| text);
            let document_result = Document::decode_text(&from_document, bytes);
            assert_eq!(
                owned_result.ok(),
                document_result.ok(),
                "{}, font {id:?}, {bytes:?}",
                path.display()
            );
        }
        fonts += 1;
    }
    fonts
}

/// The two routes disagreeing on fonts out of real PDFs, which no hand-made fixture would show. The PDFs in `assets` always, though none has such a font today, so it's
/// with `LOPDF_FONT_CORPUS` set to a folder, adding each PDF of up to 64 MiB in it, that this finds anything.
#[test]
fn both_routes_read_real_fonts_alike() {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir("assets")
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    if let Ok(folder) = std::env::var("LOPDF_FONT_CORPUS") {
        paths.extend(std::fs::read_dir(folder).unwrap().flatten().map(|entry| entry.path()));
    }
    let (mut documents, mut fonts) = (0, 0);
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "pdf") {
            continue;
        }
        if std::fs::metadata(&path).unwrap().len() > 64 * 1_024 * 1_024 {
            continue;
        }
        let Ok(doc) = Document::load_mem(&std::fs::read(&path).unwrap()) else {
            continue;
        };
        documents += 1;
        fonts += compare_real_fonts(&path, &doc);
    }
    println!("compared {fonts} simple fonts with /ToUnicode in {documents} documents");
    assert!(documents > 0, "no PDFs were read, so this checked nothing");
}
