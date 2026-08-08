//! Allocation preflight for bounded scalar parsing.

use super::*;

pub(super) struct ChargedBytes {
    pub(super) bytes: Box<[u8]>,
    _charge: ScalarCharge,
}

impl ChargedBytes {
    pub(super) fn read(
        source: &dyn RandomAccessSource, offset: u64, length: u64, permit: &ScalarResolutionPermit,
        id: crate::ObjectId, phase: &'static str,
    ) -> IndexedReaderResult<Self> {
        let charge = permit.reserve(id, length, phase)?;
        let bytes = source.read_range(offset, length, length)?.into_boxed_slice();
        Ok(Self { bytes, _charge: charge })
    }

    pub(super) fn grow_exact(
        old: Self, source: &dyn RandomAccessSource, base_offset: u64, target: u64, permit: &ScalarResolutionPermit,
        id: crate::ObjectId, phase: &'static str,
    ) -> IndexedReaderResult<Self> {
        let old_len = u64::try_from(old.bytes.len()).map_err(|_| IndexedReaderError::ScalarResourceLimit {
            id,
            requested: u64::MAX,
            limit: permit.limit_bytes(),
            phase,
        })?;
        let charge = permit.reserve(id, target, phase)?;
        let target_usize = usize::try_from(target).map_err(|_| IndexedReaderError::ScalarResourceLimit {
            id,
            requested: target,
            limit: permit.limit_bytes(),
            phase,
        })?;
        let mut bytes = vec![0; target_usize].into_boxed_slice();
        bytes[..old.bytes.len()].copy_from_slice(&old.bytes);
        let extension_offset = base_offset
            .checked_add(old_len)
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: base_offset,
            })?;
        source.read_exact_at(
            extension_offset,
            &mut bytes[usize::try_from(old_len).unwrap_or(usize::MAX)..],
        )?;
        drop(old);
        Ok(Self { bytes, _charge: charge })
    }
}

/// Allocation-free upper bound for the normal direct-object parser's owned
/// AST. The scanner deliberately mirrors the parser's token and container
/// structure, but never decodes into an owned buffer. This lets the caller
/// reserve the complete AST allowance before entering the allocating parser.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ScalarAstPreflight {
    retained_bytes: usize,
    pub(super) transient_bytes: usize,
    largest_string_bytes: usize,
}

impl ScalarAstPreflight {
    pub(super) fn reserved_bytes(self, decrypts_strings: bool) -> u64 {
        let decryption_overlap = if decrypts_strings {
            // AES decryption can hold the ciphertext plus two plaintext-sized
            // Vec allocations at once (the working buffer and the unpadded
            // result). RC4 needs less. The fixed tail covers object-key/IV
            // workspace, so this is a conservative bound for every supported
            // string crypt filter.
            self.largest_string_bytes.saturating_mul(2).saturating_add(64)
        } else {
            0
        };
        u64::try_from(
            self.retained_bytes
                .saturating_add(self.transient_bytes.max(decryption_overlap)),
        )
        .unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AstPart {
    heap_bytes: usize,
    pub(super) transient_bytes: usize,
    largest_string_bytes: usize,
}

pub(super) fn scalar_ast_preflight(input: &[u8]) -> Option<ScalarAstPreflight> {
    let mut scanner = ScalarAstScanner { input, position: 0 };
    let part = scanner.object(0)?;
    Some(ScalarAstPreflight {
        retained_bytes: std::mem::size_of::<Object>().saturating_add(part.heap_bytes),
        transient_bytes: part.transient_bytes,
        largest_string_bytes: part.largest_string_bytes,
    })
}

struct ScalarAstScanner<'a> {
    input: &'a [u8],
    position: usize,
}

impl ScalarAstScanner<'_> {
    fn object(&mut self, depth: usize) -> Option<AstPart> {
        if depth >= crate::reader::MAX_NESTING_DEPTH {
            return None;
        }
        let result = match self.peek()? {
            b'/' => {
                let (capacity, transient) = self.name_capacity()?;
                AstPart {
                    heap_bytes: capacity,
                    transient_bytes: transient,
                    largest_string_bytes: 0,
                }
            }
            b'(' => self.literal_string()?,
            b'<' if self.input.get(self.position + 1) == Some(&b'<') => self.dictionary(depth + 1)?,
            b'<' => self.hex_string()?,
            b'[' => self.array(depth + 1)?,
            b't' if self.consume(b"true") => AstPart::default(),
            b'f' if self.consume(b"false") => AstPart::default(),
            b'n' if self.consume(b"null") => AstPart::default(),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.number_or_reference()?,
            _ => return None,
        };
        self.skip_space();
        Some(result)
    }

