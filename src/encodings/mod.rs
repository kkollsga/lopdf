pub mod cmap;
mod differences;
mod glyphnames;
mod mappings;

pub use self::differences::Differences;
pub use self::glyphnames::Glyph;
pub use self::mappings::*;
use crate::Error;
use crate::Object;
use crate::Result;
use crate::parser_aux::substr;
use cmap::ToUnicodeCMap;
use encoding_rs::UTF_16BE;
use indexmap::IndexMap;
use log::debug;

pub fn bytes_to_string(encoding: &CodedCharacterSet, bytes: &[u8], out: &mut String) -> Result<()> {
    for b in bytes {
        let Some(g) = encoding.get(*b as usize).copied().flatten() else {
            continue;
        };

        for ch in char::decode_utf16([g.utf16_code_unit()]).flatten() {
            out.push(ch);
        }
    }

    Ok(())
}

pub fn string_to_bytes(encoding: &CodedCharacterSet, text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    write_to_bytes(encoding, text, &mut out);
    out
}

pub fn write_to_bytes(encoding: &CodedCharacterSet, text: &str, out: &mut Vec<u8>) {
    for c in text.encode_utf16() {
        let g = Glyph::from_utf16_code_unit(c);

        let Some(n) = encoding.iter().position(|glyph| glyph.is_some_and(|f| f == g)) else {
            continue;
        };

        out.push(n as u8);
    }
}

pub enum Encoding<'a> {
    OneByteEncoding(&'a CodedCharacterSet),
    SimpleEncoding(&'a [u8]),
    UnicodeMapEncoding(ToUnicodeCMap),
    Differences(Differences<'a>),
}

/// An owned text encoding for callers that resolve PDF objects without retaining
/// borrows into a [`crate::Document`].
///
/// This type is deliberately limited to encoding construction and exact
/// byte-to-text conversion. It performs no object lookup, stream decoding, or
/// fallback policy: callers provide already-decoded ToUnicode bytes and the
/// resolved `/Differences` array.
pub struct OwnedEncoding {
    kind: OwnedEncodingKind,
}

enum OwnedEncodingKind {
    OneByte(&'static CodedCharacterSet),
    Simple(Vec<u8>),
    UnicodeMap(ToUnicodeCMap),
    Differences {
        base: Box<OwnedEncoding>,
        map: IndexMap<u8, Glyph>,
    },
}

impl std::fmt::Debug for OwnedEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            OwnedEncodingKind::OneByte(_) => f.debug_tuple("OwnedEncoding::OneByte").finish(),
            OwnedEncodingKind::Simple(name) => f.debug_tuple("OwnedEncoding::Simple").field(name).finish(),
            OwnedEncodingKind::UnicodeMap(_) => f.debug_tuple("OwnedEncoding::UnicodeMap").finish(),
            OwnedEncodingKind::Differences { .. } => f.debug_tuple("OwnedEncoding::Differences").finish(),
        }
    }
}

impl OwnedEncoding {
    /// Construct the standard one-byte encoding.
    pub fn standard() -> Self {
        Self {
            kind: OwnedEncodingKind::OneByte(&STANDARD_ENCODING),
        }
    }

    /// Construct an encoding from a resolved PDF encoding name.
    ///
    /// The five predefined one-byte names use the same tables as
    /// [`Dictionary::get_font_encoding`](crate::Dictionary::get_font_encoding).
    /// Every other name is retained as a simple encoding; this includes the two
    /// supported UniGB names and unknown names that return
    /// [`Error::CharacterEncoding`] when text is written. `Identity-H` and
    /// `Identity-V` require the already-decoded `/ToUnicode` bytes in
    /// `to_unicode`; other names ignore that argument.
    pub fn from_named(name: &[u8], to_unicode: Option<Vec<u8>>) -> Result<Self> {
        if matches!(name, b"Identity-H" | b"Identity-V") {
            let content = to_unicode.ok_or_else(|| Error::DictKey("ToUnicode".to_string()))?;
            return Self::from_to_unicode(content);
        }
        let kind = match name {
            b"StandardEncoding" => OwnedEncodingKind::OneByte(&STANDARD_ENCODING),
            b"MacRomanEncoding" => OwnedEncodingKind::OneByte(&MAC_ROMAN_ENCODING),
            b"MacExpertEncoding" => OwnedEncodingKind::OneByte(&MAC_EXPERT_ENCODING),
            b"WinAnsiEncoding" => OwnedEncodingKind::OneByte(&WIN_ANSI_ENCODING),
            b"PDFDocEncoding" => OwnedEncodingKind::OneByte(&PDF_DOC_ENCODING),
            name => OwnedEncodingKind::Simple(name.to_vec()),
        };
        Ok(Self { kind })
    }

