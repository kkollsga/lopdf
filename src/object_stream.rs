use crate::parser;
use crate::{DecompressError, Document, Error, Object, ObjectId, Result, Stream};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::TryFromIntError;
use std::str::FromStr;

const MAX_SELECTED_OBJECT_STREAM_MEMBERS: usize = 131_072;

use log::warn;
#[cfg(feature = "rayon")]
use rayon::prelude::*;

#[derive(Debug)]
pub struct ObjectStream {
    pub objects: BTreeMap<ObjectId, Object>,
    max_objects: usize,
    compression_level: u32,
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
    /// the other members are validated as compact `(id, offset)` metadata.
    ///
    /// This decompresses without a size limit. For untrusted input, prefer
    /// [`ObjectStream::parse_selected_member_with_limit`].
    pub fn parse_selected_member(stream: &Stream, expected_id: ObjectId, member_index: u32) -> Result<Object> {
        Self::parse_selected_member_with_limit(stream, expected_id, member_index, None)
    }

    /// Parse one declared member, rejecting the object stream if decompression
    /// exceeds `max_decompressed_size`. `None` means no decompression limit.
    ///
    /// Unlike [`ObjectStream::new_with_limit`], malformed header counts,
    /// duplicate ids, invalid offsets, xref index/id disagreement and truncated
    /// selected objects are errors. This is the strict contract needed by an
    /// indexed resolver; the eager constructor retains its existing leniency.
    pub fn parse_selected_member_with_limit(
        stream: &Stream, expected_id: ObjectId, member_index: u32, max_decompressed_size: Option<usize>,
    ) -> Result<Object> {
        if expected_id.1 != 0 {
            return Err(Error::InvalidObjectStream(
                "compressed objects must have generation zero".to_string(),
            ));
        }

        let first = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)?
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        let n: usize = stream
            .dict
            .get(b"N")
            .and_then(Object::as_i64)?
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;
        if n > MAX_SELECTED_OBJECT_STREAM_MEMBERS {
            return Err(Error::InvalidObjectStream(format!(
                "declared member count {n} exceeds selected-parser limit {MAX_SELECTED_OBJECT_STREAM_MEMBERS}"
            )));
        }
        let member_index: usize = member_index
            .try_into()
            .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;

        if member_index >= n {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} is outside declared count {n}"
            )));
        }

        // Decode into call-local storage. This leaves the caller's compressed
        // stream unchanged and drops the full object-stream container before
        // returning the selected object.
        let decoded = if stream.is_compressed() {
            Cow::Owned(match max_decompressed_size {
                Some(max) => stream.decompressed_content_with_limit(max)?,
                None => stream.decompressed_content()?,
            })
        } else {
            if let Some(max) = max_decompressed_size
                && stream.content.len() > max
            {
                return Err(DecompressError::MemoryLimitExceeded { limit: max }.into());
            }
            Cow::Borrowed(stream.content.as_slice())
        };

        let index_block = decoded.get(..first).ok_or(Error::InvalidOffset(first))?;
        let index_text = std::str::from_utf8(index_block).map_err(|e| Error::InvalidObjectStream(e.to_string()))?;
        let mut numbers = index_text.split_whitespace();
        let mut ids = BTreeSet::new();
        let mut previous_offset = None;
        let mut selected = None;
        let mut selected_end = None;

        for index in 0..n {
            let id = parse_object_stream_number(numbers.next(), "object id", index)?;
            let relative_offset: usize = parse_object_stream_number(numbers.next(), "member offset", index)?
                .try_into()
                .map_err(|e: TryFromIntError| Error::NumericCast(e.to_string()))?;

            if !ids.insert(id) {
                return Err(Error::InvalidObjectStream(format!(
                    "duplicate object id {id} in object stream header"
                )));
            }
            if previous_offset.is_some_and(|previous| relative_offset <= previous) {
                return Err(Error::InvalidObjectStream(
                    "object stream member offsets are not strictly increasing".to_string(),
                ));
            }

            let absolute_offset = first
                .checked_add(relative_offset)
                .ok_or_else(|| Error::InvalidObjectStream("object stream member offset overflow".to_string()))?;
            if absolute_offset >= decoded.len() {
                return Err(Error::InvalidOffset(absolute_offset));
            }

            if index == member_index {
                selected = Some((id, absolute_offset));
            } else if index == member_index + 1 {
                selected_end = Some(absolute_offset);
            }
            previous_offset = Some(relative_offset);
        }

        if numbers.next().is_some() {
            return Err(Error::InvalidObjectStream(format!(
                "object stream header has more than the declared {n} members"
            )));
        }

        let (declared_id, start) = selected
            .ok_or_else(|| Error::InvalidObjectStream("selected object stream member was not declared".to_string()))?;
        if declared_id != expected_id.0 {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} declares object {declared_id}, not {}",
                expected_id.0
            )));
        }

        let end = selected_end.unwrap_or(decoded.len());
        let member = &decoded[start..end];
        let member = member
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|leading| &member[leading..])
            .ok_or_else(|| Error::InvalidObjectStream("selected object stream member is empty".to_string()))?;

        let (remainder, object) = parser::direct_object_with_remainder(member).ok_or_else(|| {
            Error::InvalidObjectStream("selected object stream member is truncated or invalid".to_string())
        })?;
        if !is_pdf_whitespace_or_comments(remainder) {
            return Err(Error::InvalidObjectStream(
                "selected object stream member has trailing non-whitespace data".to_string(),
            ));
        }
        Ok(object)
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