    fn array(&mut self, depth: usize) -> Option<AstPart> {
        self.position += 1;
        self.skip_space();
        let mut result = AstPart::default();
        let mut length = 0_usize;
        // nom's `many0` starts its output Vec at capacity four, including for
        // an empty array.
        let mut capacity = object_vec_min_capacity();
        let mut realloc_overlap = 0_usize;
        loop {
            if self.peek()? == b']' {
                self.position += 1;
                break;
            }
            let child = self.object(depth)?;
            let next_capacity = grown_vec_capacity(capacity, length, 1, object_vec_min_capacity())?;
            if next_capacity != capacity {
                realloc_overlap = realloc_overlap.max(capacity.saturating_mul(std::mem::size_of::<Object>()));
                capacity = next_capacity;
            }
            length = length.checked_add(1)?;
            result.heap_bytes = result.heap_bytes.saturating_add(child.heap_bytes);
            result.transient_bytes = result.transient_bytes.max(child.transient_bytes);
            result.largest_string_bytes = result.largest_string_bytes.max(child.largest_string_bytes);
        }
        result.heap_bytes = result
            .heap_bytes
            .saturating_add(capacity.saturating_mul(std::mem::size_of::<Object>()));
        result.transient_bytes = result.transient_bytes.max(realloc_overlap);
        Some(result)
    }

    fn dictionary(&mut self, depth: usize) -> Option<AstPart> {
        self.position += 2;
        self.skip_space();
        let mut result = AstPart::default();
        loop {
            if self.input.get(self.position..self.position + 2)? == b">>" {
                self.position += 2;
                break;
            }
            let (key_capacity, key_transient) = self.name_capacity()?;
            self.skip_space();
            let value = self.object(depth)?;
            result.heap_bytes = result
                .heap_bytes
                .saturating_add(key_capacity)
                .saturating_add(std::mem::size_of::<(Vec<u8>, Object)>())
                // IndexMap's entry index, hash/control storage, allocator
                // rounding, and table-growth overlap are not visible through
                // Dictionary::iter(), so carry an explicit per-entry bound.
                .saturating_add(128)
                .saturating_add(value.heap_bytes);
            result.transient_bytes = result.transient_bytes.max(key_transient).max(value.transient_bytes);
            result.largest_string_bytes = result.largest_string_bytes.max(value.largest_string_bytes);
        }
        Some(result)
    }

    fn name_capacity(&mut self) -> Option<(usize, usize)> {
        if self.peek()? != b'/' {
            return None;
        }
        self.position += 1;
        // nom's `many0` starts its output Vec at capacity four, including for
        // an empty name.
        let mut vector = SimulatedVec {
            capacity: 4,
            peak: 4,
            ..SimulatedVec::default()
        };
        while let Some(byte) = self.peek() {
            if byte == b'#' {
                let Some((first, second)) = self.input.get(self.position + 1).zip(self.input.get(self.position + 2))
                else {
                    break;
                };
                if !first.is_ascii_hexdigit() || !second.is_ascii_hexdigit() {
                    break;
                }
                vector.push(1)?;
                self.position += 3;
            } else if !is_pdf_whitespace(byte) && !is_pdf_delimiter(byte) {
                vector.push(1)?;
                self.position += 1;
            } else {
                break;
            }
        }
        Some((vector.capacity, vector.peak.saturating_sub(vector.capacity)))
    }

