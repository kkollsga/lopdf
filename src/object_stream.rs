use crate::parser;
use crate::{DecompressError, Document, Error, Object, ObjectId, Result, Stream};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::TryFromIntError;
use std::str::FromStr;
use std::sync::Arc;

/// Maximum `/N` and parsed header-pair count accepted by the selected-member
/// object-stream parsers.
///
/// This bounds header-validation work for untrusted object streams. It applies
/// only to [`ObjectStream::parse_selected_member`] and
/// [`ObjectStream::parse_selected_member_with_limit`]; the existing eager
/// constructors retain their behavior.
pub const MAX_SELECTED_OBJECT_STREAM_MEMBERS: usize = 131_072;

use log::warn;
#[cfg(feature = "rayon")]
use rayon::prelude::*;

#[derive(Debug)]
pub struct ObjectStream {
    pub objects: BTreeMap<ObjectId, Object>,
    max_objects: usize,
    compression_level: u32,
}

/// Call-local decoded bytes and a compact index for selected ObjStm members.
/// Invalid unrelated header pairs remain represented instead of rejecting the
/// complete stream, preserving the selected-member parser's permissive policy.
pub(crate) struct SelectedObjectStream {
    decoded: Arc<[u8]>,
    first: usize,
    pairs: Vec<(Option<u32>, Option<u32>)>,
}

#[derive(Debug, Clone)]
pub struct ObjectStreamBuilder {
    max_objects: usize,
    compression_level: u32,
}

#[derive(Debug, Clone)]
pub struct ObjectStreamConfig {
    pub max_objects_per_stream: usize,
    pub compression_level: u32,
}

impl Default for ObjectStreamConfig {
    fn default() -> Self {
        Self {
            max_objects_per_stream: 100,
            compression_level: 6,
        }
    }
}

impl ObjectStream {
    /// Parse an existing object stream.
    ///
    /// This decompresses the stream without any size limit. For untrusted input,
    /// prefer [`ObjectStream::new_with_limit`] to guard against decompression
    /// bombs.
    pub fn new(stream: &mut Stream) -> Result<ObjectStream> {
        Self::new_with_limit(stream, None)
    }

    /// Parse an existing object stream, rejecting it if its decompressed content
    /// would exceed `max_decompressed_size` bytes. `None` means no limit (the
    /// behavior of [`ObjectStream::new`]).
    pub fn new_with_limit(stream: &mut Stream, max_decompressed_size: Option<usize>) -> Result<ObjectStream> {
        match max_decompressed_size {
            // Object streams are decompressed while the document is loaded, so
            // enforcing the limit here bounds the memory a single stream can use.
            Some(max) => stream.decompress_with_limit(max)?,
            None => {
                let _ = stream.decompress();
            }
        }

        if stream.content.is_empty() {
            return Ok(ObjectStream {
                objects: BTreeMap::new(),
                max_objects: 100,
                compression_level: 6,
            });
        }

        let first_offset = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)?
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        let index_block = stream
            .content
            .get(..first_offset)
            .ok_or(Error::InvalidOffset(first_offset))?;

        let numbers_str = std::str::from_utf8(index_block).map_err(|e| Error::InvalidObjectStream(e.to_string()))?;
        let numbers: Vec<_> = numbers_str
            .split_whitespace()
            .map(|number| u32::from_str(number).ok())
            .collect();
        let len = numbers.len() / 2 * 2; // Ensure only pairs.

        let n = stream.dict.get(b"N").and_then(Object::as_i64)?;
        if numbers.len().try_into().ok() != n.checked_mul(2) {
            warn!("object stream: the object stream dictionary specifies a wrong number of objects")
        }

        let chunks_filter_map = |chunk: &[_]| {
            let id = chunk[0]?;
            let offset = first_offset + chunk[1]? as usize;

            if offset >= stream.content.len() {
                warn!("out-of-bounds offset in object stream");
                return None;
            }
            // Skip leading whitespace — some PDFs emit newlines before objects in ObjStm
            let mut start = offset;
            while start < stream.content.len() && stream.content[start].is_ascii_whitespace() {
                start += 1;
            }
            if start >= stream.content.len() {
                warn!("only whitespace after offset in object stream");
                return None;
            }
            let object = parser::direct_object(&stream.content[start..])?;

            Some(((id, 0), object))
        };
        #[cfg(feature = "rayon")]
        let objects = numbers[..len].par_chunks(2).filter_map(chunks_filter_map).collect();
        #[cfg(not(feature = "rayon"))]
        let objects = numbers[..len].chunks(2).filter_map(chunks_filter_map).collect();