    /// Parse already-decoded `/ToUnicode` stream bytes with lopdf's exact CMap
    /// parser and retain the resulting variable-width map.
    pub fn from_to_unicode(content: Vec<u8>) -> Result<Self> {
        Ok(Self {
            kind: OwnedEncodingKind::UnicodeMap(ToUnicodeCMap::parse(content)?),
        })
    }

    /// Apply a resolved encoding dictionary's required `/Differences` array.
    ///
    /// The caller owns the encoding dictionary's required-entry policy; this
    /// function validates the resolved array exactly.
    pub fn with_differences(self, differences: &[Object]) -> Result<Self> {
        let mut map = IndexMap::new();
        let mut current_code = 0;

        for object in differences {
            match object {
                Object::Integer(code) => {
                    if !(0..=255).contains(code) {
                        return Err(Error::InvalidEncodingDifferenceCode { code: *code });
                    }
                    current_code = *code as u8;
                }
                Object::Name(name) => {
                    let Some(glyph) = Glyph::from_name(name) else {
                        return Err(Error::InvalidEncodingDifferenceGlyph {
                            name: String::from_utf8_lossy(name).into_owned(),
                        });
                    };
                    map.insert(current_code, glyph);
                    current_code = current_code.wrapping_add(1);
                }
                object => {
                    return Err(Error::ObjectType {
                        expected: "Integer or Name",
                        found: object.enum_variant(),
                    });
                }
            }
        }

        Ok(Self {
            kind: OwnedEncodingKind::Differences {
                base: Box::new(self),
                map,
            },
        })
    }

    /// Decode bytes with the same tables, CMap recognition, replacement, and
    /// error behavior as [`Encoding::write_to_string`].
    pub fn write_to_string(&self, bytes: &[u8], out: &mut String) -> Result<()> {
        match &self.kind {
            OwnedEncodingKind::OneByte(map) => bytes_to_string(map, bytes, out),
            OwnedEncodingKind::Simple(name) => write_simple_to_string(name, bytes, out),
            OwnedEncodingKind::UnicodeMap(unicode_map) => write_unicode_map_to_string(unicode_map, bytes, out),
            OwnedEncodingKind::Differences { base, map } => {
                for byte in bytes {
                    let Some(glyph) = map.get(byte) else {
                        base.write_to_string(&[*byte], out)?;
                        continue;
                    };
                    write_glyph_to_string(*glyph, out);
                }
                Ok(())
            }
        }
    }
}

fn write_glyph_to_string(glyph: Glyph, out: &mut String) {
    for character in char::decode_utf16([glyph.utf16_code_unit()]).flatten() {
        out.push(character);
    }
}

fn write_simple_to_string(name: &[u8], bytes: &[u8], out: &mut String) -> Result<()> {
    match name {
        b"UniGB-UCS2-H" | b"UniGB-UTF16-H" => {
            out.push_str(UTF_16BE.decode(bytes).0.as_ref());
            Ok(())
        }
        b"WinAnsiEncoding" => bytes_to_string(&WIN_ANSI_ENCODING, bytes, out),
        _ => Err(Error::CharacterEncoding),
    }
}

fn write_unicode_map_to_string(unicode_map: &ToUnicodeCMap, bytes: &[u8], out: &mut String) -> Result<()> {
    let mut output_bytes = Vec::new();

    // Source codes can have a variadic length from 1 to 4 bytes.
    let mut bytes_in_considered_code = 0u8;
    let mut considered_source_code = 0u32;
    for byte in bytes {
        if bytes_in_considered_code == 4 {
            let mut value = unicode_map.get_or_replacement_char(considered_source_code, 4);
            considered_source_code = 0;
            bytes_in_considered_code = 0;
            output_bytes.append(&mut value);
        }
        bytes_in_considered_code += 1;
        considered_source_code = considered_source_code * 256 + *byte as u32;
        if let Some(mut value) = unicode_map.get(considered_source_code, bytes_in_considered_code) {
            considered_source_code = 0;
            bytes_in_considered_code = 0;
            output_bytes.append(&mut value);
        }
    }
    if bytes_in_considered_code > 0 {
        let mut value = unicode_map.get_or_replacement_char(considered_source_code, bytes_in_considered_code);
        output_bytes.append(&mut value);
    }
    let utf16_bytes: Vec<u8> = output_bytes
        .iter()
        .flat_map(|unit| [(unit / 256) as u8, (unit % 256) as u8])
        .collect();

    out.push_str(UTF_16BE.decode(&utf16_bytes).0.as_ref());
    Ok(())
}

