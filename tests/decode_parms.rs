//! `/DecodeParms` in every shape ISO 32000-1, 7.4.1 allows.
//!
//! The key may be a single dictionary or an **array parallel to `/Filter`**,
//! with `null` for the layers that take no parameters. Reading only the
//! dictionary form makes a chained predictor decode to the wrong bytes with no
//! error at all, so each case here pins the decoded payload *and* shows that the
//! parameter is live — the same stream with `/DecodeParms` dropped decodes to
//! something else.

use lopdf::{DecompressError, Dictionary, Document, Error, Object, Stream, dictionary};

const ROW: usize = 8;

fn flate_encode(data: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn lzw_encode_late_change(data: &[u8]) -> Vec<u8> {
    weezl::encode::Encoder::new(weezl::BitOrder::Msb, 8)
        .encode(data)
        .unwrap()
}

fn ascii_hex_encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() * 2 + 1);
    for byte in data {
        output.extend_from_slice(format!("{byte:02X}").as_bytes());
    }
    output.push(b'>');
    output
}

/// Apply the PNG `Up` filter (type 2) to every row, which is what a
/// `/Predictor 12` stream carries.
fn png_up_predict(data: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() / row_bytes * (row_bytes + 1));
    let mut previous = vec![0_u8; row_bytes];
    for row in data.chunks(row_bytes) {
        output.push(2);
        for (index, byte) in row.iter().enumerate() {
            output.push(byte.wrapping_sub(previous[index]));
        }
        previous.copy_from_slice(row);
    }
    output
}

/// A payload that is a whole number of predictor rows and whose rows differ, so
/// running the predictor and skipping it cannot coincide.
fn payload() -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in 0..6_u8 {
        for column in 0..ROW as u8 {
            bytes.push(b'A' + (row * 7 + column * 3) % 26);
        }
    }
    bytes
}

fn png_predictor_parms() -> Dictionary {
    dictionary! {
        "Predictor" => 12_i64,
        "Columns" => ROW as i64,
        "Colors" => 1_i64,
        "BitsPerComponent" => 8_i64,
    }
}

/// Decode `stream` unbounded and bounded; both routes share `decode_filters`, so
/// they must agree.
fn decoded(stream: &Stream) -> Vec<u8> {
    let plain = stream.decompressed_content().expect("stream must decode");
    let bounded = stream
        .decompressed_content_with_limit(1 << 20)
        .expect("the bounded route must decode too");
    assert_eq!(plain, bounded, "bounded and unbounded decode must agree");
    plain
}

/// The same stream with `/DecodeParms` removed — what a reader that cannot see
/// the array form effectively decodes.
fn decoded_without_parms(stream: &Stream) -> Vec<u8> {
    let mut stripped = stream.clone();
    stripped.dict.remove(b"DecodeParms");
    stripped.decompressed_content().expect("stream must decode")
}

#[test]
fn array_decode_parms_reach_the_matching_filter_layer() {
    let expected = payload();
    let content = ascii_hex_encode(&flate_encode(&png_up_predict(&expected, ROW)));
    let stream = Stream::new(
        dictionary! {
            "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => vec![Object::Null, Object::Dictionary(png_predictor_parms())],
        },
        content,
    );

    assert_eq!(decoded(&stream), expected);
    // The premise: the predictor is doing work, so dropping the array is not a
    // harmless simplification but a silent wrong answer.
    assert_ne!(decoded_without_parms(&stream), expected);
}

