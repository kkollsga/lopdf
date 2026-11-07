//! Finding the signatures a rewrite would break. Nothing here checks one, or says that one is valid.

use super::*;

/// Signatures one file reports. A file with more is still known to be signed, which is what decides anything.
const MAX_REPORTED_SIGNATURES: usize = 4_096;
/// Form fields looked at for signature names.
const MAX_FIELDS_LOOKED_AT: usize = 1_000_000;
/// How far past `/ByteRange` its four numbers can be, in the search over the raw file.
const BYTE_RANGE_WINDOW: usize = 128;

/// What kind of signature it says it is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexedSignatureKind {
    /// `/Type /Sig`, or a dictionary with a `/ByteRange` and no type.
    Signature,
    /// `/Type /DocTimeStamp`.
    DocumentTimestamp,
}

/// A signature found in a file. It says what's there, and never that it's valid or who really signed it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedSignature {
    /// The signature dictionary, if it's an object of its own.
    pub object: Option<crate::ObjectId>,
    /// The field's `/T`, if a form field holds it: the raw bytes of a PDF string, as untrusted as the rest.
    pub field_name: Option<Vec<u8>>,
    /// Its `/SubFilter`.
    pub sub_filter: Option<Vec<u8>>,
    /// Its `/Name`, which is only what the file says.
    pub stated_name: Option<Vec<u8>>,
    /// Its `/ByteRange`, if it's four numbers that make sense as one.
    pub byte_range: Option<[u64; 4]>,
    /// What it says it is.
    pub kind: IndexedSignatureKind,
    /// False if this was found in the file's bytes and the newest revision doesn't hold it: it signs an earlier one.
    pub in_newest_revision: bool,
}

/// Whether this dictionary is a signature, by what it says it is. A `/ByteRange` with no `/Type` counts: `/Type` is optional.
pub(super) fn is_signature_dictionary(dict: &Dictionary) -> Option<IndexedSignatureKind> {
    if dict.has_type(b"DocTimeStamp") {
        return Some(IndexedSignatureKind::DocumentTimestamp);
    }
    if dict.has_type(b"Sig") || (dict.has(b"ByteRange") && dict.has(b"Contents")) {
        return Some(IndexedSignatureKind::Signature);
    }
    None
}

fn signature_from(
    dict: &Dictionary, object: Option<crate::ObjectId>, field_name: Option<Vec<u8>>, kind: IndexedSignatureKind,
    in_newest_revision: bool,
) -> IndexedSignature {
    let name = |key: &[u8]| match dict.get(key) {
        Ok(Object::Name(value) | Object::String(value, _)) => Some(value.clone()),
        _ => None,
    };
    let byte_range = dict.get(b"ByteRange").and_then(Object::as_array).ok().and_then(|items| {
        let numbers: Vec<u64> = items
            .iter()
            .filter_map(|item| item.as_i64().ok().and_then(|value| u64::try_from(value).ok()))
            .collect();
        <[u64; 4]>::try_from(numbers).ok()
    });
    IndexedSignature {
        object,
        field_name,
        sub_filter: name(b"SubFilter"),
        stated_name: name(b"Name"),
        byte_range,
        kind,
        in_newest_revision,
    }
}

impl IndexedReader {
    /// Every signature in this file, in any revision: those a form field or `/Perms` holds, and every `/ByteRange` in the file's bytes,
    /// which finds those that the newest revision has forgotten about too. A signature can't be in an object stream, since what it signs has to
    /// leave a hole for it in the file's own bytes. Reads the whole file, in small pieces, and keeps nothing.
    ///
    /// This says what's there. It doesn't check anything, and it never says that a signature is valid. Text that happens to
    /// spell out a whole `/ByteRange` in an unfiltered stream would count, on the side of refusing.
    ///
    /// # Errors
    ///
    /// A source read failure.
    pub fn signatures(&self) -> IndexedReaderResult<Vec<IndexedSignature>> {
        let view = self.uncached_view();
        let mut found = Vec::new();
        view.collect_field_signatures(&mut found)?;
        view.collect_byte_range_signatures(&mut found)?;
        Ok(found)
    }