        Ok(ObjectStream {
            objects,
            max_objects: 100,
            compression_level: 6,
        })
    }

    /// Parse one declared member of an existing object stream.
    ///
    /// `member_index` is the zero-based index recorded by the compressed xref
    /// entry. The header entry at that index must name `expected_id`, whose
    /// generation must be zero. Only that member is parsed into an [`Object`];
    /// the header is scanned without materializing the other objects.
    /// Repeated object ids are accepted; the index selects the exact declared
    /// occurrence, which must name `expected_id`.
    ///
    /// This decompresses without a size limit. For untrusted input, prefer
    /// [`ObjectStream::parse_selected_member_with_limit`].
    pub fn parse_selected_member(stream: &Stream, expected_id: ObjectId, member_index: u32) -> Result<Object> {
        Self::parse_selected_member_with_limit(stream, expected_id, member_index, None)
    }

    /// Parse one declared member, rejecting the object stream if decompression
    /// exceeds `max_decompressed_size`. `None` means no decompression limit.
    ///
    /// Header count mismatches, odd trailing header tokens, repeated ids and
    /// unrelated malformed pairs retain [`ObjectStream::new_with_limit`]'s
    /// advisory/filtering behavior. The requested complete pair must exist,
    /// name `expected_id` and point to a parseable object. Parsing starts at its
    /// declared offset and uses the eager parser's prefix policy.
    pub fn parse_selected_member_with_limit(
        stream: &Stream, expected_id: ObjectId, member_index: u32, max_decompressed_size: Option<usize>,
    ) -> Result<Object> {
        if expected_id.1 != 0 {
            return Err(Error::InvalidObjectStream(
                "compressed objects must have generation zero".to_string(),
            ));
        }

        // Keep the scalar path allocation-compatible with the original
        // selected-member parser. The compact full header index is reserved for
        // multi-member batch calls.
        let decoded = if stream.is_compressed() {
            match max_decompressed_size {
                Some(max) => Cow::Owned(stream.decompressed_content_with_limit(max)?),
                None => match stream.decompressed_content() {
                    Ok(decoded) => Cow::Owned(decoded),
                    Err(_) => Cow::Borrowed(stream.content.as_slice()),
                },
            }
        } else {
            if let Some(max) = max_decompressed_size
                && stream.content.len() > max
            {
                return Err(DecompressError::MemoryLimitExceeded { limit: max }.into());
            }
            Cow::Borrowed(stream.content.as_slice())
        };

        if decoded.is_empty() {
            return Err(Error::InvalidObjectStream(
                "selected object stream member is not present".to_string(),
            ));
        }

        let first = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)?
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        let index_block = decoded.get(..first).ok_or(Error::InvalidOffset(first))?;
        let index_text = std::str::from_utf8(index_block).map_err(|e| Error::InvalidObjectStream(e.to_string()))?;
        let token_limit = MAX_SELECTED_OBJECT_STREAM_MEMBERS
            .checked_add(1)
            .and_then(|pairs| pairs.checked_mul(2))
            .ok_or_else(|| Error::InvalidObjectStream("selected-parser member limit overflow".to_string()))?;
        let number_count = index_text.split_whitespace().take(token_limit).count();
        if number_count == token_limit {
            return Err(Error::InvalidObjectStream(format!(
                "parsed member count exceeds selected-parser limit {MAX_SELECTED_OBJECT_STREAM_MEMBERS}"
            )));
        }
        let pair_count = number_count / 2;

        let n = stream.dict.get(b"N").and_then(Object::as_i64)?;
        let member_limit = i64::try_from(MAX_SELECTED_OBJECT_STREAM_MEMBERS)
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        if n > member_limit {
            return Err(Error::InvalidObjectStream(format!(
                "declared member count {n} exceeds selected-parser limit {MAX_SELECTED_OBJECT_STREAM_MEMBERS}"
            )));
        }
        if number_count.try_into().ok() != n.checked_mul(2) {
            warn!("object stream: the object stream dictionary specifies a wrong number of objects")
        }

        let member_index: usize = member_index
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        if member_index >= pair_count {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} is outside the {pair_count} complete header pairs"
            )));
        }
        let token_index = member_index
            .checked_mul(2)
            .ok_or_else(|| Error::InvalidObjectStream("object stream member index overflow".to_string()))?;
        let mut numbers = index_text.split_whitespace();
        let declared_id = numbers
            .nth(token_index)
            .and_then(|number| u32::from_str(number).ok())
            .ok_or_else(|| Error::InvalidObjectStream("selected object id is invalid".to_string()))?;
        let relative_offset = numbers
            .next()
            .and_then(|number| u32::from_str(number).ok())
            .ok_or_else(|| Error::InvalidObjectStream("selected object offset is invalid".to_string()))?;
        if declared_id != expected_id.0 {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} declares object {declared_id}, not {}",
                expected_id.0
            )));
        }

        let relative_offset: usize = relative_offset
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        let start = first
            .checked_add(relative_offset)
            .ok_or_else(|| Error::InvalidObjectStream("object stream member offset overflow".to_string()))?;
        if start >= decoded.len() {
            return Err(Error::InvalidOffset(start));
        }
        let start = decoded[start..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .and_then(|leading| start.checked_add(leading))
            .ok_or_else(|| Error::InvalidObjectStream("selected object stream member is empty".to_string()))?;

        parser::direct_object(&decoded[start..]).ok_or_else(|| {
            Error::InvalidObjectStream("selected object stream member is truncated or invalid".to_string())
        })
    }

    /// Decode and index one object stream for resolving multiple selected
    /// members without retaining the decoded container beyond the caller.
    pub(crate) fn selected_members_with_limit(
        stream: &Stream, max_decompressed_size: Option<usize>,
    ) -> Result<SelectedObjectStream> {
        SelectedObjectStream::new_with_limit(stream, max_decompressed_size)
    }

    /// Create a builder for constructing new object streams
    pub fn builder() -> ObjectStreamBuilder {
        ObjectStreamBuilder {
            max_objects: 100,
            compression_level: 6,
        }
    }

    /// Add an object to the stream
    pub fn add_object(&mut self, id: ObjectId, obj: Object) -> Result<()> {
        // Check if object can be added to stream
        if matches!(obj, Object::Stream(_)) {
            return Err(Error::InvalidObjectStream(
                "Stream objects cannot be stored in object streams".into(),
            ));
        }

        // Check capacity
        if self.objects.len() >= self.max_objects {
            return Err(Error::InvalidObjectStream(format!(
                "Object stream has reached maximum capacity of {} objects",
                self.max_objects
            )));
        }

        self.objects.insert(id, obj);
        Ok(())
    }

    /// Get the number of objects in the stream
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Build the stream content in the format required by PDF spec
    pub fn build_stream_content(&self) -> Result<Vec<u8>> {
        if self.objects.is_empty() {
            return Ok(Vec::new());
        }

        // Sort objects by ID for consistent output
        let mut sorted_objects: Vec<_> = self.objects.iter().collect();
        sorted_objects.sort_by_key(|(id, _)| *id);

        // First build the offset table to know its size
        let mut offset_entries = Vec::new();
        let mut current_offset = 0;

        for ((obj_num, _gen), obj) in &sorted_objects {
            // Store the object number and its offset
            offset_entries.push(format!("{obj_num} {current_offset}"));

            // Calculate size of this object's serialization
            let mut obj_bytes = Vec::new();
            crate::writer::Writer::write_object(&mut obj_bytes, obj)?;
            current_offset += obj_bytes.len() + 1; // +1 for space separator
        }

        // Build the complete offset table with proper spacing
        let offset_table = offset_entries.join(" ") + " ";

        // Now build the final content
        let mut content = Vec::new();
        content.extend_from_slice(offset_table.as_bytes());

        // Add serialized objects with space separators
        for ((_, _), obj) in &sorted_objects {
            let mut obj_bytes = Vec::new();
            crate::writer::Writer::write_object(&mut obj_bytes, obj)?;
            content.extend_from_slice(&obj_bytes);
            content.push(b' '); // Space separator between objects
        }

        Ok(content)
    }

    /// Convert to a Stream object ready for insertion into a PDF
    pub fn to_stream_object(&self) -> Result<Stream> {
        let content = self.build_stream_content()?;

        // Calculate where the first object starts
        // We need to find the size of the offset table
        let mut sorted_objects: Vec<_> = self.objects.iter().collect();
        sorted_objects.sort_by_key(|(id, _)| *id);

        // Build the offset entries to calculate exact size
        let mut offset_entries = Vec::new();
        let mut current_offset = 0;

        for ((obj_num, _gen), obj) in &sorted_objects {
            offset_entries.push(format!("{obj_num} {current_offset}"));

            // Calculate size of this object's serialization
            let mut obj_bytes = Vec::new();
            crate::writer::Writer::write_object(&mut obj_bytes, obj)?;
            current_offset += obj_bytes.len() + 1; // +1 for space separator
        }

        // The offset table is joined with spaces and has a trailing space
        let offset_table = offset_entries.join(" ") + " ";
        let first_offset = offset_table.len();

        let dict = dictionary! {
            "Type" => "ObjStm",
            "N" => self.objects.len() as i64,
            "First" => first_offset as i64,
        };

        let mut stream = Stream::new(dict, content);

        // Apply compression - object streams should always be compressed
        if self.compression_level > 0 {
            // Force compression by setting Filter directly
            use flate2::Compression;
            use flate2::write::ZlibEncoder;
            use std::io::prelude::*;

            let compression = match self.compression_level {
                0 => Compression::none(),
                1..=3 => Compression::fast(),
                4..=6 => Compression::default(),
                _ => Compression::best(),
            };

            let mut encoder = ZlibEncoder::new(Vec::new(), compression);
            encoder.write_all(&stream.content)?;
            let compressed = encoder.finish()?;

            stream.dict.set("Filter", "FlateDecode");
            stream.set_content(compressed);
        }

        Ok(stream)
    }

    /// Check if an object can be compressed into an object stream
    pub fn can_be_compressed(id: ObjectId, obj: &Object, doc: &Document) -> bool {
        // Rule 1: Stream objects cannot be compressed
        if matches!(obj, Object::Stream(_)) {
            return false;
        }

        // Rule 2: Objects with non-zero generation cannot be compressed
        if id.1 != 0 {
            return false;
        }

        // Rule 3: Only encryption dictionary cannot be compressed from trailer references
        if let Ok(Object::Reference(encrypt_ref)) = doc.trailer.get(b"Encrypt")
            && id == *encrypt_ref
        {
            return false;
        }

        // Rule 4: Specific object types that cannot be compressed
        if let Object::Dictionary(dict) = obj
            && let Ok(type_obj) = dict.get(b"Type")
            && let Ok(type_name) = type_obj.as_name()
        {
            match type_name {
                // Cross-reference streams and object streams cannot be compressed
                b"XRef" => return false,
                b"ObjStm" => return false,

                // Catalog can only be excluded in linearized PDFs
                b"Catalog" if Self::is_linearized(doc) => {
                    return false;
                }
                b"Catalog" => {}

                // Page, Pages, and all other types CAN be compressed
                _ => {}
            }
        }

        // Default: Allow compression
        true
    }

    /// Check if a PDF document is linearized
    fn is_linearized(doc: &Document) -> bool {
        // In a linearized PDF, the first object after the header should be a
        // linearization dictionary with /Linearized entry
        // For simplicity, we check if any object has a /Linearized entry
        for obj in doc.objects.values() {
            if let Object::Dictionary(dict) = obj
                && dict.has(b"Linearized")
            {
                return true;
            }
        }
        false
    }
}

