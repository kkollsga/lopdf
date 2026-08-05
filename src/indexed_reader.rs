//! Bounded structural bootstrap for the staged indexed reader.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use thiserror::Error;

use crate::source::{RandomAccessSource, SourceError};
use crate::{Dictionary, Object, Stream};

const HEADER_SCAN_LIMIT: u64 = 1_024;
const HEADER_PARSE_OVERLAP: u64 = 64;
const TAIL_SCAN_LIMIT: u64 = 64 * 1_024;
const XREF_INITIAL_WINDOW: u64 = 4 * 1_024;
const XREF_WINDOW_LIMIT: u64 = 16 * 1_024 * 1_024;
const XREF_DECOMPRESSED_LIMIT: usize = 32 * 1_024 * 1_024;
const MAX_XREF_ENTRIES: u64 = 1_000_000;
const MAX_XREF_REVISIONS: usize = 1_024;
const MAX_XREF_FIELD_WIDTH: u64 = 8;

type IndexResult<T> = std::result::Result<T, IndexError>;

#[derive(Debug, Error)]
pub(crate) enum IndexError {
    #[error("indexed source error")]
    Source(#[from] SourceError),
    #[error("invalid PDF header in the first {limit} bytes")]
    InvalidHeader { limit: u64 },
    #[error("invalid startxref in the last {limit} bytes")]
    InvalidStartXref { limit: u64 },
    #[error("invalid cross-reference structure at offset {offset}")]
    InvalidXref { offset: u64 },
    #[error("incomplete cross-reference structure at offset {offset}")]
    IncompleteXref { offset: u64 },
    #[error("invalid trailer at offset {offset}")]
    InvalidTrailer { offset: u64 },
    #[error("{structure} exceeds its {limit}-byte bounded parser window")]
    StructureLimitExceeded { structure: &'static str, limit: u64 },
    #[error("cross-reference entry count {count} exceeds the {limit}-entry limit")]
    EntryLimitExceeded { count: u64, limit: u64 },
    #[error("cross-reference revision count exceeds the {limit}-revision limit")]
    RevisionLimitExceeded { limit: usize },
    #[error("invalid {key} offset in trailer")]
    InvalidTrailerOffset { key: &'static str },
    #[error("cross-reference stream decompression failed")]
    XrefDecompression(#[source] crate::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ObjectLocation64 {
    // The eager classic-xref parser currently drops free entries. The indexed
    // representation deliberately retains them so a later writer/recovery
    // layer does not have to reconstruct the free list.
    Free { next: u64, generation: u16 },
    Normal { offset: u64, generation: u16 },
    Compressed { container: u32, index: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexXrefType {
    Table,
    Stream,
}

#[derive(Debug)]
pub(crate) struct PdfIndex {
    pub(crate) version: String,
    pub(crate) source_origin: u64,
    pub(crate) xref_start: u64,
    pub(crate) xref_type: IndexXrefType,
    pub(crate) declared_size: u64,
    pub(crate) locations: BTreeMap<u32, ObjectLocation64>,
    pub(crate) trailer: Dictionary,
}

impl PdfIndex {
    pub(crate) fn open(source: Arc<dyn RandomAccessSource>) -> IndexResult<Self> {
        let source_len = source.len()?;
        let (source_origin, version) = read_header(source.as_ref(), source_len)?;
        let xref_start = read_startxref(source.as_ref(), source_len)?;

        let mut locations = BTreeMap::new();
        let mut seen = HashSet::new();
        let mut next = Some(xref_start);
        let mut newest_trailer = None;
        let mut newest_type = None;
        let mut declared_size = 0_u64;
        let mut revisions = 0_usize;

        while let Some(offset) = next {
            if !seen.insert(offset) {
                break;
            }
            revisions = revisions.checked_add(1).ok_or(IndexError::RevisionLimitExceeded {
                limit: MAX_XREF_REVISIONS,
            })?;
            if revisions > MAX_XREF_REVISIONS {
                return Err(IndexError::RevisionLimitExceeded {
                    limit: MAX_XREF_REVISIONS,
                });
            }

            let physical = source_origin
                .checked_add(offset)
                .ok_or(IndexError::InvalidXref { offset })?;
            let section = read_xref_section(source.as_ref(), source_len, physical)?;
            if newest_trailer.is_none() {
                newest_type = Some(section.kind);
                newest_trailer = Some(section.trailer.clone());
                declared_size =
                    trailer_unsigned(&section.trailer, b"Size").ok_or(IndexError::InvalidTrailer { offset })?;
            }
            merge_newest(&mut locations, section.entries);

            // A hybrid-reference table supplements its own revision. Its
            // entries fill holes but never replace entries from that revision.
            if let Some(hybrid) = trailer_offset(&section.trailer, b"XRefStm", "XRefStm")? {
                let hybrid_physical = source_origin
                    .checked_add(hybrid)
                    .ok_or(IndexError::InvalidTrailerOffset { key: "XRefStm" })?;
                let supplement = read_xref_section(source.as_ref(), source_len, hybrid_physical)?;
                if supplement.kind != IndexXrefType::Stream {
                    return Err(IndexError::InvalidXref { offset: hybrid });
                }
                merge_newest(&mut locations, supplement.entries);
            }

            next = trailer_offset(&section.trailer, b"Prev", "Prev")?;
        }

        let trailer = newest_trailer.ok_or(IndexError::InvalidXref { offset: xref_start })?;
        Ok(Self {
            version,
            source_origin,
            xref_start,
            xref_type: newest_type.ok_or(IndexError::InvalidXref { offset: xref_start })?,
            declared_size,
            locations,
            trailer,
        })
    }
}

struct XrefSection64 {
    kind: IndexXrefType,
    entries: BTreeMap<u32, ObjectLocation64>,
    trailer: Dictionary,
}

fn merge_newest(target: &mut BTreeMap<u32, ObjectLocation64>, entries: BTreeMap<u32, ObjectLocation64>) {
    for (id, entry) in entries {
        target.entry(id).or_insert(entry);
    }
}

fn read_header(source: &dyn RandomAccessSource, source_len: u64) -> IndexResult<(u64, String)> {
    let read_limit = HEADER_SCAN_LIMIT
        .checked_add(HEADER_PARSE_OVERLAP)
        .ok_or(IndexError::InvalidHeader {
            limit: HEADER_SCAN_LIMIT,
        })?;
    let length = source_len.min(read_limit);
    let bytes = source.read_range(0, length, read_limit)?;
    let origin = bytes
        .windows(5)
        .position(|window| window == b"%PDF-")
        .filter(|origin| {
            u64::try_from(*origin)
                .ok()
                .is_some_and(|origin| origin < HEADER_SCAN_LIMIT)
        })
        .ok_or(IndexError::InvalidHeader {
            limit: HEADER_SCAN_LIMIT,
        })?;
    let version = crate::parser::header(&bytes[origin..], false).ok_or(IndexError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    let origin = u64::try_from(origin).map_err(|_| IndexError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    Ok((origin, version))
}

fn read_startxref(source: &dyn RandomAccessSource, source_len: u64) -> IndexResult<u64> {
    let length = source_len.min(TAIL_SCAN_LIMIT);
    let offset = source_len
        .checked_sub(length)
        .ok_or(IndexError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let tail = source.read_range(offset, length, TAIL_SCAN_LIMIT)?;
    let eof = rfind(&tail, b"%%EOF").ok_or(IndexError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let marker = rfind(&tail[..eof], b"startxref").ok_or(IndexError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let mut cursor = TokenCursor::new(&tail[marker + b"startxref".len()..]);
    cursor
        .unsigned()
        .ok_or(IndexError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })
}

fn read_xref_section(
    source: &dyn RandomAccessSource, source_len: u64, physical_offset: u64,
) -> IndexResult<XrefSection64> {
    if physical_offset >= source_len {
        return Err(IndexError::InvalidXref {
            offset: physical_offset,
        });
    }
    let remaining = source_len.checked_sub(physical_offset).ok_or(IndexError::InvalidXref {
        offset: physical_offset,
    })?;
    let maximum = remaining.min(XREF_WINDOW_LIMIT);
    let mut length = maximum.min(XREF_INITIAL_WINDOW);
    loop {
        let window = source.read_range(physical_offset, length, length)?;
        let result = if starts_with_token(&window, b"xref") {
            parse_classic_xref(&window, physical_offset)
        } else {
            parse_xref_stream(&window, physical_offset)
        };
        match result {
            Ok(section) => return Ok(section),
            Err(IndexError::IncompleteXref { .. }) if length < maximum => {
                length = length.saturating_mul(2).min(maximum);
            }
            Err(IndexError::IncompleteXref { .. }) if remaining > XREF_WINDOW_LIMIT => {
                return Err(IndexError::StructureLimitExceeded {
                    structure: "cross-reference section",
                    limit: XREF_WINDOW_LIMIT,
                });
            }
            Err(IndexError::IncompleteXref { .. }) => {
                return Err(IndexError::InvalidXref {
                    offset: physical_offset,
                });
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_classic_xref(window: &[u8], offset: u64) -> IndexResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_token(&mut cursor, b"xref", offset)?;
    let mut entries = BTreeMap::new();
    let mut entry_count = 0_u64;

    loop {
        cursor.skip_space();
        if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"trailer".len() && b"trailer".starts_with(cursor.remaining()))
        {
            return Err(IndexError::IncompleteXref { offset });
        }
        if cursor.consume(b"trailer") {
            let trailer = match cursor.direct_object() {
                Some(Object::Dictionary(dictionary)) => dictionary,
                Some(_) => return Err(IndexError::InvalidTrailer { offset }),
                None if dictionary_may_be_truncated(cursor.remaining()) => {
                    return Err(IndexError::IncompleteXref { offset });
                }
                None => return Err(IndexError::InvalidTrailer { offset }),
            };
            return Ok(XrefSection64 {
                kind: IndexXrefType::Table,
                entries,
                trailer,
            });
        }

        let start = required_xref_unsigned(&mut cursor, offset)?;
        let count = required_xref_unsigned(&mut cursor, offset)?;
        entry_count = entry_count.checked_add(count).ok_or(IndexError::EntryLimitExceeded {
            count: u64::MAX,
            limit: MAX_XREF_ENTRIES,
        })?;
        check_entry_limit(entry_count)?;

        for index in 0..count {
            let object_number = start.checked_add(index).ok_or(IndexError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexError::InvalidXref { offset })?;
            let field = required_xref_unsigned(&mut cursor, offset)?;
            let generation = required_xref_unsigned(&mut cursor, offset)?;
            let generation = u16::try_from(generation).map_err(|_| IndexError::InvalidXref { offset })?;
            cursor.skip_space();
            if cursor.remaining().is_empty() {
                return Err(IndexError::IncompleteXref { offset });
            }
            let state = cursor.token().ok_or(IndexError::InvalidXref { offset })?;
            let location = match state {
                b"n" => ObjectLocation64::Normal {
                    offset: field,
                    generation,
                },
                b"f" => ObjectLocation64::Free {
                    next: field,
                    generation,
                },
                _ => return Err(IndexError::InvalidXref { offset }),
            };
            entries.insert(object_number, location);
        }
    }
}

fn parse_xref_stream(window: &[u8], offset: u64) -> IndexResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_token(&mut cursor, b"obj", offset)?;
    let dictionary = match cursor.direct_object() {
        Some(Object::Dictionary(dictionary)) => dictionary,
        Some(_) => return Err(IndexError::InvalidTrailer { offset }),
        None if dictionary_may_be_truncated(cursor.remaining()) => {
            return Err(IndexError::IncompleteXref { offset });
        }
        None => return Err(IndexError::InvalidTrailer { offset }),
    };
    required_xref_token(&mut cursor, b"stream", offset)?;
    if cursor.consume_stream_eol().is_none() {
        return if cursor.remaining().is_empty() {
            Err(IndexError::IncompleteXref { offset })
        } else {
            Err(IndexError::InvalidXref { offset })
        };
    }

    let stream_len = trailer_unsigned(&dictionary, b"Length").ok_or(IndexError::InvalidTrailer { offset })?;
    let stream_len = usize::try_from(stream_len).map_err(|_| IndexError::StructureLimitExceeded {
        structure: "cross-reference stream",
        limit: XREF_WINDOW_LIMIT,
    })?;
    if stream_len > cursor.remaining().len() {
        return Err(IndexError::IncompleteXref { offset });
    }
    let content = cursor
        .take(stream_len)
        .ok_or(IndexError::IncompleteXref { offset })?
        .to_vec();
    cursor.consume_optional_eol();
    if !cursor.consume_exact(b"endstream") {
        return if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"endstream".len() && b"endstream".starts_with(cursor.remaining()))
        {
            Err(IndexError::IncompleteXref { offset })
        } else {
            Err(IndexError::InvalidXref { offset })
        };
    }
    let mut stream = Stream::new(dictionary.clone(), content);
    if stream.is_compressed() {
        stream
            .decompress_with_limit(XREF_DECOMPRESSED_LIMIT)
            .map_err(IndexError::XrefDecompression)?;
    }
    decode_xref_stream64(stream, offset)
}

fn decode_xref_stream64(stream: Stream, offset: u64) -> IndexResult<XrefSection64> {
    let mut trailer = stream.dict;
    let size = trailer_unsigned(&trailer, b"Size").ok_or(IndexError::InvalidTrailer { offset })?;
    let widths = integer_array(&trailer, b"W").ok_or(IndexError::InvalidXref { offset })?;
    if widths.len() < 3 || widths[..3].iter().any(|width| *width > MAX_XREF_FIELD_WIDTH) {
        return Err(IndexError::InvalidXref { offset });
    }
    let indices = integer_array(&trailer, b"Index").unwrap_or_else(|| vec![0, size]);
    if !indices.chunks_exact(2).remainder().is_empty() {
        return Err(IndexError::InvalidXref { offset });
    }

    let mut total = 0_u64;
    for pair in indices.chunks_exact(2) {
        total = total.checked_add(pair[1]).ok_or(IndexError::EntryLimitExceeded {
            count: u64::MAX,
            limit: MAX_XREF_ENTRIES,
        })?;
    }
    check_entry_limit(total)?;

    let entry_width = widths[..3]
        .iter()
        .try_fold(0_u64, |sum, width| sum.checked_add(*width))
        .ok_or(IndexError::InvalidXref { offset })?;
    let required = total
        .checked_mul(entry_width)
        .ok_or(IndexError::InvalidXref { offset })?;
    let content_len = u64::try_from(stream.content.len()).map_err(|_| IndexError::InvalidXref { offset })?;
    if required > content_len {
        return Err(IndexError::InvalidXref { offset });
    }

    let mut content = stream.content.as_slice();
    let mut entries = BTreeMap::new();
    for pair in indices.chunks_exact(2) {
        let start = pair[0];
        let count = pair[1];
        for index in 0..count {
            let kind = if widths[0] == 0 {
                1
            } else {
                read_be(&mut content, widths[0], offset)?
            };
            let field2 = read_be(&mut content, widths[1], offset)?;
            let field3 = read_be(&mut content, widths[2], offset)?;
            let object_number = start.checked_add(index).ok_or(IndexError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexError::InvalidXref { offset })?;
            let location = match kind {
                0 => ObjectLocation64::Free {
                    next: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexError::InvalidXref { offset })?,
                },
                1 => ObjectLocation64::Normal {
                    offset: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexError::InvalidXref { offset })?,
                },
                2 => ObjectLocation64::Compressed {
                    container: u32::try_from(field2).map_err(|_| IndexError::InvalidXref { offset })?,
                    index: u32::try_from(field3).map_err(|_| IndexError::InvalidXref { offset })?,
                },
                _ => continue,
            };
            entries.insert(object_number, location);
        }
    }

    trailer.remove(b"Length");
    trailer.remove(b"W");
    trailer.remove(b"Index");
    Ok(XrefSection64 {
        kind: IndexXrefType::Stream,
        entries,
        trailer,
    })
}

fn read_be(input: &mut &[u8], width: u64, offset: u64) -> IndexResult<u64> {
    let width = usize::try_from(width).map_err(|_| IndexError::InvalidXref { offset })?;
    let bytes = input.get(..width).ok_or(IndexError::InvalidXref { offset })?;
    *input = input.get(width..).ok_or(IndexError::InvalidXref { offset })?;
    Ok(bytes.iter().fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn check_entry_limit(count: u64) -> IndexResult<()> {
    if count > MAX_XREF_ENTRIES {
        return Err(IndexError::EntryLimitExceeded {
            count,
            limit: MAX_XREF_ENTRIES,
        });
    }
    Ok(())
}

fn integer_array(dictionary: &Dictionary, key: &[u8]) -> Option<Vec<u64>> {
    dictionary
        .get(key)
        .ok()?
        .as_array()
        .ok()?
        .iter()
        .map(|object| object.as_i64().ok().and_then(|value| u64::try_from(value).ok()))
        .collect()
}

fn trailer_unsigned(dictionary: &Dictionary, key: &[u8]) -> Option<u64> {
    dictionary
        .get(key)
        .ok()?
        .as_i64()
        .ok()
        .and_then(|value| u64::try_from(value).ok())
}

fn trailer_offset(dictionary: &Dictionary, key: &[u8], name: &'static str) -> IndexResult<Option<u64>> {
    match dictionary.get(key) {
        Ok(object) => object
            .as_i64()
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .map(Some)
            .ok_or(IndexError::InvalidTrailerOffset { key: name }),
        Err(_) => Ok(None),
    }
}

fn required_xref_token(cursor: &mut TokenCursor<'_>, expected: &[u8], offset: u64) -> IndexResult<()> {
    cursor.skip_space();
    if cursor.remaining().is_empty()
        || (cursor.remaining().len() < expected.len() && expected.starts_with(cursor.remaining()))
    {
        return Err(IndexError::IncompleteXref { offset });
    }
    cursor.expect(expected).ok_or(IndexError::InvalidXref { offset })
}

fn required_xref_unsigned(cursor: &mut TokenCursor<'_>, offset: u64) -> IndexResult<u64> {
    cursor.skip_space();
    if cursor.remaining().is_empty() {
        return Err(IndexError::IncompleteXref { offset });
    }
    let token = cursor.token().ok_or(IndexError::InvalidXref { offset })?;
    if !token.iter().all(u8::is_ascii_digit) {
        return Err(IndexError::InvalidXref { offset });
    }
    std::str::from_utf8(token)
        .ok()
        .and_then(|token| token.parse().ok())
        .ok_or(IndexError::InvalidXref { offset })
}

fn dictionary_may_be_truncated(input: &[u8]) -> bool {
    if input.is_empty() || (input.len() < 2 && b"<<".starts_with(input)) {
        return true;
    }
    if !input.starts_with(b"<<") {
        return false;
    }

    let mut depth = 0_usize;
    let mut position = 0_usize;
    while position < input.len() {
        match input[position] {
            b'%' => {
                position += 1;
                while position < input.len() && !matches!(input[position], b'\r' | b'\n') {
                    position += 1;
                }
            }
            b'(' => {
                let Some(end) = literal_string_end(input, position) else {
                    return true;
                };
                position = end;
            }
            b'<' if input.get(position + 1) == Some(&b'<') => {
                depth += 1;
                position += 2;
            }
            b'<' => {
                let Some(end) = input[position + 1..].iter().position(|byte| *byte == b'>') else {
                    return true;
                };
                position += end + 2;
            }
            b'>' if input.get(position + 1) == Some(&b'>') => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next_depth;
                position += 2;
                if depth == 0 {
                    return false;
                }
            }
            _ => position += 1,
        }
    }
    depth != 0
}

fn literal_string_end(input: &[u8], start: usize) -> Option<usize> {
    let mut depth = 1_usize;
    let mut position = start + 1;
    while position < input.len() {
        match input[position] {
            b'\\' => {
                position += 1;
                if input.get(position) == Some(&b'\r') && input.get(position + 1) == Some(&b'\n') {
                    position += 2;
                } else if position < input.len() {
                    position += 1;
                }
            }
            b'(' => {
                depth += 1;
                position += 1;
            }
            b')' => {
                depth -= 1;
                position += 1;
                if depth == 0 {
                    return Some(position);
                }
            }
            _ => position += 1,
        }
    }
    None
}

fn starts_with_token(input: &[u8], token: &[u8]) -> bool {
    let mut cursor = TokenCursor::new(input);
    cursor.consume(token)
}

fn rfind(input: &[u8], pattern: &[u8]) -> Option<usize> {
    input.windows(pattern.len()).rposition(|window| window == pattern)
}

struct TokenCursor<'a> {
    remaining: &'a [u8],
}

impl<'a> TokenCursor<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { remaining: input }
    }

    fn remaining(&self) -> &'a [u8] {
        self.remaining
    }

    fn skip_space(&mut self) {
        loop {
            let start = self
                .remaining
                .iter()
                .position(|byte| !is_pdf_whitespace(*byte))
                .unwrap_or(self.remaining.len());
            self.remaining = &self.remaining[start..];
            if self.remaining.first() != Some(&b'%') {
                return;
            }
            match self.remaining.iter().position(|byte| matches!(byte, b'\r' | b'\n')) {
                Some(end) => self.remaining = &self.remaining[end..],
                None => {
                    self.remaining = &[];
                    return;
                }
            }
        }
    }

    fn consume(&mut self, expected: &[u8]) -> bool {
        self.skip_space();
        if self.remaining.starts_with(expected) && is_token_boundary(self.remaining.get(expected.len()).copied()) {
            self.remaining = &self.remaining[expected.len()..];
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &[u8]) -> Option<()> {
        self.consume(expected).then_some(())
    }

    fn token(&mut self) -> Option<&'a [u8]> {
        self.skip_space();
        let end = self
            .remaining
            .iter()
            .position(|byte| is_pdf_whitespace(*byte) || is_pdf_delimiter(*byte))
            .unwrap_or(self.remaining.len());
        if end == 0 {
            return None;
        }
        let (token, remaining) = self.remaining.split_at(end);
        self.remaining = remaining;
        Some(token)
    }

    fn unsigned(&mut self) -> Option<u64> {
        let token = self.token()?;
        if !token.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(token).ok()?.parse().ok()
    }

    fn direct_object(&mut self) -> Option<Object> {
        self.skip_space();
        let (consumed, object) = crate::parser::direct_object_with_consumed(self.remaining)?;
        self.remaining = self.remaining.get(consumed..)?;
        Some(object)
    }

    fn consume_stream_eol(&mut self) -> Option<()> {
        let horizontal = self
            .remaining
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .unwrap_or(self.remaining.len());
        self.remaining = &self.remaining[horizontal..];
        if self.remaining.starts_with(b"\r\n") {
            self.remaining = &self.remaining[2..];
            Some(())
        } else if self.remaining.starts_with(b"\n") || self.remaining.starts_with(b"\r") {
            self.remaining = &self.remaining[1..];
            Some(())
        } else {
            None
        }
    }

    fn consume_optional_eol(&mut self) {
        if self.remaining.starts_with(b"\r\n") {
            self.remaining = &self.remaining[2..];
        } else if self.remaining.starts_with(b"\n") || self.remaining.starts_with(b"\r") {
            self.remaining = &self.remaining[1..];
        }
    }

    fn consume_exact(&mut self, expected: &[u8]) -> bool {
        if self.remaining.starts_with(expected) {
            self.remaining = &self.remaining[expected.len()..];
            true
        } else {
            false
        }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (taken, remaining) = self.remaining.split_at_checked(length)?;
        self.remaining = remaining;
        Some(taken)
    }
}

fn is_token_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| is_pdf_whitespace(byte) || is_pdf_delimiter(byte))
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    b"()<>[]{}/%".contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;
    use crate::source::BytesSource;
    use crate::xref::XrefEntry;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    use std::sync::Mutex;

    type ClassicEntry = (u64, u16, bool);
    type ClassicSection = (u32, Vec<ClassicEntry>);

    fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> u64 {
        let offset = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
        offset
    }

    fn basic_body() -> (Vec<u8>, Vec<u64>) {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![0];
        offsets.push(push_object(&mut pdf, 1, b"<< /Type /Catalog /Pages 2 0 R >>"));
        offsets.push(push_object(&mut pdf, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"));
        offsets.push(push_object(
            &mut pdf,
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        ));
        offsets.push(push_object(&mut pdf, 4, b"<< /Title (base) >>"));
        (pdf, offsets)
    }

    fn append_classic(pdf: &mut Vec<u8>, sections: &[ClassicSection], trailer: &str) -> u64 {
        let xref = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(b"xref\n");
        for (start, entries) in sections {
            pdf.extend_from_slice(format!("{start} {}\n", entries.len()).as_bytes());
            for (offset, generation, in_use) in entries {
                let state = if *in_use { 'n' } else { 'f' };
                pdf.extend_from_slice(format!("{offset:010} {generation:05} {state} \n").as_bytes());
            }
        }
        pdf.extend_from_slice(format!("trailer\n{trailer}\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        xref
    }

    fn classic_pdf() -> Vec<u8> {
        let (mut pdf, offsets) = basic_body();
        let entries = offsets
            .into_iter()
            .enumerate()
            .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
            .collect();
        append_classic(&mut pdf, &[(0, entries)], "<< /Size 5 /Root 1 0 R /Info 4 0 R >>");
        pdf
    }

    fn encode_field(value: u64, width: usize, output: &mut Vec<u8>) {
        for shift in (0..width).rev() {
            output.push((value >> (shift * 8)) as u8);
        }
    }

    fn xref_stream_pdf(compressed: bool) -> Vec<u8> {
        let (mut pdf, offsets) = basic_body();
        let xref = u64::try_from(pdf.len()).unwrap();
        let mut decoded = Vec::new();
        encode_field(0, 1, &mut decoded);
        encode_field(0, 8, &mut decoded);
        encode_field(65535, 2, &mut decoded);
        for offset in offsets.iter().skip(1).copied().chain(std::iter::once(xref)) {
            encode_field(1, 1, &mut decoded);
            encode_field(offset, 8, &mut decoded);
            encode_field(0, 2, &mut decoded);
        }
        let (content, filter) = if compressed {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(&decoded).unwrap();
            (encoder.finish().unwrap(), " /Filter /FlateDecode")
        } else {
            (decoded, "")
        };
        pdf.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /XRef /Size 6 /Root 1 0 R /Info 4 0 R /W [1 8 2] /Length {}{} >>\nstream\n",
                content.len(),
                filter
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&content);
        pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        pdf
    }

    fn open_bytes(pdf: &[u8]) -> PdfIndex {
        PdfIndex::open(Arc::new(BytesSource::from(pdf.to_vec()))).unwrap()
    }

    fn assert_eager_normal_fingerprint(pdf: &[u8], index: &PdfIndex) {
        let eager = Document::load_mem(pdf).unwrap();
        assert_eq!(index.version, eager.version);
        assert_eq!(index.xref_start, u64::try_from(eager.xref_start).unwrap());
        assert_eq!(index.declared_size, u64::from(eager.reference_table.size));
        for (&id, eager_entry) in &eager.reference_table.entries {
            // Normal entries are the shared behavioral surface. The one
            // deliberate divergence is documented on ObjectLocation64::Free.
            if let XrefEntry::Normal { offset, generation } = eager_entry {
                assert_eq!(
                    index.locations.get(&id),
                    Some(&ObjectLocation64::Normal {
                        offset: u64::from(*offset),
                        generation: *generation
                    })
                );
            }
        }
        assert_eq!(
            index.trailer.get(b"Root").unwrap().as_reference().unwrap(),
            eager.trailer.get(b"Root").unwrap().as_reference().unwrap()
        );
    }

    #[test]
    fn classic_bootstrap_matches_eager_fingerprint() {
        let pdf = classic_pdf();
        let index = open_bytes(&pdf);

        assert_eq!(index.xref_type, IndexXrefType::Table);
        assert_eq!(index.source_origin, 0);
        assert_eager_normal_fingerprint(&pdf, &index);
    }

    #[test]
    fn plain_and_compressed_xref_streams_match_eager() {
        for compressed in [false, true] {
            let pdf = xref_stream_pdf(compressed);
            let index = open_bytes(&pdf);
            assert_eq!(index.xref_type, IndexXrefType::Stream);
            assert_eager_normal_fingerprint(&pdf, &index);
        }
    }

    #[test]
    fn incremental_hybrid_merge_keeps_newest_and_supplement() {
        let (mut pdf, offsets) = basic_body();
        let base_entries = offsets
            .into_iter()
            .enumerate()
            .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
            .collect();
        let base_xref = append_classic(&mut pdf, &[(0, base_entries)], "<< /Size 5 /Root 1 0 R /Info 4 0 R >>");

        let new_info = push_object(&mut pdf, 4, b"<< /Title (newest) >>");
        let marker = push_object(&mut pdf, 6, b"<< /Marker (hybrid) >>");
        let supplement = u64::try_from(pdf.len()).unwrap();
        let mut encoded = Vec::new();
        encode_field(1, 1, &mut encoded);
        encode_field(marker, 8, &mut encoded);
        encode_field(0, 2, &mut encoded);
        pdf.extend_from_slice(
            format!(
                "7 0 obj\n<< /Type /XRef /Size 8 /Index [6 1] /W [1 8 2] /Length {} >>\nstream\n",
                encoded.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&encoded);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        append_classic(
            &mut pdf,
            &[(4, vec![(new_info, 0, true)]), (7, vec![(supplement, 0, true)])],
            &format!("<< /Size 8 /Root 1 0 R /Info 4 0 R /Prev {base_xref} /XRefStm {supplement} >>"),
        );
        let index = open_bytes(&pdf);

        assert_eq!(
            index.locations.get(&4),
            Some(&ObjectLocation64::Normal {
                offset: new_info,
                generation: 0
            })
        );
        assert_eq!(
            index.locations.get(&6),
            Some(&ObjectLocation64::Normal {
                offset: marker,
                generation: 0
            })
        );
        assert_eager_normal_fingerprint(&pdf, &index);
    }

    #[test]
    fn leading_junk_is_rebased_like_the_eager_reader() {
        let pdf = classic_pdf();
        let prefix = b"ignored transport prefix\n";
        let mut prefixed = prefix.to_vec();
        prefixed.extend_from_slice(&pdf);
        let index = open_bytes(&prefixed);

        assert_eq!(index.source_origin, u64::try_from(prefix.len()).unwrap());
        assert_eager_normal_fingerprint(&prefixed, &index);
    }

    #[test]
    fn header_starting_at_last_scannable_byte_uses_bounded_overlap() {
        let pdf = classic_pdf();
        let mut boundary = vec![b'x'; usize::try_from(HEADER_SCAN_LIMIT - 1).unwrap()];
        boundary.extend_from_slice(&pdf);
        let index = open_bytes(&boundary);
        assert_eq!(index.source_origin, HEADER_SCAN_LIMIT - 1);

        let mut outside = vec![b'x'; usize::try_from(HEADER_SCAN_LIMIT).unwrap()];
        outside.extend_from_slice(&pdf);
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(outside))),
            Err(IndexError::InvalidHeader { .. })
        ));
    }

    #[test]
    fn malformed_startxref_and_prev_fail_without_fallback() {
        let mut missing = classic_pdf();
        let marker = rfind(&missing, b"startxref").unwrap();
        missing[marker..marker + b"startxref".len()].fill(b'x');
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(missing))),
            Err(IndexError::InvalidStartXref { .. })
        ));