    fn hex_string(&mut self) -> Option<AstPart> {
        self.position += 1;
        let mut vector = SimulatedVec::default();
        let mut high_nibble = false;
        loop {
            let byte = self.peek()?;
            if byte == b'>' {
                self.position += 1;
                break;
            }
            if is_pdf_whitespace(byte) {
                self.position += 1;
            } else if byte.is_ascii_hexdigit() {
                if !high_nibble {
                    vector.push(1)?;
                }
                high_nibble = !high_nibble;
                self.position += 1;
            } else {
                return None;
            }
        }
        Some(AstPart {
            heap_bytes: vector.capacity,
            transient_bytes: vector.peak.saturating_sub(vector.capacity),
            largest_string_bytes: vector.capacity,
        })
    }

    fn literal_string(&mut self) -> Option<AstPart> {
        self.position += 1;
        let mut vectors = [SimulatedVec::default(); crate::reader::MAX_BRACKET + 1];
        let mut depth = 0_usize;
        let mut active_capacity = 0_usize;
        let mut peak_capacity = 0_usize;
        loop {
            let byte = self.peek()?;
            match byte {
                b'(' => {
                    if depth >= crate::reader::MAX_BRACKET {
                        return None;
                    }
                    depth += 1;
                    self.position += 1;
                }
                b')' if depth == 0 => {
                    self.position += 1;
                    break;
                }
                b')' => {
                    self.position += 1;
                    let child = vectors[depth];
                    let mut completed = child;
                    simulate_vec_growth(&mut completed, 1, &mut active_capacity, &mut peak_capacity)?;
                    completed.length += 1;
                    simulate_vec_growth(&mut completed, 1, &mut active_capacity, &mut peak_capacity)?;
                    completed.length += 1;
                    vectors[depth] = completed;

                    let parent = &mut vectors[depth - 1];
                    simulate_vec_growth(parent, completed.length, &mut active_capacity, &mut peak_capacity)?;
                    parent.length = parent.length.checked_add(completed.length)?;
                    active_capacity = active_capacity.saturating_sub(completed.capacity);
                    vectors[depth] = SimulatedVec::default();
                    depth -= 1;
                }
                b'\\' => {
                    self.position += 1;
                    let escaped = self.peek()?;
                    let produced = if escaped.is_ascii_digit() && escaped < b'8' {
                        let mut digits = 0;
                        while digits < 3
                            && self
                                .input
                                .get(self.position + digits)
                                .is_some_and(|byte| byte.is_ascii_digit() && *byte < b'8')
                        {
                            digits += 1;
                        }
                        self.position += digits;
                        1
                    } else if escaped == b'\r' {
                        self.position += 1;
                        if self.peek() == Some(b'\n') {
                            self.position += 1;
                        }
                        0
                    } else if escaped == b'\n' {
                        self.position += 1;
                        0
                    } else {
                        self.position += 1;
                        1
                    };
                    if produced != 0 {
                        let current = &mut vectors[depth];
                        simulate_vec_growth(current, produced, &mut active_capacity, &mut peak_capacity)?;
                        current.length = current.length.checked_add(produced)?;
                    }
                }
                b'\r' | b'\n' => {
                    let produced = if byte == b'\r' && self.input.get(self.position + 1) == Some(&b'\n') {
                        self.position += 2;
                        2
                    } else {
                        self.position += 1;
                        1
                    };
                    let current = &mut vectors[depth];
                    simulate_vec_growth(current, produced, &mut active_capacity, &mut peak_capacity)?;
                    current.length = current.length.checked_add(produced)?;
                }
                _ => {
                    let start = self.position;
                    while self
                        .peek()
                        .is_some_and(|byte| !matches!(byte, b'(' | b')' | b'\\' | b'\r' | b'\n'))
                    {
                        self.position += 1;
                    }
                    let produced = self.position.checked_sub(start)?;
                    let current = &mut vectors[depth];
                    simulate_vec_growth(current, produced, &mut active_capacity, &mut peak_capacity)?;
                    current.length = current.length.checked_add(produced)?;
                }
            }
        }
        let final_capacity = vectors[0].capacity;
        Some(AstPart {
            heap_bytes: final_capacity,
            transient_bytes: peak_capacity.saturating_sub(final_capacity),
            largest_string_bytes: final_capacity,
        })
    }