impl SelectedObjectStream {
    fn new_with_limit(stream: &Stream, max_decompressed_size: Option<usize>) -> Result<Self> {
        // Keep decompression call-local. A batch therefore retains at most the
        // decoded containers it is actively resolving, never a source-wide map.
        let decoded: Arc<[u8]> = if stream.is_compressed() {
            match max_decompressed_size {
                Some(max) => Arc::from(stream.decompressed_content_with_limit(max)?),
                // Preserve the eager unbounded constructor's fallback to the
                // original bytes when a filter cannot be decoded.
                None => match stream.decompressed_content() {
                    Ok(decoded) => Arc::from(decoded),
                    Err(_) => Arc::from(stream.content.as_slice()),
                },
            }
        } else {
            if let Some(max) = max_decompressed_size
                && stream.content.len() > max
            {
                return Err(DecompressError::MemoryLimitExceeded { limit: max }.into());
            }
            Arc::from(stream.content.as_slice())
        };

        if decoded.is_empty() {
            return Err(Error::InvalidObjectStream(
                "selected object stream member is not present".to_string(),
            ));
        }

        let first = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)?
            .try_into()
            .map_err(|error: TryFromIntError| Error::NumericCast(error.to_string()))?;
        let index_block = decoded.get(..first).ok_or(Error::InvalidOffset(first))?;
        let index_text =
            std::str::from_utf8(index_block).map_err(|error| Error::InvalidObjectStream(error.to_string()))?;
        let token_limit = MAX_SELECTED_OBJECT_STREAM_MEMBERS
            .checked_add(1)
            .and_then(|pairs| pairs.checked_mul(2))
            .ok_or_else(|| Error::InvalidObjectStream("selected-parser member limit overflow".to_string()))?;
        let number_count = index_text.split_whitespace().take(token_limit).count();
        if number_count == token_limit {
            return Err(Error::InvalidObjectStream(format!(
                "parsed member count exceeds selected-parser limit {MAX_SELECTED_OBJECT_STREAM_MEMBERS}"
            )));
        }