        let mut out_of_bounds = classic_pdf();
        let marker = rfind(&out_of_bounds, b"startxref\n").unwrap() + b"startxref\n".len();
        let end = out_of_bounds[marker..].iter().position(|byte| *byte == b'\n').unwrap() + marker;
        out_of_bounds.splice(marker..end, b"999999999".iter().copied());
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(out_of_bounds))),
            Err(IndexError::InvalidXref { .. })
        ));

        let (mut bad_prev, offsets) = basic_body();
        let entries = offsets
            .into_iter()
            .enumerate()
            .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
            .collect();
        append_classic(&mut bad_prev, &[(0, entries)], "<< /Size 5 /Root 1 0 R /Prev -1 >>");
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(bad_prev))),
            Err(IndexError::InvalidTrailerOffset { key: "Prev" })
        ));
    }

    #[test]
    fn repeated_prev_is_cycle_checked_and_terminates() {
        let (mut pdf, offsets) = basic_body();
        let xref = u64::try_from(pdf.len()).unwrap();
        let entries = offsets
            .into_iter()
            .enumerate()
            .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
            .collect();
        append_classic(
            &mut pdf,
            &[(0, entries)],
            &format!("<< /Size 5 /Root 1 0 R /Prev {xref} >>"),
        );
        let index = open_bytes(&pdf);
        assert_eq!(index.xref_start, xref);
    }

    fn padded_classic_xref(padding_over_limit: u64) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let xref = u64::try_from(pdf.len()).unwrap();
        let prefix = b"xref\n";
        let suffix = b"trailer\n<< /Size 1 /Root 1 0 R >>";
        pdf.extend_from_slice(prefix);
        let used = u64::try_from(prefix.len() + suffix.len()).unwrap();
        let padding = XREF_WINDOW_LIMIT - used + padding_over_limit;
        pdf.resize(pdf.len() + usize::try_from(padding).unwrap(), 0);
        pdf.extend_from_slice(suffix);
        pdf.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        pdf
    }

    #[test]
    fn raw_xref_window_accepts_boundary_and_rejects_one_byte_over() {
        assert!(PdfIndex::open(Arc::new(BytesSource::from(padded_classic_xref(0)))).is_ok());
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(padded_classic_xref(1)))),
            Err(IndexError::StructureLimitExceeded {
                structure: "cross-reference section",
                limit: XREF_WINDOW_LIMIT
            })
        ));
    }

    fn compressed_limit_xref(decoded_len: usize) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&vec![0_u8; decoded_len]).unwrap();
        let content = encoder.finish().unwrap();
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let xref = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(
            format!(
                "1 0 obj\n<< /Type /XRef /Size 1 /Index [0 1] /W [1 0 0] /Filter /FlateDecode /Length {} >>\nstream\n",
                content.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&content);
        pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        pdf
    }

    #[test]
    fn decoded_xref_limit_accepts_boundary_and_rejects_one_byte_over() {
        assert!(
            PdfIndex::open(Arc::new(BytesSource::from(compressed_limit_xref(
                XREF_DECOMPRESSED_LIMIT
            ))))
            .is_ok()
        );
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(compressed_limit_xref(
                XREF_DECOMPRESSED_LIMIT + 1
            )))),
            Err(IndexError::XrefDecompression(_))
        ));
    }

    fn empty_width_xref(width: u64) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let xref = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(
            format!(
                "1 0 obj\n<< /Type /XRef /Size 0 /Index [0 0] /W [1 {width} 1] /Length 0 >>\nstream\n\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn xref_field_width_and_entry_count_limits_are_inclusive() {
        assert!(PdfIndex::open(Arc::new(BytesSource::from(empty_width_xref(8)))).is_ok());
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(empty_width_xref(9)))),
            Err(IndexError::InvalidXref { .. })
        ));
        assert!(check_entry_limit(MAX_XREF_ENTRIES).is_ok());
        assert!(matches!(
            check_entry_limit(MAX_XREF_ENTRIES + 1),
            Err(IndexError::EntryLimitExceeded {
                count,
                limit: MAX_XREF_ENTRIES
            }) if count == MAX_XREF_ENTRIES + 1
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn sparse_file_preserves_normal_offset_beyond_u32() {
        use crate::source::FileSource;
        use std::io::{Seek, SeekFrom};

        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"%PDF-1.7\n").unwrap();
        let xref = u64::from(u32::MAX) + 4_096;
        let object_offset = u64::from(u32::MAX) + 17;
        file.seek(SeekFrom::Start(xref)).unwrap();
        file.write_all(
            format!(
                "xref\n1 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        )
        .unwrap();
        file.flush().unwrap();

        let index = PdfIndex::open(Arc::new(FileSource::from_file(file).unwrap())).unwrap();
        assert_eq!(
            index.locations.get(&1),
            Some(&ObjectLocation64::Normal {
                offset: object_offset,
                generation: 0
            })
        );
    }

    struct OverlaySource {
        len: u64,
        regions: Vec<(u64, Vec<u8>)>,
        requests: Mutex<Vec<(u64, usize)>>,
    }

    impl RandomAccessSource for OverlaySource {
        fn len(&self) -> Result<u64, SourceError> {
            Ok(self.len)
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
            if offset > self.len {
                return Err(SourceError::OutOfBounds {
                    offset,
                    length: 0,
                    source_len: self.len,
                });
            }
            self.requests.lock().unwrap().push((offset, output.len()));
            let available = usize::try_from(self.len - offset).unwrap_or(usize::MAX);
            let read = output.len().min(available);
            output[..read].fill(0);
            let read_end = offset + u64::try_from(read).unwrap();
            for (region_offset, bytes) in &self.regions {
                let region_end = *region_offset + u64::try_from(bytes.len()).unwrap();
                let overlap_start = offset.max(*region_offset);
                let overlap_end = read_end.min(region_end);
                if overlap_start < overlap_end {
                    let destination = usize::try_from(overlap_start - offset).unwrap();
                    let source = usize::try_from(overlap_start - region_offset).unwrap();
                    let length = usize::try_from(overlap_end - overlap_start).unwrap();
                    output[destination..destination + length].copy_from_slice(&bytes[source..source + length]);
                }
            }
            Ok(read)
        }
    }

    #[test]
    fn traced_hundred_megabyte_source_never_requests_the_whole_source() {
        let len = 100_u64 * 1_024 * 1_024;
        let xref = len - 256;
        let tail =
            format!("xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![(0, b"%PDF-1.7\n".to_vec()), (xref, tail)],
            requests: Mutex::new(Vec::new()),
        });
        let index = PdfIndex::open(source.clone()).unwrap();
        assert_eq!(index.xref_start, xref);

        let requests = source.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|(_, length)| u64::try_from(*length).unwrap() <= TAIL_SCAN_LIMIT)
        );
        let total: usize = requests.iter().map(|(_, length)| *length).sum();
        assert!(u64::try_from(total).unwrap() < len / 100);
    }

    #[test]
    fn tiny_xref_at_one_megabyte_uses_only_the_initial_window() {
        let len = 100_u64 * 1_024 * 1_024;
        let xref = 1_024_u64 * 1_024;
        let xref_bytes = b"xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\n".to_vec();
        let eof_offset = len - 128;
        let tail = format!("startxref\n{xref}\n%%EOF\n").into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![(0, b"%PDF-1.7\n".to_vec()), (xref, xref_bytes), (eof_offset, tail)],
            requests: Mutex::new(Vec::new()),
        });

        PdfIndex::open(source.clone()).unwrap();
        let requests = source.requests.lock().unwrap();
        assert!(requests.contains(&(xref, usize::try_from(XREF_INITIAL_WINDOW).unwrap())));
        assert!(
            !requests
                .iter()
                .any(|(offset, length)| { *offset == xref && u64::try_from(*length).unwrap() > XREF_INITIAL_WINDOW })
        );
    }

    #[test]
    fn incremental_revisions_each_stop_at_the_initial_window() {
        let len = 100_u64 * 1_024 * 1_024;
        let base = 1_024_u64 * 1_024;
        let newest = 2_u64 * 1_024 * 1_024;
        let base_bytes = b"xref\n1 1\n0000000009 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\n".to_vec();
        let newest_bytes =
            format!("xref\n1 1\n0000000042 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {base} >>\n").into_bytes();
        let eof_offset = len - 128;
        let tail = format!("startxref\n{newest}\n%%EOF\n").into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (base, base_bytes),
                (newest, newest_bytes),
                (eof_offset, tail),
            ],
            requests: Mutex::new(Vec::new()),
        });

        let index = PdfIndex::open(source.clone()).unwrap();
        assert_eq!(
            index.locations.get(&1),
            Some(&ObjectLocation64::Normal {
                offset: 42,
                generation: 0
            })
        );
        let requests = source.requests.lock().unwrap();
        for offset in [base, newest] {
            let lengths: Vec<_> = requests
                .iter()
                .filter_map(|(actual, length)| (*actual == offset).then_some(*length))
                .collect();
            assert_eq!(lengths, vec![usize::try_from(XREF_INITIAL_WINDOW).unwrap()]);
        }
    }

    fn revision_source(count: usize) -> Arc<OverlaySource> {
        let first = 1_024_u64 * 1_024;
        let stride = 8_u64 * 1_024;
        let mut regions = vec![(0, b"%PDF-1.7\n".to_vec())];
        for revision in 0..count {
            let revision = u64::try_from(revision).unwrap();
            let offset = first + revision * stride;
            let previous = (revision > 0).then(|| first + (revision - 1) * stride);
            let prev = previous.map(|value| format!(" /Prev {value}")).unwrap_or_default();
            regions.push((
                offset,
                format!("xref\n1 1\n{revision:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R{prev} >>\n").into_bytes(),
            ));
        }
        let newest = first + u64::try_from(count - 1).unwrap() * stride;
        let len = newest + 2 * TAIL_SCAN_LIMIT;
        let eof_offset = len - 128;
        regions.push((eof_offset, format!("startxref\n{newest}\n%%EOF\n").into_bytes()));
        Arc::new(OverlaySource {
            len,
            regions,
            requests: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn revision_limit_accepts_1024_and_rejects_1025() {
        let boundary = revision_source(MAX_XREF_REVISIONS);
        assert!(PdfIndex::open(boundary).is_ok());

        let over = revision_source(MAX_XREF_REVISIONS + 1);
        assert!(matches!(
            PdfIndex::open(over),
            Err(IndexError::RevisionLimitExceeded {
                limit: MAX_XREF_REVISIONS
            })
        ));
    }

    #[test]
    fn two_node_prev_cycle_is_detected_without_extra_reads() {
        let len = 4_u64 * 1_024 * 1_024;
        let first = 1_024_u64 * 1_024;
        let second = 2_u64 * 1_024 * 1_024;
        let first_bytes =
            format!("xref\n1 1\n0000000011 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {second} >>\n").into_bytes();
        let second_bytes =
            format!("xref\n1 1\n0000000022 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev {first} >>\n").into_bytes();
        let eof_offset = len - 128;
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (first, first_bytes),
                (second, second_bytes),
                (eof_offset, format!("startxref\n{second}\n%%EOF\n").into_bytes()),
            ],
            requests: Mutex::new(Vec::new()),
        });

        let index = PdfIndex::open(source.clone()).unwrap();
        assert_eq!(
            index.locations.get(&1),
            Some(&ObjectLocation64::Normal {
                offset: 22,
                generation: 0
            })
        );
        let requests = source.requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|(offset, _)| *offset == first).count(), 1);
        assert_eq!(requests.iter().filter(|(offset, _)| *offset == second).count(), 1);
    }

    #[test]
    fn semantic_xref_stream_error_stops_after_initial_window() {
        let len = 100_u64 * 1_024 * 1_024;
        let xref = 1_024_u64 * 1_024;
        let xref_bytes =
            b"1 0 obj\n<< /Type /XRef /Size 0 /Index [0 0] /W [1 9 1] /Length 0 >>\nstream\n\nendstream\nendobj\n"
                .to_vec();
        let eof_offset = len - 128;
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (xref, xref_bytes),
                (eof_offset, format!("startxref\n{xref}\n%%EOF\n").into_bytes()),
            ],
            requests: Mutex::new(Vec::new()),
        });

        assert!(matches!(
            PdfIndex::open(source.clone()),
            Err(IndexError::InvalidXref { .. })
        ));
        let requests = source.requests.lock().unwrap();
        let lengths: Vec<_> = requests
            .iter()
            .filter_map(|(offset, length)| (*offset == xref).then_some(*length))
            .collect();
        assert_eq!(lengths, vec![usize::try_from(XREF_INITIAL_WINDOW).unwrap()]);
    }

    #[test]
    fn incomplete_dictionary_probe_tracks_nesting_and_pdf_strings() {
        assert!(dictionary_may_be_truncated(b"<< /Nested << /Value 1 >>"));
        assert!(!dictionary_may_be_truncated(b"<< /Nested << /Value 1 >> >>"));
        assert!(dictionary_may_be_truncated(b"<< /Text (a >> nested \\) value)"));
        assert!(!dictionary_may_be_truncated(
            b"<< /Text (a >> nested \\) value) /Hex <3e3e> >>"
        ));
    }

    fn corrupt_marker(mut pdf: Vec<u8>, marker: &[u8]) -> Vec<u8> {
        let position = rfind(&pdf, marker).unwrap();
        pdf[position..position + marker.len()].fill(b'x');
        pdf
    }

    #[test]
    fn xref_stream_framing_matches_eager_reader() {
        let valid = xref_stream_pdf(true);
        assert!(Document::load_mem(&valid).is_ok());
        assert!(PdfIndex::open(Arc::new(BytesSource::from(valid.clone()))).is_ok());

        let missing_endstream = corrupt_marker(valid.clone(), b"endstream");
        assert!(Document::load_mem(&missing_endstream).is_err());
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(missing_endstream))),
            Err(IndexError::InvalidXref { .. })
        ));

        let mut missing_endobj = valid.clone();
        let marker = rfind(&missing_endobj, b"endobj").unwrap();
        missing_endobj.drain(marker..marker + b"endobj".len());
        assert!(Document::load_mem(&missing_endobj).is_ok());
        assert!(PdfIndex::open(Arc::new(BytesSource::from(missing_endobj))).is_ok());

        let mut spaced_header = valid.clone();
        let marker = spaced_header
            .windows(b"stream\n".len())
            .position(|window| window == b"stream\n")
            .unwrap();
        spaced_header.splice(
            marker + b"stream".len()..marker + b"stream".len(),
            b" \t".iter().copied(),
        );
        assert!(Document::load_mem(&spaced_header).is_ok());
        assert!(PdfIndex::open(Arc::new(BytesSource::from(spaced_header))).is_ok());

        for replacement in [b"".as_slice(), b"\r\n"] {
            let mut accepted = valid.clone();
            let marker = rfind(&accepted, b"endstream").unwrap();
            assert_eq!(accepted[marker - 1], b'\n');
            accepted.splice(marker - 1..marker, replacement.iter().copied());
            assert!(Document::load_mem(&accepted).is_ok());
            assert!(PdfIndex::open(Arc::new(BytesSource::from(accepted))).is_ok());
        }

        for replacement in [b" \n".as_slice(), b"\n% gap\n"] {
            let mut rejected = valid.clone();
            let marker = rfind(&rejected, b"endstream").unwrap();
            rejected.splice(marker - 1..marker, replacement.iter().copied());
            assert!(Document::load_mem(&rejected).is_err());
            assert!(matches!(
                PdfIndex::open(Arc::new(BytesSource::from(rejected))),
                Err(IndexError::InvalidXref { .. })
            ));
        }
    }
}
