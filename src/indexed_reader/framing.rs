//! Bounded framing of indirect objects and stream payload validation.

use super::*;

pub(super) fn read_window(
    source: &dyn RandomAccessSource, source_len: u64, offset: u64, limit: u64,
) -> IndexedReaderResult<Vec<u8>> {
    let remaining = source_len.checked_sub(offset).ok_or(SourceError::OutOfBounds {
        offset,
        length: 0,
        source_len,
    })?;
    let length = remaining.min(limit);
    Ok(source.read_range(offset, length, limit)?)
}

pub(super) fn parse_indirect_header(input: &[u8]) -> Option<(crate::ObjectId, usize)> {
    let original_len = input.len();
    let mut cursor = TokenCursor::new(input);
    let number = u32::try_from(cursor.unsigned()?).ok()?;
    let generation = u16::try_from(cursor.unsigned()?).ok()?;
    cursor.expect(b"obj")?;
    cursor.skip_space();
    Some(((number, generation), original_len - cursor.remaining().len()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameStatus {
    NeedMore,
    Ready,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameLex {
    Normal,
    Comment {
        resume: FrameResume,
    },
    Name {
        hash_digits: u8,
        dictionary_key: bool,
    },
    Keyword {
        kind: FrameKeyword,
        matched: usize,
    },
    Number {
        start: usize,
        dot: bool,
        digits: bool,
        unsigned: bool,
    },
    ReferenceGap {
        scalar_completion: usize,
    },
    ReferenceSecond {
        scalar_completion: usize,
        value: u32,
    },
    ReferenceTail {
        scalar_completion: usize,
        second_token_end: usize,
        second_completion: usize,
    },
    ReferenceComment {
        scalar_completion: usize,
        second_token_end: Option<usize>,
        second_completion: Option<usize>,
    },
    ReferenceBoundary,
    Literal {
        depth: usize,
        escaped: bool,
    },
    Hex,
    LessThan,
    GreaterThan,
    AfterValue,
    AfterDictionary,
    AfterComment,
    StreamToken {
        matched: usize,
    },
    StreamEol,
    StreamEolCr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameResume {
    Normal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameKeyword {
    True,
    False,
    Null,
}

impl FrameKeyword {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::True => b"true",
            Self::False => b"false",
            Self::Null => b"null",
        }
    }
}

/// Incrementally frames one direct object and a possible dictionary `stream`
/// header. The fixed container stack avoids a second allocation beside the
/// retained source window; semantic construction remains in the normal parser.
pub(super) struct DirectObjectFramer {
    position: usize,
    containers: [u8; crate::reader::MAX_NESTING_DEPTH],
    container_depth: usize,
    pub(super) lex: FrameLex,
    status: FrameStatus,
    pub(super) scanned_work: usize,
}

impl DirectObjectFramer {
    pub(super) fn new() -> Self {
        Self {
            position: 0,
            containers: [0; crate::reader::MAX_NESTING_DEPTH],
            container_depth: 0,
            lex: FrameLex::Normal,
            status: FrameStatus::NeedMore,
            scanned_work: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn for_dictionary(input: &[u8]) -> Option<Self> {
        if !input.starts_with(b"<<") {
            return None;
        }
        let mut framer = Self::new();
        let _ = framer.advance(input);
        Some(framer)
    }

    pub(super) fn advance(&mut self, input: &[u8]) -> FrameStatus {
        while self.status == FrameStatus::NeedMore && self.position < input.len() {
            let position = self.position;
            match self.lex {
                FrameLex::Normal => self.advance_normal(input),
                FrameLex::Comment { resume } => self.advance_comment(input, resume),
                FrameLex::Name {
                    hash_digits,
                    dictionary_key,
                } => self.advance_name(input, hash_digits, dictionary_key),
                FrameLex::Keyword { kind, matched } => self.advance_keyword(input, kind, matched),
                FrameLex::Number {
                    start,
                    dot,
                    digits,
                    unsigned,
                } => self.advance_number(input, start, dot, digits, unsigned),
                FrameLex::ReferenceGap { scalar_completion } => self.advance_reference_gap(input, scalar_completion),
                FrameLex::ReferenceSecond {
                    scalar_completion,
                    value,
                } => self.advance_reference_second(input, scalar_completion, value),
                FrameLex::ReferenceTail {
                    scalar_completion,
                    second_token_end,
                    second_completion,
                } => self.advance_reference_tail(input, scalar_completion, second_token_end, second_completion),
                FrameLex::ReferenceComment {
                    scalar_completion,
                    second_token_end,
                    second_completion,
                } => self.advance_reference_comment(input, scalar_completion, second_token_end, second_completion),
                FrameLex::ReferenceBoundary => self.advance_reference_boundary(input),
                FrameLex::Literal { depth, escaped } => self.advance_literal(input, depth, escaped),
                FrameLex::Hex => self.advance_hex(input),
                FrameLex::LessThan => self.advance_less_than(input),
                FrameLex::GreaterThan => self.advance_greater_than(input),
                FrameLex::AfterValue => self.status = FrameStatus::Ready,
                FrameLex::AfterDictionary => self.advance_after_dictionary(input),
                FrameLex::AfterComment => self.advance_after_comment(input),
                FrameLex::StreamToken { matched } => self.advance_stream_token(input, matched),
                FrameLex::StreamEol => self.advance_stream_eol(input),
                FrameLex::StreamEolCr => self.advance_stream_eol_cr(input),
            }
            self.scanned_work += self.position.saturating_sub(position);
        }
        self.status
    }

    fn advance_normal(&mut self, input: &[u8]) {
        let byte = input[self.position];
        if self.container_depth != 0
            && self.containers[self.container_depth - 1] == b'k'
            && !matches!(byte, b'%' | b'/' | b'>')
            && !is_pdf_whitespace(byte)
        {
            self.status = FrameStatus::Invalid;
            return;
        }
        match byte {
            b'%' => {
                self.position += 1;
                self.lex = FrameLex::Comment {
                    resume: FrameResume::Normal,
                };
            }
            b'(' => {
                self.position += 1;
                self.lex = FrameLex::Literal {
                    depth: 1,
                    escaped: false,
                };
            }
            b'/' => {
                self.position += 1;
                self.lex = FrameLex::Name {
                    hash_digits: 0,
                    dictionary_key: self.container_depth != 0 && self.containers[self.container_depth - 1] == b'k',
                };
            }
            b'<' => {
                self.position += 1;
                self.lex = FrameLex::LessThan;
            }
            b'>' => {
                self.position += 1;
                self.lex = FrameLex::GreaterThan;
            }
            b'[' => {
                self.position += 1;
                self.push_container(b'[');
            }
            b']' => {
                self.position += 1;
                self.close_container(b'[', false);
            }
            b't' => self.start_keyword(FrameKeyword::True),
            b'f' => self.start_keyword(FrameKeyword::False),
            b'n' => self.start_keyword(FrameKeyword::Null),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.start_number(byte),
            byte if is_pdf_whitespace(byte) => self.position += 1,
            _ => self.status = FrameStatus::Invalid,
        }
    }

    fn advance_comment(&mut self, input: &[u8], resume: FrameResume) {
        let byte = input[self.position];
        self.position += 1;
        if matches!(byte, b'\r' | b'\n') {
            self.lex = match resume {
                FrameResume::Normal => FrameLex::Normal,
            };
        }
    }

    fn advance_name(&mut self, input: &[u8], hash_digits: u8, dictionary_key: bool) {
        let byte = input[self.position];
        if hash_digits != 0 {
            if !byte.is_ascii_hexdigit() {
                // `name` accepts the valid prefix before a malformed escape.
                // Revisit `#` as the next token when this is nested.
                self.position -= 1;
                self.finish_value(false);
                return;
            }
            self.position += 1;
            self.lex = FrameLex::Name {
                hash_digits: if hash_digits == 1 { 2 } else { 0 },
                dictionary_key,
            };
        } else if byte == b'#' {
            self.position += 1;
            self.lex = FrameLex::Name {
                hash_digits: 1,
                dictionary_key,
            };
        } else if is_pdf_whitespace(byte) || is_pdf_delimiter(byte) {
            if dictionary_key {
                self.containers[self.container_depth - 1] = b'v';
                self.lex = FrameLex::Normal;
            } else {
                self.finish_value(false);
            }
        } else {
            self.position += 1;
        }
    }

    fn start_keyword(&mut self, kind: FrameKeyword) {
        self.position += 1;
        self.lex = FrameLex::Keyword { kind, matched: 1 };
    }

    fn advance_keyword(&mut self, input: &[u8], kind: FrameKeyword, matched: usize) {
        let token = kind.bytes();
        let byte = input[self.position];
        if matched < token.len() {
            if byte != token[matched] {
                self.status = FrameStatus::Invalid;
            } else {
                self.position += 1;
                self.lex = FrameLex::Keyword {
                    kind,
                    matched: matched + 1,
                };
            }
        } else {
            self.finish_value(false);
        }
    }

    fn start_number(&mut self, byte: u8) {
        let start = self.position;
        self.position += 1;
        self.lex = FrameLex::Number {
            start,
            dot: byte == b'.',
            digits: byte.is_ascii_digit(),
            unsigned: byte.is_ascii_digit(),
        };
    }

    fn advance_number(&mut self, input: &[u8], start: usize, dot: bool, digits: bool, unsigned: bool) {
        let byte = input[self.position];
        if byte.is_ascii_digit() {
            self.position += 1;
            self.lex = FrameLex::Number {
                start,
                dot,
                digits: true,
                unsigned,
            };
        } else if byte == b'.' && !dot {
            self.position += 1;
            self.lex = FrameLex::Number {
                start,
                dot: true,
                digits,
                unsigned: false,
            };
        } else if is_pdf_whitespace(byte) || byte == b'%' {
            if !digits || !self.number_is_valid(input, start, dot) {
                self.status = FrameStatus::Invalid;
            } else if unsigned
                && !dot
                && input[start..self.position]
                    .iter()
                    .try_fold(0_u32, |value, digit| {
                        value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
                    })
                    .is_some()
            {
                self.lex = FrameLex::ReferenceGap {
                    scalar_completion: self.position,
                };
            } else {
                self.finish_value(false);
            }
        } else if is_pdf_delimiter(byte) {
            if !digits || !self.number_is_valid(input, start, dot) {
                self.status = FrameStatus::Invalid;
            } else {
                self.finish_value(false);
            }
        } else {
            if digits && self.number_is_valid(input, start, dot) {
                self.finish_value(false);
            } else {
                self.status = FrameStatus::Invalid;
            }
        }
    }

    fn number_is_valid(&self, input: &[u8], start: usize, dot: bool) -> bool {
        let Ok(text) = std::str::from_utf8(&input[start..self.position]) else {
            return false;
        };
        if dot {
            text.parse::<f32>().is_ok()
        } else {
            text.parse::<i64>().is_ok()
        }
    }

    fn advance_reference_gap(&mut self, input: &[u8], scalar_completion: usize) {
        match input[self.position] {
            byte if is_pdf_whitespace(byte) => {
                self.position += 1;
                self.lex = FrameLex::ReferenceGap {
                    scalar_completion: self.position,
                };
            }
            b'%' => {
                self.position += 1;
                self.lex = FrameLex::ReferenceComment {
                    scalar_completion,
                    second_token_end: None,
                    second_completion: None,
                };
            }
            byte if byte.is_ascii_digit() => {
                self.position += 1;
                self.lex = FrameLex::ReferenceSecond {
                    scalar_completion,
                    value: u32::from(byte - b'0'),
                };
            }
            _ => self.fallback_integer(scalar_completion),
        }
    }

    fn advance_reference_second(&mut self, input: &[u8], scalar_completion: usize, value: u32) {
        let byte = input[self.position];
        if byte.is_ascii_digit() {
            let next = value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(byte - b'0')));
            if let Some(next) = next.filter(|value| u16::try_from(*value).is_ok()) {
                self.position += 1;
                self.lex = FrameLex::ReferenceSecond {
                    scalar_completion,
                    value: next,
                };
            } else {
                // A generation cannot exceed u16. Stop the speculative
                // reference probe immediately and continue the already-scanned
                // integer without replaying its potentially long zero prefix.
                self.continue_second_integer(scalar_completion);
            }
        } else {
            self.lex = FrameLex::ReferenceTail {
                scalar_completion,
                second_token_end: self.position,
                second_completion: self.position,
            };
        }
    }

    fn advance_reference_tail(
        &mut self, input: &[u8], scalar_completion: usize, second_token_end: usize, second_completion: usize,
    ) {
        match input[self.position] {
            byte if is_pdf_whitespace(byte) => {
                self.position += 1;
                self.lex = FrameLex::ReferenceTail {
                    scalar_completion,
                    second_token_end,
                    second_completion: self.position,
                };
            }
            b'%' => {
                self.position += 1;
                self.lex = FrameLex::ReferenceComment {
                    scalar_completion,
                    second_token_end: Some(second_token_end),
                    second_completion: Some(second_completion),
                };
            }
            b'R' => {
                self.position += 1;
                self.lex = FrameLex::ReferenceBoundary;
            }
            b'.' if self.position == second_token_end => self.continue_adjacent_real(scalar_completion),
            _ => self.fallback_two_integers(scalar_completion, second_completion),
        }
    }

    fn advance_reference_comment(
        &mut self, input: &[u8], scalar_completion: usize, second_token_end: Option<usize>,
        second_completion: Option<usize>,
    ) {
        let byte = input[self.position];
        self.position += 1;
        if matches!(byte, b'\r' | b'\n') {
            self.lex = if second_completion.is_some() {
                FrameLex::ReferenceTail {
                    scalar_completion,
                    second_token_end: second_token_end.unwrap(),
                    second_completion: self.position,
                }
            } else {
                FrameLex::ReferenceGap {
                    scalar_completion: self.position,
                }
            };
        }
    }

    fn advance_reference_boundary(&mut self, _input: &[u8]) {
        // The parser accepts `R` without requiring a token boundary. A nested
        // caller will validate the following byte as a new token.
        self.finish_value(false);
    }

    fn fallback_integer(&mut self, fallback: usize) {
        self.position = fallback;
        self.lex = FrameLex::Normal;
        self.finish_value(false);
    }

    fn continue_adjacent_real(&mut self, second_token_start: usize) {
        if self.container_depth == 0 {
            self.fallback_integer(second_token_start);
            return;
        }
        self.finish_value(false);
        if self.status != FrameStatus::NeedMore {
            return;
        }
        self.position += 1;
        self.lex = FrameLex::Number {
            start: second_token_start,
            dot: true,
            digits: true,
            unsigned: false,
        };
    }

    fn continue_second_integer(&mut self, second_token_start: usize) {
        if self.container_depth == 0 {
            self.fallback_integer(second_token_start);
            return;
        }
        self.finish_value(false);
        if self.status != FrameStatus::NeedMore {
            return;
        }
        self.lex = FrameLex::Number {
            start: second_token_start,
            dot: false,
            digits: true,
            unsigned: true,
        };
    }

    fn fallback_two_integers(&mut self, second_token_start: usize, second_completion: usize) {
        let probe_position = self.position;
        let top_level = self.container_depth == 0;
        self.position = second_token_start;
        self.finish_value(false);
        if top_level || self.status != FrameStatus::NeedMore {
            return;
        }
        // The failed `n n R` probe only proves the first integer is scalar.
        // Keep the already-scanned second integer as a possible object number
        // so it can begin a following reference such as `255 34473 0 R`.
        // Resuming at the probe boundary avoids replaying the token, comments,
        // or whitespace.
        self.position = probe_position;
        self.lex = FrameLex::ReferenceGap {
            scalar_completion: second_completion,
        };
    }

    fn advance_literal(&mut self, input: &[u8], mut depth: usize, escaped: bool) {
        let byte = input[self.position];
        self.position += 1;
        if escaped {
            self.lex = FrameLex::Literal { depth, escaped: false };
            return;
        }
        match byte {
            b'\\' => {
                self.lex = FrameLex::Literal { depth, escaped: true };
            }
            b'(' => {
                if depth > crate::reader::MAX_BRACKET {
                    self.status = FrameStatus::Invalid;
                    return;
                }
                depth += 1;
                self.lex = FrameLex::Literal { depth, escaped: false };
            }
            b')' if depth == 1 => self.finish_value(false),
            b')' => {
                depth -= 1;
                self.lex = FrameLex::Literal { depth, escaped: false };
            }
            _ => {}
        }
    }

    fn advance_hex(&mut self, input: &[u8]) {
        let byte = input[self.position];
        if byte == b'>' {
            self.position += 1;
            self.finish_value(false);
        } else if byte.is_ascii_hexdigit() || is_pdf_whitespace(byte) {
            self.position += 1;
        } else {
            self.status = FrameStatus::Invalid;
        }
    }

    fn advance_less_than(&mut self, input: &[u8]) {
        if input[self.position] == b'<' {
            self.position += 1;
            self.push_container(b'<');
        } else {
            self.lex = FrameLex::Hex;
        }
    }

    fn advance_greater_than(&mut self, input: &[u8]) {
        if input[self.position] != b'>' {
            self.status = FrameStatus::Invalid;
            return;
        }
        self.position += 1;
        self.close_container(b'<', true);
    }

    fn push_container(&mut self, kind: u8) {
        if self.container_depth >= self.containers.len() {
            self.status = FrameStatus::Invalid;
        } else {
            self.containers[self.container_depth] = if kind == b'<' { b'k' } else { kind };
            self.container_depth += 1;
            self.lex = FrameLex::Normal;
        }
    }

    fn close_container(&mut self, kind: u8, dictionary: bool) {
        let expected = if kind == b'<' { b'k' } else { kind };
        if self.container_depth == 0 || self.containers[self.container_depth - 1] != expected {
            self.status = FrameStatus::Invalid;
            return;
        }
        self.container_depth -= 1;
        self.finish_value(dictionary);
    }

    fn finish_value(&mut self, dictionary: bool) {
        if self.container_depth == 0 {
            if dictionary {
                self.lex = FrameLex::AfterDictionary;
            } else {
                // The direct-object parser requires at least one following
                // byte before it can distinguish a complete body from a
                // window ending exactly at the object's final delimiter.
                self.lex = FrameLex::AfterValue;
            }
        } else {
            let parent = &mut self.containers[self.container_depth - 1];
            if *parent == b'v' {
                *parent = b'k';
                self.lex = FrameLex::Normal;
            } else if *parent == b'[' {
                self.lex = FrameLex::Normal;
            } else {
                self.status = FrameStatus::Invalid;
            }
        }
    }

    fn advance_after_dictionary(&mut self, input: &[u8]) {
        match input[self.position] {
            b'%' => {
                self.position += 1;
                self.lex = FrameLex::AfterComment;
            }
            byte if is_pdf_whitespace(byte) => self.position += 1,
            b's' => {
                self.position += 1;
                self.lex = FrameLex::StreamToken { matched: 1 };
            }
            _ => self.status = FrameStatus::Ready,
        }
    }

    fn advance_after_comment(&mut self, input: &[u8]) {
        let byte = input[self.position];
        self.position += 1;
        if matches!(byte, b'\r' | b'\n') {
            self.lex = FrameLex::AfterDictionary;
        }
    }

    fn advance_stream_token(&mut self, input: &[u8], matched: usize) {
        const STREAM: &[u8] = b"stream";
        let byte = input[self.position];
        if matched < STREAM.len() {
            if byte != STREAM[matched] {
                self.status = FrameStatus::Ready;
            } else {
                self.position += 1;
                self.lex = FrameLex::StreamToken { matched: matched + 1 };
            }
            return;
        }
        if !is_token_boundary(Some(byte)) {
            self.status = FrameStatus::Ready;
        } else {
            self.lex = FrameLex::StreamEol;
            self.advance_stream_eol(input);
        }
    }

    fn advance_stream_eol(&mut self, input: &[u8]) {
        match input[self.position] {
            b' ' | b'\t' => self.position += 1,
            b'\r' => {
                self.position += 1;
                self.lex = FrameLex::StreamEolCr;
            }
            b'\n' => {
                self.position += 1;
                self.status = FrameStatus::Ready;
            }
            _ => self.status = FrameStatus::Ready,
        }
    }

    fn advance_stream_eol_cr(&mut self, input: &[u8]) {
        if input[self.position] == b'\n' {
            self.position += 1;
        }
        self.status = FrameStatus::Ready;
    }
}

pub(super) fn parse_object_body(input: &[u8], id: crate::ObjectId, offset: u64) -> IndexedReaderResult<ParsedObject> {
    #[cfg(test)]
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let Some((consumed, object)) = crate::parser::direct_object_with_consumed(input) else {
        return if direct_object_may_be_truncated(input) {
            Err(IndexedReaderError::IncompleteObject { id, offset })
        } else {
            Err(IndexedReaderError::InvalidIndirectObject { id, offset })
        };
    };
    if !matches!(object, Object::Dictionary(_)) {
        let remaining = input
            .get(consumed..)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        if remaining.is_empty() || integer_reference_may_be_truncated(&object, remaining) {
            return Err(IndexedReaderError::IncompleteObject { id, offset });
        }
        return Ok(ParsedObject {
            object,
            consumed,
            stream_prefix: None,
        });
    }

    let remaining = input
        .get(consumed..)
        .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
    let mut cursor = TokenCursor::new(remaining);
    cursor.skip_space();
    if cursor.remaining().is_empty()
        || (cursor.remaining().len() < b"stream".len() && b"stream".starts_with(cursor.remaining()))
    {
        return Err(IndexedReaderError::IncompleteObject { id, offset });
    }
    if !cursor.consume(b"stream") {
        return Ok(ParsedObject {
            object,
            consumed,
            stream_prefix: None,
        });
    }
    if cursor.consume_stream_eol().is_none() {
        return if cursor.remaining().is_empty() {
            Err(IndexedReaderError::IncompleteObject { id, offset })
        } else {
            Ok(ParsedObject {
                object,
                consumed,
                stream_prefix: None,
            })
        };
    }
    let prefix = remaining.len() - cursor.remaining().len();
    Ok(ParsedObject {
        object,
        consumed,
        stream_prefix: Some(
            u64::try_from(prefix).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?,
        ),
    })
}

fn integer_reference_may_be_truncated(object: &Object, input: &[u8]) -> bool {
    if !matches!(object, Object::Integer(_)) {
        return false;
    }
    let mut cursor = TokenCursor::new(input);
    if cursor.unsigned().is_none() {
        return false;
    }
    cursor.skip_space();
    cursor.remaining().is_empty()
}

fn direct_object_may_be_truncated(input: &[u8]) -> bool {
    let mut cursor = TokenCursor::new(input);
    cursor.skip_space();
    let input = cursor.remaining();
    if input.is_empty() {
        return true;
    }
    if input.starts_with(b"<<") || (input.len() < 2 && b"<<".starts_with(input)) {
        return dictionary_may_be_truncated(input);
    }
    if input.starts_with(b"[") {
        return array_may_be_truncated(input);
    }
    if input.starts_with(b"(") {
        return literal_string_end(input, 0).is_none();
    }
    if input.starts_with(b"<") {
        return !input[1..].contains(&b'>');
    }
    [b"true".as_slice(), b"false", b"null"]
        .into_iter()
        .any(|token| input.len() < token.len() && token.starts_with(input))
}

fn array_may_be_truncated(input: &[u8]) -> bool {
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
            b'<' if input.get(position + 1) == Some(&b'<') => position += 2,
            b'<' => {
                let Some(end) = input[position + 1..].iter().position(|byte| *byte == b'>') else {
                    return true;
                };
                position += end + 2;
            }
            b'[' => {
                depth += 1;
                position += 1;
            }
            b']' => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next_depth;
                position += 1;
                if depth == 0 {
                    return false;
                }
            }
            _ => position += 1,
        }
    }
    depth != 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EndstreamStatus {
    Found,
    Missing,
    LimitExceeded,
}

pub(super) fn checked_stream_end(
    id: crate::ObjectId, encoded_start: u64, encoded_len: u64,
) -> IndexedReaderResult<u64> {
    encoded_start
        .checked_add(encoded_len)
        .ok_or(IndexedReaderError::InvalidIndirectObject {
            id,
            offset: encoded_start,
        })
}

pub(super) fn validate_endstream(
    source: &dyn RandomAccessSource, source_len: u64, offset: u64, limit: u64,
) -> IndexedReaderResult<EndstreamStatus> {
    let remaining = source_len.checked_sub(offset).ok_or(SourceError::OutOfBounds {
        offset,
        length: 0,
        source_len,
    })?;
    let length = remaining.min(limit);
    let tail = source.read_range(offset, length, limit)?;
    let tail = if tail.starts_with(b"\r\n") {
        &tail[2..]
    } else if tail.starts_with(b"\r") || tail.starts_with(b"\n") {
        &tail[1..]
    } else {
        &tail
    };
    if tail.starts_with(b"endstream") {
        Ok(EndstreamStatus::Found)
    } else if remaining > length && (tail.is_empty() || b"endstream".starts_with(tail)) {
        Ok(EndstreamStatus::LimitExceeded)
    } else {
        Ok(EndstreamStatus::Missing)
    }
}