    fn collect_field_signatures(&self, found: &mut Vec<IndexedSignature>) -> IndexedReaderResult<()> {
        let Some(catalog) = self
            .index
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .ok()
            .and_then(|root| self.look_at(root))
            .and_then(|root| root.as_dict().ok().cloned())
        else {
            return Ok(());
        };
        if let Some(permissions) = self.dict_at(catalog.get(b"Perms").ok()) {
            for (_, value) in permissions.iter() {
                self.note_field_signature(value, None, found);
            }
        }
        let Some(form) = self.dict_at(catalog.get(b"AcroForm").ok()) else {
            return Ok(());
        };
        let mut stack: Vec<u32> = Vec::new();
        if let Some(Object::Array(fields)) = self.value_at(form.get(b"Fields").ok()) {
            stack.extend(fields.iter().rev().filter_map(|field| field.as_reference().ok().map(|id| id.0)));
        }
        let mut seen = HashSet::new();
        while let Some(number) = stack.pop() {
            if seen.len() >= MAX_FIELDS_LOOKED_AT || !seen.insert(number) {
                continue;
            }
            let Some(field) = self.dict_of_number(number) else {
                continue;
            };
            if let Ok(value) = field.get(b"V") {
                let name = match field.get(b"T") {
                    Ok(Object::String(name, _)) => Some(name.clone()),
                    _ => None,
                };
                self.note_field_signature(value, name, found);
            }
            if let Some(Object::Array(kids)) = self.value_at(field.get(b"Kids").ok()) {
                stack.extend(kids.iter().rev().filter_map(|kid| kid.as_reference().ok().map(|id| id.0)));
            }
        }
        Ok(())
    }

    fn note_field_signature(&self, value: &Object, field_name: Option<Vec<u8>>, found: &mut Vec<IndexedSignature>) {
        let (object, dict) = match value {
            Object::Reference(id) => (Some(*id), self.dict_of_number(id.0)),
            Object::Dictionary(dict) => (None, Some(dict.clone())),
            _ => (None, None),
        };
        if let Some(dict) = dict
            && let Some(kind) = is_signature_dictionary(&dict)
            && found.len() < MAX_REPORTED_SIGNATURES
            && !object.is_some_and(|id| found.iter().any(|known| known.object == Some(id)))
        {
            found.push(signature_from(&dict, object, field_name, kind, true));
        }
    }

    fn look_at(&self, id: crate::ObjectId) -> Option<Object> {
        match self.index.locations.get(&id.0)? {
            ObjectLocation64::Normal { generation, .. } => {
                let (_, _, parsed) = self.resolve_normal_framed((id.0, *generation)).ok()?;
                Some(parsed.object)
            }
            ObjectLocation64::Compressed { .. } => self.resolve_object((id.0, 0)).ok(),
            ObjectLocation64::Free { .. } => None,
        }
    }

    fn value_at(&self, value: Option<&Object>) -> Option<Object> {
        match value? {
            Object::Reference(id) => self.look_at(*id),
            other => Some(other.clone()),
        }
    }

    fn dict_at(&self, value: Option<&Object>) -> Option<Dictionary> {
        match self.value_at(value)? {
            Object::Dictionary(dict) => Some(dict),
            _ => None,
        }
    }

    fn dict_of_number(&self, number: u32) -> Option<Dictionary> {
        match self.look_at((number, 0))? {
            Object::Dictionary(dict) => Some(dict),
            _ => None,
        }
    }