impl std::fmt::Debug for Encoding<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // UnicodeCMap and Bytes encoding ommitted to not bloat debug log
            Self::OneByteEncoding(_arg0) => f.debug_tuple("OneByteEncoding").finish(),
            Self::SimpleEncoding(arg0) => f.debug_tuple("SimpleEncoding").field(arg0).finish(),
            Self::UnicodeMapEncoding(_arg0) => f.debug_tuple("UnicodeMapEncoding").finish(),
            Self::Differences(_arg0) => f.debug_tuple("Differences").finish(),
        }
    }
}

impl Encoding<'_> {
    pub fn bytes_to_string(&self, bytes: &[u8]) -> Result<String> {
        let mut out = String::new();
        self.write_to_string(bytes, &mut out)?;
        Ok(out)
    }

    pub fn write_to_string(&self, bytes: &[u8], out: &mut String) -> Result<()> {
        match self {
            Self::OneByteEncoding(map) => bytes_to_string(map, bytes, out),
            Self::SimpleEncoding(name) => write_simple_to_string(name, bytes, out),
            Self::UnicodeMapEncoding(unicode_map) => write_unicode_map_to_string(unicode_map, bytes, out),
            Self::Differences(differences) => differences.bytes_to_string(bytes, out),
        }
    }

    pub fn string_to_bytes(&self, text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.write_to_bytes(text, &mut bytes);
        bytes
    }

    pub fn write_to_bytes(&self, text: &str, out: &mut Vec<u8>) {
        match self {
            Self::OneByteEncoding(map) => write_to_bytes(map, text, out),
            Self::SimpleEncoding(b"UniGB-UCS2-H") | Self::SimpleEncoding(b"UniGB-UTF16-H") => {
                encode_utf16_be(text, out)
            }
            Self::SimpleEncoding(b"WinAnsiEncoding") => write_to_bytes(&WIN_ANSI_ENCODING, text, out),
            Self::UnicodeMapEncoding(unicode_map) => {
                let mut i = 0;
                while i < text.chars().count() {
                    let current_unicode_seq: Vec<u16> = substr(text, i, 1).encode_utf16().collect();

                    if let Some(entries) = unicode_map.get_source_codes_for_unicode(&current_unicode_seq) {
                        if let Some(entry) = entries.first() {
                            // TODO: Add logic to pick the best entry if multiple
                            let mut bytes_for_code = Vec::new();
                            let val = entry.source_code;
                            match entry.code_len {
                                1 => bytes_for_code.push(val as u8),
                                2 => bytes_for_code.extend_from_slice(&(val as u16).to_be_bytes()),
                                3 => {
                                    bytes_for_code.push((val >> 16) as u8);
                                    bytes_for_code.push((val >> 8) as u8);
                                    bytes_for_code.push(val as u8);
                                }
                                4 => bytes_for_code.extend_from_slice(&val.to_be_bytes()),
                                _ => { /* Should not happen */ }
                            }
                            out.extend(bytes_for_code);
                        } else {
                            // No specific entry, handle as unmappable
                            log::warn!(
                                "Unicode sequence {current_unicode_seq:04X?} found in map but no entries, skipping."
                            );
                        }
                    } else {
                        // Character or sequence not found in CMap
                        log::warn!(
                            "Unicode sequence {current_unicode_seq:04X?} not found in ToUnicode CMap, skipping."
                        );
                    }
                    i += 1;
                }
            }
            Self::SimpleEncoding(_) => {
                debug!("Unknown encoding used to encode text {self:?}");
                out.extend_from_slice(text.as_bytes());
            }
            Self::Differences(differences) => {
                differences.string_to_bytes(text, out);
            }
        }
    }
}

/// Encodes the given `str` to UTF-16BE.
/// The recommended way to encode text strings, as it supports all of
/// unicode and all major PDF readers support it.
pub fn encode_utf16_be(text: &str, out: &mut Vec<u8>) {
    // Prepend BOM to the mark string as UTF-16BE encoded.
    let bom_be: [u8; 2] = [0xFE, 0xFF];
    out.extend_from_slice(&bom_be);
    out.extend(text.encode_utf16().flat_map(|b| b.to_be_bytes()));
}