        let n = stream.dict.get(b"N").and_then(Object::as_i64)?;
        let member_limit = i64::try_from(MAX_SELECTED_OBJECT_STREAM_MEMBERS)
            .map_err(|error: TryFromIntError| Error::NumericCast(error.to_string()))?;
        if n > member_limit {
            return Err(Error::InvalidObjectStream(format!(
                "declared member count {n} exceeds selected-parser limit {MAX_SELECTED_OBJECT_STREAM_MEMBERS}"
            )));
        }
        if number_count.try_into().ok() != n.checked_mul(2) {
            warn!("object stream: the object stream dictionary specifies a wrong number of objects")
        }

        // Parse each header token independently. Unrelated malformed pairs stay
        // advisory; only selecting one turns its invalid token into an error.
        let pair_count = number_count / 2;
        let mut pairs = Vec::with_capacity(pair_count);
        let mut tokens = index_text.split_whitespace();
        for _ in 0..pair_count {
            pairs.push((
                tokens.next().and_then(|token| u32::from_str(token).ok()),
                tokens.next().and_then(|token| u32::from_str(token).ok()),
            ));
        }
        Ok(Self { decoded, first, pairs })
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.decoded.len().saturating_add(
            self.pairs
                .len()
                .saturating_mul(std::mem::size_of::<(Option<u32>, Option<u32>)>()),
        )
    }

    pub(crate) fn parse_member(&self, expected_id: ObjectId, member_index: u32) -> Result<Object> {
        if expected_id.1 != 0 {
            return Err(Error::InvalidObjectStream(
                "compressed objects must have generation zero".to_string(),
            ));
        }

        let member_index: usize = member_index
            .try_into()
            .map_err(|error: TryFromIntError| Error::NumericCast(error.to_string()))?;
        let Some(&(declared_id, relative_offset)) = self.pairs.get(member_index) else {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} is outside the {} complete header pairs",
                self.pairs.len()
            )));
        };
        let declared_id =
            declared_id.ok_or_else(|| Error::InvalidObjectStream("selected object id is invalid".to_string()))?;
        let relative_offset = relative_offset
            .ok_or_else(|| Error::InvalidObjectStream("selected object offset is invalid".to_string()))?;
        if declared_id != expected_id.0 {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} declares object {declared_id}, not {}",
                expected_id.0
            )));
        }

        let relative_offset: usize = relative_offset
            .try_into()
            .map_err(|error: TryFromIntError| Error::NumericCast(error.to_string()))?;
        let start = self
            .first
            .checked_add(relative_offset)
            .ok_or_else(|| Error::InvalidObjectStream("object stream member offset overflow".to_string()))?;
        if start >= self.decoded.len() {
            return Err(Error::InvalidOffset(start));
        }
        let start = self.decoded[start..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .and_then(|leading| start.checked_add(leading))
            .ok_or_else(|| Error::InvalidObjectStream("selected object stream member is empty".to_string()))?;

        // Do not bound parsing at the next declared offset: the existing eager
        // prefix policy accepts the first complete direct object from this tail.
        parser::direct_object(&self.decoded[start..]).ok_or_else(|| {
            Error::InvalidObjectStream("selected object stream member is truncated or invalid".to_string())
        })
    }
}