    fn number_or_reference(&mut self) -> Option<AstPart> {
        let first_start = self.position;
        let mut dot = false;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.position += 1;
        }
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.position += 1;
            } else if byte == b'.' && !dot {
                dot = true;
                self.position += 1;
            } else {
                break;
            }
        }
        if self.position == first_start
            || self.input[first_start..self.position]
                .iter()
                .all(|byte| !byte.is_ascii_digit())
        {
            return None;
        }
        if !dot
            && !matches!(self.input[first_start], b'+' | b'-')
            && self.input[first_start..self.position].iter().all(u8::is_ascii_digit)
        {
            let after_first = self.position;
            self.skip_space();
            let second_start = self.position;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
            if self.position != second_start {
                self.skip_space();
                if self.peek() == Some(b'R') {
                    self.position += 1;
                    return Some(AstPart::default());
                }
            }
            self.position = after_first;
        }
        Some(AstPart::default())
    }

    pub(super) fn skip_space(&mut self) {
        loop {
            while self.peek().is_some_and(is_pdf_whitespace) {
                self.position += 1;
            }
            if self.peek() != Some(b'%') {
                break;
            }
            let comment_start = self.position;
            self.position += 1;
            while self.peek().is_some_and(|byte| !matches!(byte, b'\r' | b'\n')) {
                self.position += 1;
            }
            if self.peek().is_none() {
                self.position = comment_start;
                break;
            }
        }
    }

    pub(super) fn consume(&mut self, expected: &[u8]) -> bool {
        if self
            .input
            .get(self.position..self.position.saturating_add(expected.len()))
            == Some(expected)
        {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SimulatedVec {
    length: usize,
    capacity: usize,
    peak: usize,
}

impl SimulatedVec {
    fn push(&mut self, additional: usize) -> Option<()> {
        let next = grown_vec_capacity(self.capacity, self.length, additional, 8)?;
        if next != self.capacity {
            self.peak = self.peak.max(self.capacity.saturating_add(next));
            self.capacity = next;
        }
        self.length = self.length.checked_add(additional)?;
        self.peak = self.peak.max(self.capacity);
        Some(())
    }
}

fn simulate_vec_growth(
    vector: &mut SimulatedVec, additional: usize, active_capacity: &mut usize, peak_capacity: &mut usize,
) -> Option<()> {
    let next = grown_vec_capacity(vector.capacity, vector.length, additional, 8)?;
    if next != vector.capacity {
        // `active_capacity` includes the old allocation. During realloc the
        // new allocation exists before that old allocation is released.
        *peak_capacity = (*peak_capacity).max(active_capacity.saturating_add(next));
        *active_capacity = active_capacity.saturating_sub(vector.capacity).saturating_add(next);
        vector.capacity = next;
    }
    vector.peak = vector.peak.max(*active_capacity);
    *peak_capacity = (*peak_capacity).max(*active_capacity);
    Some(())
}

fn grown_vec_capacity(capacity: usize, length: usize, additional: usize, minimum: usize) -> Option<usize> {
    let required = length.checked_add(additional)?;
    if required <= capacity {
        return Some(capacity);
    }
    Some(capacity.saturating_mul(2).max(required).max(minimum))
}

const fn object_vec_min_capacity() -> usize {
    if std::mem::size_of::<Object>() == 1 {
        8
    } else if std::mem::size_of::<Object>() <= 1024 {
        4
    } else {
        1
    }
}