/// Encodes the given `str` to UTF-8. This method of encoding text strings
/// is first specified in PDF2.0 and reader support is still lacking
/// (notably, Adobe Acrobat Reader doesn't support it at the time of writing).
/// Thus, using it is **NOT RECOMMENDED**.
pub fn encode_utf8(text: &str) -> Vec<u8> {
    // Prepend BOM to the mark string as UTF-8 encoded.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(text.bytes());
    bytes
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::Document;

    fn exact_cmap() -> Vec<u8> {
        b"/CIDInit /ProcSet findresource begin\n\
          12 dict begin\n\
          begincmap\n\
          /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
          /CMapName /Owned-Exact def\n\
          /CMapType 2 def\n\
          4 begincodespacerange\n\
          <00> <FF>\n\
          <0000> <FFFF>\n\
          <000000> <FFFFFF>\n\
          <00000000> <FFFFFFFF>\n\
          endcodespacerange\n\
          4 beginbfchar\n\
          <01> <0041>\n\
          <0203> <0042>\n\
          <040506> <D83DDE00>\n\
          <0708090A> <0044>\n\
          endbfchar\n\
          endcmap\n\
          CMapName currentdict /CMap defineresource pop\n\
          end\n\
          end\n"
            .to_vec()
    }

    fn owned_text(encoding: &OwnedEncoding, bytes: &[u8]) -> Result<String> {
        let mut output = String::new();
        encoding.write_to_string(bytes, &mut output)?;
        Ok(output)
    }

    #[test]
    fn owned_named_tables_match_borrowed_encodings_for_every_byte() {
        let bytes: Vec<u8> = (0..=u8::MAX).collect();
        for (name, table) in [
            (b"StandardEncoding".as_slice(), &STANDARD_ENCODING),
            (b"MacRomanEncoding".as_slice(), &MAC_ROMAN_ENCODING),
            (b"MacExpertEncoding".as_slice(), &MAC_EXPERT_ENCODING),
            (b"WinAnsiEncoding".as_slice(), &WIN_ANSI_ENCODING),
            (b"PDFDocEncoding".as_slice(), &PDF_DOC_ENCODING),
        ] {
            let borrowed = Encoding::OneByteEncoding(table).bytes_to_string(&bytes).unwrap();
            let encoding = OwnedEncoding::from_named(name, None).unwrap();
            let owned = owned_text(&encoding, &bytes).unwrap();
            assert_eq!(owned, borrowed, "{}", String::from_utf8_lossy(name));
        }
    }

    #[test]
    fn owned_simple_encodings_match_unknown_and_unigb_replacement_behavior() {
        for name in [b"UniGB-UCS2-H".as_slice(), b"UniGB-UTF16-H".as_slice()] {
            // Includes a valid scalar, an unpaired high surrogate, and an odd trailing byte.
            let bytes = [0x00, 0x41, 0xd8, 0x00, 0x42];
            let borrowed = Encoding::SimpleEncoding(name).bytes_to_string(&bytes).unwrap();
            let encoding = OwnedEncoding::from_named(name, None).unwrap();
            let owned = owned_text(&encoding, &bytes).unwrap();
            assert_eq!(owned, borrowed);
            assert_eq!(owned, "A�");
        }

        let borrowed = Encoding::SimpleEncoding(b"UnknownEncoding").bytes_to_string(b"text");
        let encoding = OwnedEncoding::from_named(b"UnknownEncoding", None).unwrap();
        let owned = owned_text(&encoding, b"text");
        assert!(matches!(borrowed, Err(Error::CharacterEncoding)));
        assert!(matches!(owned, Err(Error::CharacterEncoding)));
    }

    #[test]
    fn owned_unicode_matches_mixed_width_cmap_and_replacement_boundaries() {
        let content = exact_cmap();
        let borrowed = Encoding::UnicodeMapEncoding(ToUnicodeCMap::parse(content.clone()).unwrap());
        let owned = OwnedEncoding::from_to_unicode(content.clone()).unwrap();
        let bytes = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, // A B 😀 D
            0xaa, 0xbb, 0xcc, 0xdd, // unmatched four-byte code
            0xee, 0xff, // unmatched tail
        ];
        let expected = borrowed.bytes_to_string(&bytes).unwrap();
        assert_eq!(owned_text(&owned, &bytes).unwrap(), expected);
        assert_eq!(expected, "AB😀D��");

        let identity = OwnedEncoding::from_named(b"Identity-H", Some(content)).unwrap();
        assert_eq!(owned_text(&identity, &bytes).unwrap(), expected);
        assert!(matches!(
            OwnedEncoding::from_named(b"Identity-V", None),
            Err(Error::DictKey(key)) if key == "ToUnicode"
        ));
    }

    #[test]
    fn owned_unicode_rejects_the_same_malformed_cmap_as_borrowed_parser() {
        let malformed = b"1 beginbfchar\n<01> <0041>\n".to_vec();
        assert!(ToUnicodeCMap::parse(malformed.clone()).is_err());
        assert!(OwnedEncoding::from_to_unicode(malformed).is_err());
    }

    #[test]
    fn owned_differences_match_borrowed_valid_and_wrapping_codes() {
        let objects = [
            Object::Integer(65),
            Object::Name(b"Aacute".to_vec()),
            Object::Integer(255),
            Object::Name(b"A".to_vec()),
            Object::Name(b"B".to_vec()),
        ];
        let owned = OwnedEncoding::standard().with_differences(&objects).unwrap();

        let mut map = IndexMap::new();
        let mut inverse = IndexMap::new();
        for (code, glyph) in [
            (65, Glyph::from_name(b"Aacute").unwrap()),
            (255, Glyph::from_name(b"A").unwrap()),
            (0, Glyph::from_name(b"B").unwrap()),
        ] {
            map.insert(code, glyph);
            inverse.insert(glyph, code);
        }
        let borrowed = Encoding::Differences(Differences {
            base: Box::new(Encoding::OneByteEncoding(&STANDARD_ENCODING)),
            map,
            inverse,
        });
        let bytes = [65, 66, 255, 0];
        assert_eq!(
            owned_text(&owned, &bytes).unwrap(),
            borrowed.bytes_to_string(&bytes).unwrap()
        );
        assert_eq!(owned_text(&owned, &bytes).unwrap(), "ÁBAB");
    }

    #[test]
    fn owned_differences_preserve_strict_error_variants() {
        assert!(matches!(
            OwnedEncoding::standard().with_differences(&[Object::Integer(-1)]),
            Err(Error::InvalidEncodingDifferenceCode { code: -1 })
        ));
        assert!(matches!(
            OwnedEncoding::standard().with_differences(&[Object::Integer(256)]),
            Err(Error::InvalidEncodingDifferenceCode { code: 256 })
        ));
        assert!(matches!(
            OwnedEncoding::standard().with_differences(&[Object::Name(b"not-a-glyph".to_vec())]),
            Err(Error::InvalidEncodingDifferenceGlyph { name }) if name == "not-a-glyph"
        ));
        assert!(matches!(
            OwnedEncoding::standard().with_differences(&[Object::Boolean(true)]),
            Err(Error::ObjectType {
                expected: "Integer or Name",
                found: "Boolean"
            })
        ));
    }

    #[test]
    fn owned_caller_missing_differences_matches_borrowed_standard_fallback() {
        let font = dictionary! {
            "Type" => "Font",
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "BaseEncoding" => "MacRomanEncoding",
            },
        };
        let document = Document::new();
        let borrowed = font.get_font_encoding(&document).unwrap();
        let bytes = [0x80, b'A'];
        assert_eq!(
            borrowed.bytes_to_string(&bytes).unwrap(),
            owned_text(&OwnedEncoding::standard(), &bytes).unwrap()
        );
    }

    #[test]
    fn unicode_with_2byte_code_does_not_convert_single_bytes() {
        let mut cmap = ToUnicodeCMap::new();

        cmap.put(0x0000, 0x0002, 2, cmap::BfRangeTarget::UTF16CodePoint { offset: 0 });
        cmap.put(0x0024, 0x0025, 2, cmap::BfRangeTarget::UTF16CodePoint { offset: 0 });

        let bytes: [u8; 2] = [0x00, 0x24];

        let result = Encoding::UnicodeMapEncoding(cmap).bytes_to_string(&bytes);

        assert_eq!(result.unwrap(), "\u{0024}");
    }

    #[test]
    fn winansi_bytes_to_string() {
        // 0xe9 = é in WinAnsi, 0xfc = ü, 0xdf = ß
        let bytes = [0x41, 0xe9, 0x42, 0xfc, 0xdf]; // AéBüß
        let result = Encoding::SimpleEncoding(b"WinAnsiEncoding")
            .bytes_to_string(&bytes)
            .expect("WinAnsi decode should succeed");
        assert_eq!(result, "AéBüß");
    }

    #[test]
    fn winansi_string_to_bytes() {
        let text = "Sébastien 0,019€ ü ÄÖÜ ß";
        let bytes = Encoding::SimpleEncoding(b"WinAnsiEncoding").string_to_bytes(text);
        // Round-trip: decode the bytes back via the same encoding
        let decoded = Encoding::OneByteEncoding(&WIN_ANSI_ENCODING)
            .bytes_to_string(&bytes)
            .expect("WinAnsi decode should succeed");
        assert_eq!(decoded, text);
    }
}