impl ObjectStreamBuilder {
    /// Set the maximum number of objects per stream
    pub fn max_objects(mut self, max: usize) -> Self {
        self.max_objects = max;
        self
    }

    /// Set the compression level (0-9)
    pub fn compression_level(mut self, level: u32) -> Self {
        self.compression_level = level;
        self
    }

    /// Build the ObjectStream
    pub fn build(self) -> ObjectStream {
        ObjectStream {
            objects: BTreeMap::new(),
            max_objects: self.max_objects,
            compression_level: self.compression_level,
        }
    }

    /// Get the current max_objects setting
    pub fn get_max_objects(&self) -> usize {
        self.max_objects
    }

    /// Get the current compression_level setting
    pub fn get_compression_level(&self) -> u32 {
        self.compression_level
    }
}

#[cfg(test)]
mod selected_member_tests {
    use super::*;
    use crate::Dictionary;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    fn raw_stream(index: &[u8], members: &[u8], n: i64) -> Stream {
        let mut content = index.to_vec();
        content.extend_from_slice(members);
        let mut dict = Dictionary::new();
        dict.set("Type", "ObjStm");
        dict.set("N", n);
        dict.set("First", index.len() as i64);
        Stream::new(dict, content)
    }

    fn generated_stream(members: &[(u32, &[u8])]) -> Stream {
        let mut index = String::new();
        let mut body = Vec::new();
        for (id, object) in members {
            index.push_str(&format!("{id} {} ", body.len()));
            body.extend_from_slice(object);
            body.push(b' ');
        }
        raw_stream(index.as_bytes(), &body, members.len() as i64)
    }

