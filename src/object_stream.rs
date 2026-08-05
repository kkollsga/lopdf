use crate::parser;
use crate::{Document, Error, Object, ObjectId, Result, Stream};
use std::collections::{BTreeMap, BTreeSet};
use std::num::TryFromIntError;
use std::str::FromStr;

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
    pub fn parse_selected_member(stream: &mut Stream, expected_id: ObjectId, member_index: u16) -> Result<Object> {
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
        stream: &mut Stream, expected_id: ObjectId, member_index: u16, max_decompressed_size: Option<usize>,
    ) -> Result<Object> {
        if expected_id.1 != 0 {
            return Err(Error::InvalidObjectStream(
                "compressed objects must have generation zero".to_string(),
            ));
        }

        match max_decompressed_size {
            Some(max) => stream.decompress_with_limit(max)?,
            None => {
                stream.decompress()?;
            }
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
        let member_index = usize::from(member_index);

        if member_index >= n {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} is outside declared count {n}"
            )));
        }

        let index_block = stream.content.get(..first).ok_or(Error::InvalidOffset(first))?;
        let index_text = std::str::from_utf8(index_block).map_err(|e| Error::InvalidObjectStream(e.to_string()))?;
        let mut numbers = index_text.split_whitespace();
        let mut entries = Vec::new();
        let mut ids = BTreeSet::new();
        let mut previous_offset = None;

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
            if absolute_offset >= stream.content.len() {
                return Err(Error::InvalidOffset(absolute_offset));
            }

            entries.push((id, absolute_offset));
            previous_offset = Some(relative_offset);
        }

        if numbers.next().is_some() {
            return Err(Error::InvalidObjectStream(format!(
                "object stream header has more than the declared {n} members"
            )));
        }

        let (declared_id, start) = entries[member_index];
        if declared_id != expected_id.0 {
            return Err(Error::InvalidObjectStream(format!(
                "member index {member_index} declares object {declared_id}, not {}",
                expected_id.0
            )));
        }

        let end = entries
            .get(member_index + 1)
            .map(|(_, offset)| *offset)
            .unwrap_or(stream.content.len());
        let member = &stream.content[start..end];
        let member = member
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|leading| &member[leading..])
            .ok_or_else(|| Error::InvalidObjectStream("selected object stream member is empty".to_string()))?;

        parser::direct_object(member).ok_or_else(|| {
            Error::InvalidObjectStream("selected object stream member is truncated or invalid".to_string())
        })
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
    use crate::{DecompressError, Dictionary};
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
            let mut selected_stream = generated_stream(members);
            let selected = ObjectStream::parse_selected_member(&mut selected_stream, (*id, 0), index as u16).unwrap();
            assert_eq!(&selected, eager.objects.get(&(*id, 0)).unwrap());
        }
    }

    #[test]
    fn selected_member_does_not_materialize_other_members() {
        let members: &[(u32, &[u8])] = &[(1, b"<< /Valid true >>"), (2, b"<< /Truncated")];
        let mut stream = generated_stream(members);
        assert!(ObjectStream::parse_selected_member(&mut stream, (1, 0), 0).is_ok());

        let mut stream = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut stream, (2, 0), 1));
        assert!(message.contains("truncated or invalid"));
    }

    #[test]
    fn declared_count_must_match_the_complete_header() {
        let mut too_large = raw_stream(b"1 0 2 3 ", b"42 true ", 3);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut too_large, (1, 0), 0));
        assert!(message.contains("ended before object id for member 2"));

        let mut too_small = raw_stream(b"1 0 2 3 ", b"42 true ", 1);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut too_small, (1, 0), 0));
        assert!(message.contains("more than the declared 1 members"));

        let mut negative = raw_stream(b"1 0 ", b"42 ", -1);
        assert!(matches!(
            ObjectStream::parse_selected_member(&mut negative, (1, 0), 0),
            Err(Error::NumericCast(_))
        ));
    }

    #[test]
    fn first_must_bound_the_header() {
        let mut past_end = generated_stream(&[(1, b"42")]);
        let invalid_first = past_end.content.len() as i64 + 1;
        past_end.dict.set("First", invalid_first);
        assert!(matches!(
            ObjectStream::parse_selected_member(&mut past_end, (1, 0), 0),
            Err(Error::InvalidOffset(_))
        ));

        let mut cuts_header = generated_stream(&[(1, b"42")]);
        cuts_header.dict.set("First", 0i64);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut cuts_header, (1, 0), 0));
        assert!(message.contains("ended before object id"));

        let mut negative = generated_stream(&[(1, b"42")]);
        negative.dict.set("First", -1i64);
        assert!(matches!(
            ObjectStream::parse_selected_member(&mut negative, (1, 0), 0),
            Err(Error::NumericCast(_))
        ));
    }

    #[test]
    fn xref_member_index_id_and_generation_are_validated() {
        let members: &[(u32, &[u8])] = &[(5, b"42"), (9, b"true")];

        let mut bad_index = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut bad_index, (9, 0), 2));
        assert!(message.contains("outside declared count"));

        let mut bad_id = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut bad_id, (9, 0), 0));
        assert!(message.contains("declares object 5, not 9"));

        let mut bad_generation = generated_stream(members);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut bad_generation, (5, 1), 0));
        assert!(message.contains("generation zero"));
    }

    #[test]
    fn duplicate_ids_and_invalid_offsets_are_rejected() {
        let mut duplicate_id = raw_stream(b"1 0 1 3 ", b"42 true ", 2);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut duplicate_id, (1, 0), 0));
        assert!(message.contains("duplicate object id 1"));

        let mut duplicate_offset = raw_stream(b"1 0 2 0 ", b"42 true ", 2);
        let message = invalid_object_stream(ObjectStream::parse_selected_member(&mut duplicate_offset, (1, 0), 0));
        assert!(message.contains("not strictly increasing"));

        let mut out_of_bounds = raw_stream(b"1 50 ", b"42 ", 1);
        assert!(matches!(
            ObjectStream::parse_selected_member(&mut out_of_bounds, (1, 0), 0),
            Err(Error::InvalidOffset(_))
        ));
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
        let mut stream = Stream::new(dict, compressed);

        assert!(matches!(
            ObjectStream::parse_selected_member_with_limit(&mut stream, (1, 0), 0, Some(LIMIT)),
            Err(Error::Decompress(DecompressError::MemoryLimitExceeded { limit: LIMIT }))
        ));
    }
}
