//! Byte-level PDF token scanning and truncation heuristics.

use super::*;

pub(super) fn dictionary_may_be_truncated(input: &[u8]) -> bool {
    if input.is_empty() || (input.len() < 2 && b"<<".starts_with(input)) {
        return true;
    }
    if !input.starts_with(b"<<") {
        return false;
    }
    if contains_invalid_bare_token(input, b'@') {
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

fn contains_invalid_bare_token(input: &[u8], invalid: u8) -> bool {
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
                    return false;
                };
                position = end;
            }
            b'<' if input.get(position + 1) == Some(&b'<') => position += 2,
            b'<' => {
                let Some(end) = input[position + 1..].iter().position(|byte| *byte == b'>') else {
                    return false;
                };
                position += end + 2;
            }
            b'/' => {
                position += 1;
                while position < input.len()
                    && !is_pdf_whitespace(input[position])
                    && !is_pdf_delimiter(input[position])
                {
                    position += 1;
                }
            }
            byte if byte == invalid => return true,
            _ => position += 1,
        }
    }
    false
}

pub(super) fn literal_string_end(input: &[u8], start: usize) -> Option<usize> {
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

pub(super) fn starts_with_token(input: &[u8], token: &[u8]) -> bool {
    let mut cursor = TokenCursor::new(input);
    cursor.consume(token)
}

pub(super) fn rfind(input: &[u8], pattern: &[u8]) -> Option<usize> {
    input.windows(pattern.len()).rposition(|window| window == pattern)
}

pub(super) struct TokenCursor<'a> {
    remaining: &'a [u8],
}

impl<'a> TokenCursor<'a> {
    pub(super) fn new(input: &'a [u8]) -> Self {
        Self { remaining: input }
    }

    pub(super) fn remaining(&self) -> &'a [u8] {
        self.remaining
    }

    pub(super) fn skip_space(&mut self) {
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

    pub(super) fn consume(&mut self, expected: &[u8]) -> bool {
        self.skip_space();
        if self.remaining.starts_with(expected) && is_token_boundary(self.remaining.get(expected.len()).copied()) {
            self.remaining = &self.remaining[expected.len()..];
            true
        } else {
            false
        }
    }

    pub(super) fn expect(&mut self, expected: &[u8]) -> Option<()> {
        self.consume(expected).then_some(())
    }

    pub(super) fn token(&mut self) -> Option<&'a [u8]> {
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

    pub(super) fn unsigned(&mut self) -> Option<u64> {
        let token = self.token()?;
        if !token.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(token).ok()?.parse().ok()
    }

    pub(super) fn direct_object(&mut self) -> Option<Object> {
        self.skip_space();
        let (consumed, object) = crate::parser::direct_object_with_consumed(self.remaining)?;
        self.remaining = self.remaining.get(consumed..)?;
        Some(object)
    }

    pub(super) fn consume_stream_eol(&mut self) -> Option<()> {
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

    pub(super) fn consume_optional_eol(&mut self) {
        if self.remaining.starts_with(b"\r\n") {
            self.remaining = &self.remaining[2..];
        } else if self.remaining.starts_with(b"\n") || self.remaining.starts_with(b"\r") {
            self.remaining = &self.remaining[1..];
        }
    }

    pub(super) fn consume_exact(&mut self, expected: &[u8]) -> bool {
        if self.remaining.starts_with(expected) {
            self.remaining = &self.remaining[expected.len()..];
            true
        } else {
            false
        }
    }

    pub(super) fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (taken, remaining) = self.remaining.split_at_checked(length)?;
        self.remaining = remaining;
        Some(taken)
    }
}

pub(super) fn is_token_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| is_pdf_whitespace(byte) || is_pdf_delimiter(byte))
}

pub(super) fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

pub(super) fn is_pdf_delimiter(byte: u8) -> bool {
    b"()<>[]{}/%".contains(&byte)
}