    fn invalid_object_stream(result: Result<Object>) -> String {
        match result {
            Err(Error::InvalidObjectStream(message)) => message,
            other => panic!("expected InvalidObjectStream, got {other:?}"),
        }
    }

    #[derive(Debug, PartialEq)]
    enum MemberFingerprint {
        Value(Object),
        Error,
    }

    fn eager_fingerprint(mut stream: Stream, id: ObjectId) -> MemberFingerprint {
        match ObjectStream::new(&mut stream) {
            Ok(parsed) => parsed
                .objects
                .get(&id)
                .cloned()
                .map(MemberFingerprint::Value)
                .unwrap_or(MemberFingerprint::Error),
            Err(_) => MemberFingerprint::Error,
        }
    }

    fn selected_fingerprint(stream: &Stream, id: ObjectId, index: u32) -> MemberFingerprint {
        ObjectStream::parse_selected_member(stream, id, index)
            .map(MemberFingerprint::Value)
            .unwrap_or(MemberFingerprint::Error)
    }

    fn assert_eager_selected_fingerprint(stream: Stream, id: ObjectId, index: u32, expected: MemberFingerprint) {
        assert_eq!(eager_fingerprint(stream.clone(), id), expected);
        assert_eq!(selected_fingerprint(&stream, id, index), expected);
    }

    #[test]
    fn selected_members_match_the_existing_eager_parser() {
        let members: &[(u32, &[u8])] = &[
            (7, b"42"),
            (11, b"<< /Type /Example /Enabled true >>"),
            (19, b"[1 2 /Three]"),
        ];
        let mut eager_stream = generated_stream(members);
        let eager = ObjectStream::new(&mut eager_stream).unwrap();

        for (index, (id, _)) in members.iter().enumerate() {
            let selected_stream = generated_stream(members);
            let selected = ObjectStream::parse_selected_member(&selected_stream, (*id, 0), index as u32).unwrap();
            assert_eq!(&selected, eager.objects.get(&(*id, 0)).unwrap());
        }
    }

    #[test]
    fn member_index_supports_both_sides_of_the_u16_boundary() {
        const N: u32 = 65_537;
        let mut index = String::new();
        let mut body = Vec::with_capacity(N as usize * 2);
        for member_index in 0..N {
            index.push_str(&format!("{} {} ", member_index + 1, body.len()));
            body.extend_from_slice(b"0 ");
        }
        let stream = raw_stream(index.as_bytes(), &body, i64::from(N));

        assert_eq!(
            ObjectStream::parse_selected_member(&stream, (65_536, 0), 65_535).unwrap(),
            Object::Integer(0)
        );
        assert_eq!(
            ObjectStream::parse_selected_member(&stream, (65_537, 0), 65_536).unwrap(),
            Object::Integer(0)
        );
    }