fn parse_object_stream_number(value: Option<&str>, kind: &str, index: usize) -> Result<u32> {
    let value = value.ok_or_else(|| {
        Error::InvalidObjectStream(format!("object stream header ended before {kind} for member {index}"))
    })?;
    value
        .parse()
        .map_err(|_| Error::InvalidObjectStream(format!("invalid {kind} for object stream member {index}")))
}

fn is_pdf_whitespace_or_comments(mut input: &[u8]) -> bool {
    while let Some((&byte, remainder)) = input.split_first() {
        if b" \t\n\r\0\x0C".contains(&byte) {
            input = remainder;
        } else if byte == b'%' {
            input = remainder;
            while let Some((&comment_byte, remainder)) = input.split_first() {
                input = remainder;
                if matches!(comment_byte, b'\r' | b'\n') {
                    break;
                }
            }
        } else {
            return false;
        }
    }
    true
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
    fn declared_count_must_match_the_complete_header() {
        let too_large = raw_stream(b"1 0 2 3 ", b"42 true ", 3);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&too_large, (1, 0), 0));
        assert!(message.contains("ended before object id for member 2"));

        let too_small = raw_stream(b"1 0 2 3 ", b"42 true ", 1);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&too_small, (1, 0), 0));
        assert!(message.contains("more than the declared 1 members"));

        let negative = raw_stream(b"1 0 ", b"42 ", -1);
        assert!(matches!(
            ObjectStream::parse_selected_member(&negative, (1, 0), 0),
            Err(Error::NumericCast(_))
        ));
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
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&cuts_header, (1, 0), 0));
        assert!(message.contains("ended before object id"));

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
        assert!(message.contains("outside declared count"));

        let bad_id = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&bad_id, (9, 0), 0));
        assert!(message.contains("declares object 5, not 9"));

        let bad_generation = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&bad_generation, (5, 1), 0));
        assert!(message.contains("generation zero"));
    }

    #[test]
    fn duplicate_ids_and_invalid_offsets_are_rejected() {
        let mut duplicate_id = raw_stream(b"1 0 1 3 ", b"42 true ", 2);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&duplicate_id, (1, 0), 0));
        assert!(message.contains("duplicate object id 1"));

        let eager = ObjectStream::new(&mut duplicate_id).unwrap();
        assert_eq!(eager.objects.get(&(1, 0)), Some(&Object::Boolean(true)));

        let duplicate_offset = raw_stream(b"1 0 2 0 ", b"42 true ", 2);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&duplicate_offset, (1, 0), 0));
        assert!(message.contains("not strictly increasing"));

        let out_of_bounds = raw_stream(b"1 50 ", b"42 ", 1);
        assert!(matches!(
            ObjectStream::parse_selected_member(&out_of_bounds, (1, 0), 0),
            Err(Error::InvalidOffset(_))
        ));
    }

    #[test]
    fn selected_member_rejects_trailing_non_whitespace_data() {
        let valid_comment = generated_stream(&[(1, b"42 % trailing comment")]);
        assert_eq!(
            ObjectStream::parse_selected_member(&valid_comment, (1, 0), 0).unwrap(),
            Object::Integer(42)
        );

        let trailing_object = generated_stream(&[(1, b"42 true"), (2, b"false")]);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&trailing_object, (1, 0), 0));
        assert!(message.contains("trailing non-whitespace data"));
    }

    #[test]
    fn selected_member_metadata_count_is_capped() {
        for accepted_count in [
            MAX_SELECTED_OBJECT_STREAM_MEMBERS - 1,
            MAX_SELECTED_OBJECT_STREAM_MEMBERS,
        ] {
            let within_limit = raw_stream(b"1 0 ", b"42 ", accepted_count as i64);
            let message = invalid_object_stream(ObjectStream::parse_selected_member(&within_limit, (1, 0), 0));
            assert!(message.contains("ended before object id for member 1"));
        }

        let above_limit = raw_stream(
            b"1 0 ",
            b"42 ",
            i64::try_from(MAX_SELECTED_OBJECT_STREAM_MEMBERS + 1).unwrap(),
        );
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&above_limit, (1, 0), 0));
        assert!(message.contains("exceeds selected-parser limit"));
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