#[test]
fn a_single_filter_accepts_its_parms_as_a_one_element_array() {
    let expected = payload();
    let content = flate_encode(&png_up_predict(&expected, ROW));
    let stream = Stream::new(
        dictionary! {
            "Filter" => vec![Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => vec![Object::Dictionary(png_predictor_parms())],
        },
        content.clone(),
    );
    assert_eq!(decoded(&stream), expected);
    assert_ne!(decoded_without_parms(&stream), expected);

    // …and as the bare dictionary, which is the form this key has always had.
    let dictionary_form = Stream::new(
        dictionary! {
            "Filter" => "FlateDecode",
            "DecodeParms" => png_predictor_parms(),
        },
        content,
    );
    assert_eq!(decoded(&dictionary_form), expected);
}

#[test]
fn a_lone_dictionary_still_reaches_the_terminal_layer_of_a_chain() {
    // Non-conforming but common: one dictionary against an array `/Filter`. Only
    // the Flate layer reads parameters, so applying it to every layer is
    // unambiguous — and is what this reader did before the array form was
    // understood, so no document regresses.
    let expected = payload();
    let content = ascii_hex_encode(&flate_encode(&png_up_predict(&expected, ROW)));
    let stream = Stream::new(
        dictionary! {
            "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => png_predictor_parms(),
        },
        content,
    );
    assert_eq!(decoded(&stream), expected);
}

/// A payload long and varied enough that the LZW code table crosses a code-size
/// boundary, which is the only place `/EarlyChange` is observable.
fn lzw_payload() -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    (0..8192)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn array_decode_parms_carry_early_change_to_lzw() {
    let expected = lzw_payload();
    let stream = Stream::new(
        dictionary! {
            "Filter" => vec![Object::Name(b"LZWDecode".to_vec())],
            "DecodeParms" => vec![Object::Dictionary(dictionary! { "EarlyChange" => 0_i64 })],
        },
        lzw_encode_late_change(&expected),
    );
    assert_eq!(decoded(&stream), expected);
    // `/EarlyChange` defaults to 1, i.e. the other code-size switch, so dropping
    // the array desynchronises the decoder rather than erroring.
    assert_ne!(decoded_without_parms(&stream), expected);
}

#[test]
fn entries_that_are_not_dictionaries_leave_their_layer_on_its_defaults() {
    let expected = payload();
    let encoded = ascii_hex_encode(&flate_encode(&expected));

    // A short array, a `null` opposite the layer that would read it, and an
    // empty array all mean "this layer decodes with its defaults" — which for
    // an unpredicted stream is the right answer. An indirect reference does
    // *not* belong in this list: it names parameters that exist, so guessing
    // defaults for it is a wrong answer, not a lenient one (see
    // `an_indirect_entry_is_refused_rather_than_decoded_on_defaults`).
    for parms in [
        Object::Array(vec![Object::Null]),
        Object::Array(vec![Object::Null, Object::Null]),
        Object::Array(vec![]),
        Object::Null,
    ] {
        let stream = Stream::new(
            dictionary! {
                "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
                "DecodeParms" => parms.clone(),
            },
            encoded.clone(),
        );
        assert_eq!(decoded(&stream), expected, "{parms:?}");
    }
}

/// An indirect `/DecodeParms` entry is legal (7.4.1) and names parameters that exist. A bare
/// `Stream` has nothing to resolve it against, and the two things it could do instead are not
/// equal: decoding the layer on its defaults returns bytes that look fine and are wrong, which
/// is the exact failure mode the array form was taught to avoid. So it says so.
#[test]
fn an_indirect_entry_is_refused_rather_than_decoded_on_defaults() {
    let expected = payload();
    let predicted = ascii_hex_encode(&flate_encode(&png_up_predict(&expected, ROW)));

    let array_form = Stream::new(
        dictionary! {
            "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => vec![Object::Null, Object::Reference((9, 0))],
        },
        predicted.clone(),
    );
    assert!(matches!(
        array_form.decompressed_content(),
        Err(Error::Decompress(DecompressError::UnresolvedDecodeParms { index: 1 }))
    ));
    assert!(matches!(
        array_form.decompressed_content_with_limit(1 << 20),
        Err(Error::Decompress(DecompressError::UnresolvedDecodeParms { index: 1 }))
    ));

    // The whole value as a reference is the same question with one filter's worth of context.
    let whole_value = Stream::new(
        dictionary! {
            "Filter" => "FlateDecode",
            "DecodeParms" => Object::Reference((9, 0)),
        },
        flate_encode(&png_up_predict(&expected, ROW)),
    );
    assert!(matches!(
        whole_value.decompressed_content(),
        Err(Error::Decompress(DecompressError::UnresolvedDecodeParms { index: 0 }))
    ));
}

/// The same two streams, decoded through the document that holds the parameters: the reference
/// resolves, the predictor runs, and the bytes are the ones the file encoded.
#[test]
fn a_document_resolves_an_indirect_decode_parms_entry() {
    let expected = payload();
    let mut document = Document::with_version("1.7");
    let parms = document.add_object(Object::Dictionary(png_predictor_parms()));
    let content = document.add_object(Object::Stream(Stream::new(
        dictionary! {
            "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => vec![Object::Null, Object::Reference(parms)],
        },
        ascii_hex_encode(&flate_encode(&png_up_predict(&expected, ROW))),
    )));
    let pages_id = document.new_object_id();
    let page = document.add_object(Object::Dictionary(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "Contents" => Object::Reference(content),
    }));
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1_i64,
        }),
    );
    let catalog = document.add_object(Object::Dictionary(dictionary! {
        "Type" => "Catalog", "Pages" => Object::Reference(pages_id),
    }));
    document.trailer.set("Root", Object::Reference(catalog));

    // The route a caller takes: page content, bounded and unbounded.
    let mut with_newline = expected.clone();
    with_newline.push(b'\n');
    assert_eq!(document.get_page_content(page), with_newline);
    assert_eq!(
        document.get_page_content_with_limit(page, 1 << 20).unwrap(),
        with_newline
    );

    // And the stream route the document lends its resolver to.
    let stream = document.get_object(content).unwrap().as_stream().unwrap();
    assert_eq!(stream.decompressed_content_with_document(&document).unwrap(), expected);
    assert_eq!(
        stream
            .decompressed_content_with_document_and_limit(&document, 1 << 20)
            .unwrap(),
        expected
    );
    // The premise: without the reference resolved these bytes are not obtainable at all.
    assert!(stream.decompressed_content().is_err());
}

#[test]
fn a_full_document_round_trips_a_chained_predictor_stream() {
    // The end-to-end shape: an object stream whose members are only reachable if
    // the predictor named in the array reaches the Flate layer.
    let expected = payload();
    let mut stream = Stream::new(
        dictionary! {
            "Filter" => vec!["ASCIIHexDecode".into(), Object::Name(b"FlateDecode".to_vec())],
            "DecodeParms" => vec![Object::Null, Object::Dictionary(png_predictor_parms())],
        },
        ascii_hex_encode(&flate_encode(&png_up_predict(&expected, ROW))),
    );
    stream.set_content(stream.content.clone());
    assert_eq!(stream.get_plain_content().unwrap(), expected);
    assert_eq!(stream.get_plain_content_with_limit(1 << 20).unwrap(), expected);
}