    /// Every `/ByteRange [a b c d]` in the file's bytes, read in pieces.
    fn collect_byte_range_signatures(&self, found: &mut Vec<IndexedSignature>) -> IndexedReaderResult<()> {
        const KEY: &[u8] = b"/ByteRange";
        let source = self.source.as_ref();
        let length = self.index.source_len;
        let overlap = KEY.len() + BYTE_RANGE_WINDOW;
        let mut buffer = vec![0_u8; COPY_CHUNK_BYTES + overlap];
        let mut position = 0_u64;
        let mut hits = Vec::new();
        while position < length {
            let step = usize::try_from(length - position).map_or(buffer.len(), |left| left.min(buffer.len()));
            source.read_exact_at(position, &mut buffer[..step])?;
            let window = &buffer[..step];
            let last_chunk = position + step as u64 >= length;
            // A match in the overlap is found whole by the next piece.
            let limit = if last_chunk { step } else { COPY_CHUNK_BYTES };
            let mut at = 0;
            while let Some(found_at) = find_bytes(&window[at..limit.min(step)], KEY) {
                let hit = at + found_at;
                if let Some(range) = parse_byte_range(&window[hit + KEY.len()..]) {
                    hits.push((position + hit as u64, range));
                }
                at = hit + KEY.len();
                if at >= limit.min(step) {
                    break;
                }
            }
            if last_chunk {
                break;
            }
            position += COPY_CHUNK_BYTES as u64;
        }
        if hits.is_empty() {
            return Ok(());
        }
        // Each hit belongs to the last object that starts before it, if that object says so.
        let mut starts: Vec<(u64, u32, u16)> = self
            .index
            .locations
            .iter()
            .filter_map(|(number, location)| match location {
                ObjectLocation64::Normal { offset, generation } => {
                    Some((self.index.source_origin.saturating_add(*offset), *number, *generation))
                }
                _ => None,
            })
            .collect();
        starts.sort_unstable();
        for (at, range) in hits {
            if found.len() >= MAX_REPORTED_SIGNATURES {
                break;
            }
            if found.iter().any(|known| known.byte_range == Some(range)) {
                continue;
            }
            let holder = starts
                .partition_point(|(offset, _, _)| *offset <= at)
                .checked_sub(1)
                .and_then(|index| starts.get(index));
            let dict = holder.and_then(|(_, number, generation)| {
                let (_, _, parsed) = self.resolve_normal_framed((*number, *generation)).ok()?;
                parsed.object.as_dict().ok().cloned()
            });
            let in_this_object = dict.as_ref().is_some_and(|dict| {
                is_signature_dictionary(dict).is_some()
                    && dict
                        .get(b"ByteRange")
                        .and_then(Object::as_array)
                        .is_ok_and(|items| items.iter().filter_map(|item| item.as_i64().ok()).map(|value| value as u64).eq(range))
            });
            if let (true, Some(dict), Some((_, number, generation))) = (in_this_object, &dict, holder) {
                let kind = is_signature_dictionary(dict).unwrap_or(IndexedSignatureKind::Signature);
                found.push(signature_from(dict, Some((*number, *generation)), None, kind, true));
            } else {
                found.push(IndexedSignature {
                    object: None,
                    field_name: None,
                    sub_filter: None,
                    stated_name: None,
                    byte_range: Some(range),
                    kind: IndexedSignatureKind::Signature,
                    in_newest_revision: false,
                });
            }
        }
        Ok(())
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The four numbers of a `/ByteRange` that starts here. Whitespace between them may be anything PDF allows.
fn parse_byte_range(bytes: &[u8]) -> Option<[u64; 4]> {
    let mut cursor = TokenCursor::new(bytes.get(..BYTE_RANGE_WINDOW.min(bytes.len()))?);
    cursor.skip_space();
    cursor.remaining().starts_with(b"[").then_some(())?;
    cursor.take(1)?;
    let mut numbers = [0_u64; 4];
    for number in &mut numbers {
        *number = cursor.unsigned()?;
    }
    cursor.skip_space();
    cursor.remaining().starts_with(b"]").then_some(numbers)
}

impl Plan<'_> {
    /// A signature dictionary the walk came across that the search of the file didn't find, as in an object stream.
    pub(super) fn note_signature_dictionary(&mut self, number: Option<u32>, dict: &Dictionary) {
        let Some(kind) = is_signature_dictionary(dict) else {
            return;
        };
        if self.signatures.len() >= MAX_REPORTED_SIGNATURES {
            return;
        }
        let object = number.map(|number| (number, self.generation(number)));
        let duplicate = self.signatures.iter().any(|known| {
            object.is_some_and(|object| known.object == Some(object))
                || (number.is_none()
                    && dict
                        .get(b"ByteRange")
                        .and_then(Object::as_array)
                        .is_ok_and(|items| {
                            known.byte_range.is_some_and(|range| {
                                items.iter().filter_map(|item| item.as_i64().ok()).map(|value| value as u64).eq(range)
                            })
                        }))
        });
        if !duplicate {
            self.signatures.push(signature_from(dict, object, None, kind, true));
        }
    }
}