    #[test]
    fn selected_member_does_not_materialize_other_members() {
        let members: &[(u32, &[u8])] = &[(1, b"<< /Valid true >>"), (2, b"<< /Truncated")];
        let stream = generated_stream(members);
        assert!(ObjectStream::parse_selected_member(&stream, (1, 0), 0).is_ok());

        let stream = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&stream, (2, 0), 1));
        assert!(message.contains("truncated or invalid"));
    }

    #[test]
    fn malformed_header_counts_and_odd_tokens_match_eager_fingerprints() {
        let header = b"10 0 11 5 ";
        let body = b"(ten)(eleven)";
        assert_eager_selected_fingerprint(
            raw_stream(header, body, 1),
            (11, 0),
            1,
            MemberFingerprint::Value(Object::string_literal("eleven")),
        );
        assert_eager_selected_fingerprint(
            raw_stream(header, body, 3),
            (10, 0),
            0,
            MemberFingerprint::Value(Object::string_literal("ten")),
        );
        assert_eager_selected_fingerprint(
            raw_stream(header, body, -1),
            (11, 0),
            1,
            MemberFingerprint::Value(Object::string_literal("eleven")),
        );

        let odd = raw_stream(b"10 0 dangling", b"(ten)", 1);
        assert_eager_selected_fingerprint(
            odd.clone(),
            (10, 0),
            0,
            MemberFingerprint::Value(Object::string_literal("ten")),
        );
        assert_eager_selected_fingerprint(odd, (11, 0), 1, MemberFingerprint::Error);

        let mut invalid_n = raw_stream(b"10 0 ", b"(ten)", 1);
        invalid_n.dict.set("N", "invalid");
        assert_eager_selected_fingerprint(invalid_n, (10, 0), 0, MemberFingerprint::Error);
    }

    #[test]
    fn first_must_bound_the_header() {
        let mut past_end = generated_stream(&[(1, b"42")]);
        let invalid_first = past_end.content.len() as i64 + 1;
        past_end.dict.set("First", invalid_first);
        assert!(matches!(
            ObjectStream::parse_selected_member(&past_end, (1, 0), 0),
            Err(Error::InvalidOffset(_))
        ));

        let mut cuts_header = generated_stream(&[(1, b"42")]);
        cuts_header.dict.set("First", 0i64);
        assert!(ObjectStream::parse_selected_member(&cuts_header, (1, 0), 0).is_err());

        let mut negative = generated_stream(&[(1, b"42")]);
        negative.dict.set("First", -1i64);
        assert!(matches!(
            ObjectStream::parse_selected_member(&negative, (1, 0), 0),
            Err(Error::NumericCast(_))
        ));
    }

    #[test]
    fn xref_member_index_id_and_generation_are_validated() {
        let members: &[(u32, &[u8])] = &[(5, b"42"), (9, b"true")];

        let bad_index = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&bad_index, (9, 0), 2));
        assert!(message.contains("complete header pairs"));

        let bad_id = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&bad_id, (9, 0), 0));
        assert!(message.contains("declares object 5, not 9"));

        let bad_generation = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&bad_generation, (5, 1), 0));
        assert!(message.contains("generation zero"));
    }

    #[test]
    fn duplicate_ids_preserve_eager_acceptance_and_exact_index_selection() {
        let members: &[(u32, &[u8])] = &[(1, b"42"), (1, b"true"), (2, b"false"), (2, b"null"), (3, b"[7]")];
        let mut eager_stream = generated_stream(members);
        let eager = ObjectStream::new(&mut eager_stream).unwrap();

        // Eager collection accepts duplicate declarations and retains the last
        // value for each repeated id.
        assert_eq!(eager.objects.get(&(1, 0)), Some(&Object::Boolean(true)));
        assert_eq!(eager.objects.get(&(2, 0)), Some(&Object::Null));

        let selected_stream = generated_stream(members);
        assert_eq!(
            ObjectStream::parse_selected_member(&selected_stream, (1, 0), 0).unwrap(),
            Object::Integer(42)
        );
        assert_eq!(
            ObjectStream::parse_selected_member(&selected_stream, (1, 0), 1).unwrap(),
            Object::Boolean(true)
        );
        assert_eq!(
            ObjectStream::parse_selected_member(&selected_stream, (2, 0), 2).unwrap(),
            Object::Boolean(false)
        );
        assert_eq!(
            ObjectStream::parse_selected_member(&selected_stream, (2, 0), 3).unwrap(),
            Object::Null
        );
        assert_eq!(
            ObjectStream::parse_selected_member(&selected_stream, (3, 0), 4).unwrap(),
            eager.objects.get(&(3, 0)).unwrap().clone()
        );
    }

    #[test]
    fn malformed_offsets_and_tokens_match_eager_fingerprints() {
        let equal = raw_stream(b"10 0 11 0 ", b"(shared)", 2);
        for (id, index) in [((10, 0), 0), ((11, 0), 1)] {
            assert_eager_selected_fingerprint(
                equal.clone(),
                id,
                index,
                MemberFingerprint::Value(Object::string_literal("shared")),
            );
        }

        let decreasing = raw_stream(b"10 8 11 0 ", b"(eleven)(ten)", 2);
        assert_eager_selected_fingerprint(
            decreasing.clone(),
            (10, 0),
            0,
            MemberFingerprint::Value(Object::string_literal("ten")),
        );
        assert_eager_selected_fingerprint(
            decreasing,
            (11, 0),
            1,
            MemberFingerprint::Value(Object::string_literal("eleven")),
        );

        let unrelated_out_of_bounds = raw_stream(b"10 0 11 99 ", b"(ten)", 2);
        assert_eager_selected_fingerprint(
            unrelated_out_of_bounds.clone(),
            (10, 0),
            0,
            MemberFingerprint::Value(Object::string_literal("ten")),
        );
        assert_eager_selected_fingerprint(unrelated_out_of_bounds, (11, 0), 1, MemberFingerprint::Error);

        let unrelated_invalid = raw_stream(b"10 0 invalid offset ", b"(ten)", 2);
        assert_eager_selected_fingerprint(
            unrelated_invalid,
            (10, 0),
            0,
            MemberFingerprint::Value(Object::string_literal("ten")),
        );

        let target_invalid = raw_stream(b"10 invalid 11 0 ", b"(eleven)", 2);
        assert_eager_selected_fingerprint(target_invalid.clone(), (10, 0), 0, MemberFingerprint::Error);
        assert_eager_selected_fingerprint(
            target_invalid,
            (11, 0),
            1,
            MemberFingerprint::Value(Object::string_literal("eleven")),
        );
    }

    #[test]
    fn selected_member_uses_eager_prefix_parse_policy() {
        let valid_comment = generated_stream(&[(1, b"42 % trailing comment")]);
        assert_eq!(
            ObjectStream::parse_selected_member(&valid_comment, (1, 0), 0).unwrap(),
            Object::Integer(42)
        );

        let trailing_object = generated_stream(&[(1, b"42 true"), (2, b"false")]);
        assert_eager_selected_fingerprint(
            trailing_object,
            (1, 0),
            0,
            MemberFingerprint::Value(Object::Integer(42)),
        );
    }

    #[test]
    fn unbounded_decompression_failure_matches_eager_raw_byte_fallback() {
        let mut dict = Dictionary::new();
        dict.set("Type", "ObjStm");
        dict.set("N", 1i64);
        dict.set("First", 4i64);
        dict.set("Filter", "UnsupportedDecode");
        let stream = Stream::new(dict, b"1 0 42".to_vec());

        assert_eager_selected_fingerprint(stream, (1, 0), 0, MemberFingerprint::Value(Object::Integer(42)));
    }

    #[test]
    fn selected_member_count_is_capped() {
        for accepted_count in [
            MAX_SELECTED_OBJECT_STREAM_MEMBERS - 1,
            MAX_SELECTED_OBJECT_STREAM_MEMBERS,
        ] {
            let within_limit = raw_stream(b"1 0 ", b"42 ", accepted_count as i64);
            assert_eq!(
                ObjectStream::parse_selected_member(&within_limit, (1, 0), 0).unwrap(),
                Object::Integer(42)
            );
        }

        let above_limit = raw_stream(
            b"1 0 ",
            b"42 ",
            i64::try_from(MAX_SELECTED_OBJECT_STREAM_MEMBERS + 1).unwrap(),
        );
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&above_limit, (1, 0), 0));
        assert!(message.contains("exceeds selected-parser limit"));

        let oversized_header = "1 0 ".repeat(MAX_SELECTED_OBJECT_STREAM_MEMBERS + 1);
        let actual_above_limit = raw_stream(oversized_header.as_bytes(), b"42 ", 1);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&actual_above_limit, (1, 0), 0));
        assert!(message.contains("parsed member count"));
    }

    #[test]
    fn repeated_tiny_selection_does_not_mutate_or_retain_large_container() {
        const LARGE_MEMBER_SIZE: usize = 4 * 1024 * 1024;
        let large_member = vec![b' '; LARGE_MEMBER_SIZE];
        let mut stream = generated_stream(&[(1, b"42"), (2, &large_member)]);
        stream.compress().unwrap();
        let original = stream.clone();
        let mut selected = Vec::new();

        for _ in 0..3 {
            selected.push(
                ObjectStream::parse_selected_member_with_limit(&stream, (1, 0), 0, Some(LARGE_MEMBER_SIZE + 1024))
                    .unwrap(),
            );
            assert_eq!(stream, original);
        }

        drop(stream);
        assert_eq!(selected, vec![Object::Integer(42); 3]);
    }

    #[test]
    fn selected_member_honors_the_decompression_limit() {
        const LIMIT: usize = 4096;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&vec![b'0'; LIMIT * 4]).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut dict = Dictionary::new();
        dict.set("Type", "ObjStm");
        dict.set("N", 1i64);
        dict.set("First", 4i64);
        dict.set("Filter", "FlateDecode");
        let stream = Stream::new(dict, compressed);

        assert!(matches!(
            ObjectStream::parse_selected_member_with_limit(&stream, (1, 0), 0, Some(LIMIT)),
            Err(Error::Decompress(DecompressError::MemoryLimitExceeded { limit: LIMIT }))
        ));
    }
}
