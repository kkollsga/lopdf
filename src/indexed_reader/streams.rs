//! Encoded-stream descriptors, readers, and bounded object-stream containers.

use super::*;

/// Classification of an indexed stream's encoded bytes.
///
/// Only [`Plain`](Self::Plain) descriptors can currently be opened. Protected
/// streams continue to use the indexed reader's existing materialized object
/// path, which performs the required decryption before exposing bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodedStreamProtection {
    /// The encoded bytes are not protected by document or stream encryption.
    Plain,
    /// The PDF has an authenticated encryption state, so this stream must use
    /// the existing decrypting object path.
    DocumentEncrypted,
    /// The stream explicitly names the PDF `Crypt` filter.
    CryptFilter,
    /// Filter metadata is malformed or could not be resolved within the
    /// indexed reader's ordinary reference and resource limits.
    UnresolvedFilter,
}

/// Provenance-aware encoded stream length.
///
/// A scalar object resolution may preserve compatibility by degrading an
/// unavailable `/Length` to empty owned content.  Streaming callers must not
/// confuse that fallback with a proven zero-byte encoded span.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodedStreamLength {
    /// A validated encoded payload span with this exact byte length.
    Known(u64),
    /// No safe encoded payload span can be opened for the stated reason.
    Unavailable(EncodedStreamLengthUnavailableReason),
}

/// Reason an encoded stream's payload length is unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodedStreamLengthUnavailableReason {
    /// `/Length` is absent, malformed, or cannot be resolved within the
    /// indexed reader's bounded reference rules.
    MissingOrInvalid,
}

/// Owned metadata and a private checked span for one encoded PDF stream.
///
/// The descriptor never owns payload bytes and intentionally exposes neither
/// the source offset nor encryption key material. It may be moved or shared
/// across threads; each opened reader has an independent cursor. Length checks
/// cannot detect a same-length byte rewrite, so the source's immutable-byte
/// contract remains required for the descriptor's entire lifetime.
pub struct IndexedStreamDescriptor {
    pub(super) id: crate::ObjectId,
    dictionary: Dictionary,
    encoded_length: EncodedStreamLength,
    protection: EncodedStreamProtection,
    source: Arc<dyn RandomAccessSource>,
    source_len: u64,
    encoded_start: u64,
    metadata_frame_bytes: u64,
    _metadata_charge: Option<crate::scalar_budget::ScalarCharge>,
}

impl std::fmt::Debug for IndexedStreamDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexedStreamDescriptor")
            .field("id", &self.id)
            .field("dictionary", &self.dictionary)
            .field("encoded_length", &self.encoded_length)
            .field("protection", &self.protection)
            .finish_non_exhaustive()
    }
}

impl IndexedStreamDescriptor {
    /// Object id whose stream metadata was resolved.
    pub const fn id(&self) -> crate::ObjectId {
        self.id
    }

    /// Cloned stream dictionary. No payload bytes are retained by it.
    pub const fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// Provenance-aware encoded payload length.
    pub const fn encoded_length(&self) -> EncodedStreamLength {
        self.encoded_length
    }

    /// Checked encoded payload length when a safe span is available.
    pub const fn encoded_len(&self) -> Option<u64> {
        match self.encoded_length {
            EncodedStreamLength::Known(length) => Some(length),
            EncodedStreamLength::Unavailable(_) => None,
        }
    }

    /// Whether the encoded bytes can be opened by the L0 plain reader.
    pub const fn protection(&self) -> EncodedStreamProtection {
        self.protection
    }

    /// Conservative simultaneous-allocation bound for resolving this normal
    /// stream through [`IndexedReader::resolve_stream_with_permit`]. The bound
    /// covers frame growth, dictionary AST/transients, encoded content, and
    /// the worst supported AES decryption overlap without scaling a tiny
    /// object by an arbitrary global multiplier.
    pub fn bounded_resolution_bytes(&self) -> Option<u64> {
        let encoded = self.encoded_len()?;
        let dictionary = u64::try_from(dictionary_retained_bytes(&self.dictionary)).unwrap_or(u64::MAX);
        let metadata_window = self.metadata_frame_bytes.max(INITIAL_OBJECT_WINDOW);
        let metadata_peak = metadata_window
            .saturating_mul(3)
            .saturating_add(dictionary.saturating_mul(2))
            .saturating_add(INDIRECT_HEADER_LIMIT);
        let content_peak = dictionary.saturating_add(encoded.saturating_mul(3)).saturating_add(64);
        Some(metadata_peak.max(content_peak))
    }

    /// Open an independent bounded reader for unencrypted encoded bytes.
    ///
    /// Protected streams fail before any payload read. Decoding PDF filter
    /// chains is intentionally outside this reader's contract.
    pub fn open_plain_encoded(&self) -> IndexedStreamReadResult<EncodedStreamReader> {
        let encoded_len = match self.encoded_length {
            EncodedStreamLength::Known(length) => length,
            EncodedStreamLength::Unavailable(reason) => {
                return Err(IndexedStreamReadError::LengthUnavailable { id: self.id, reason });
            }
        };
        if self.protection != EncodedStreamProtection::Plain {
            return Err(IndexedStreamReadError::Protected {
                id: self.id,
                protection: self.protection,
            });
        }
        let actual = self.source.len()?;
        if actual != self.source_len {
            return Err(IndexedStreamReadError::SourceLengthChanged {
                expected: self.source_len,
                actual,
            });
        }
        Ok(EncodedStreamReader {
            id: self.id,
            source: Arc::clone(&self.source),
            source_len: self.source_len,
            encoded_start: self.encoded_start,
            encoded_len,
            position: 0,
        })
    }
}

/// Cursor over one descriptor's checked encoded stream span.
///
/// A call fills at most 64 KiB, even when the caller supplies a larger buffer.
/// The reader retries interrupted and partial positional reads without a
/// shared source cursor.
pub struct EncodedStreamReader {
    pub(super) id: crate::ObjectId,
    source: Arc<dyn RandomAccessSource>,
    source_len: u64,
    encoded_start: u64,
    encoded_len: u64,
    position: u64,
}

impl std::fmt::Debug for EncodedStreamReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncodedStreamReader")
            .field("id", &self.id)
            .field("encoded_len", &self.encoded_len)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl EncodedStreamReader {
    /// Return the number of encoded bytes not yet read.
    pub const fn remaining(&self) -> u64 {
        self.encoded_len - self.position
    }

    /// Fill a bounded chunk from the encoded payload.
    pub fn read_chunk(&mut self, output: &mut [u8]) -> IndexedStreamReadResult<usize> {
        if output.is_empty() || self.position == self.encoded_len {
            return Ok(0);
        }
        let actual_len = self.source.len()?;
        if actual_len != self.source_len {
            return Err(IndexedStreamReadError::SourceLengthChanged {
                expected: self.source_len,
                actual: actual_len,
            });
        }
        let requested = output
            .len()
            .min(ENCODED_STREAM_CHUNK_LIMIT)
            .min(usize::try_from(self.remaining()).unwrap_or(usize::MAX));
        let absolute = self
            .encoded_start
            .checked_add(self.position)
            .ok_or(SourceError::RangeOverflow {
                offset: self.encoded_start,
                length: self.position,
            })?;
        let mut completed = 0;
        while completed < requested {
            let offset = absolute
                .checked_add(u64::try_from(completed).map_err(|_| SourceError::RangeOverflow {
                    offset: absolute,
                    length: u64::MAX,
                })?)
                .ok_or(SourceError::RangeOverflow {
                    offset: absolute,
                    length: u64::try_from(requested).unwrap_or(u64::MAX),
                })?;
            let remaining = requested - completed;
            match self.source.read_at(offset, &mut output[completed..requested]) {
                Ok(0) => {
                    return Err(SourceError::UnexpectedEof {
                        offset: absolute,
                        expected: u64::try_from(requested).unwrap_or(u64::MAX),
                        actual: u64::try_from(completed).unwrap_or(u64::MAX),
                    }
                    .into());
                }
                Ok(read) if read > remaining => {
                    return Err(SourceError::InvalidReadCount {
                        returned: read,
                        buffer_len: remaining,
                    }
                    .into());
                }
                Ok(read) => completed += read,
                Err(SourceError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        let actual_len = self.source.len()?;
        if actual_len != self.source_len {
            return Err(IndexedStreamReadError::SourceLengthChanged {
                expected: self.source_len,
                actual: actual_len,
            });
        }
        let completed_u64 = u64::try_from(completed).map_err(|_| SourceError::RangeOverflow {
            offset: absolute,
            length: u64::MAX,
        })?;
        self.position = self
            .position
            .checked_add(completed_u64)
            .ok_or(SourceError::RangeOverflow {
                offset: self.position,
                length: completed_u64,
            })?;
        Ok(completed)
    }
}

pub(super) enum PreparedObjectStream {
    Selected(crate::object_stream::SelectedObjectStream),
    Raw(Stream),
    NotStream,
}

pub(super) enum BoundedPreparedObjectStream {
    Cached(Arc<PreparedObjectStream>),
    CallLocal {
        prepared: Arc<PreparedObjectStream>,
        _charges: Vec<ScalarCharge>,
    },
}

/// One decoded and indexed object-stream container retained under a caller's
/// scalar-resolution permit.
///
/// The owner is intentionally cache-agnostic. A downstream caller may place it
/// in a container-keyed cell, and may parse any declared member in that
/// container without decoding it again. Container and member allocations stay
/// charged until their respective bounded owners are dropped.
pub struct BoundedObjectStream {
    pub(super) container_id: crate::ObjectId,
    pub(super) selected: crate::object_stream::SelectedObjectStream,
    pub(super) permit: ScalarResolutionPermit,
    pub(super) charges: Box<[ScalarCharge]>,
}

/// Conservative bytes retained by a prepared object-stream owner which are
/// deliberately not included in [`BoundedObjectStream::retained_bytes`].
///
/// The decoded buffer and member-index capacities are charged by the scalar
/// permit. This envelope covers their allocation owners, the shared permit
/// state, two retained charge handles, and allocator headers. Downstream cell
/// caches must precharge at least this amount before preparing a container.
pub const BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES: u64 = 512;

pub(super) const BOUNDED_OBJECT_STREAM_CHARGE_HANDLES: usize = 2;
const BOUNDED_OBJECT_STREAM_ALLOCATIONS: usize = 5;
const ALLOCATOR_HEADER_ENVELOPE_BYTES: usize = 2 * std::mem::size_of::<usize>();
pub(super) const BOUNDED_OBJECT_STREAM_EXCLUDED_STRUCTURAL_BYTES: usize = std::mem::size_of::<Vec<u8>>()
    + 4 * std::mem::size_of::<usize>()
    + crate::scalar_budget::PERMIT_INNER_STRUCTURAL_BYTES
    + BOUNDED_OBJECT_STREAM_CHARGE_HANDLES * crate::scalar_budget::SCALAR_CHARGE_STRUCTURAL_BYTES
    + BOUNDED_OBJECT_STREAM_ALLOCATIONS * ALLOCATOR_HEADER_ENVELOPE_BYTES;
const _: () = assert!(
    BOUNDED_OBJECT_STREAM_EXCLUDED_STRUCTURAL_BYTES <= BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES as usize
);

impl std::fmt::Debug for BoundedObjectStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedObjectStream")
            .field("container_id", &self.container_id)
            .field("retained_bytes", &self.retained_bytes())
            .finish_non_exhaustive()
    }
}

impl BoundedObjectStream {
    /// Normal object id of this prepared object-stream container.
    pub const fn container_id(&self) -> crate::ObjectId {
        self.container_id
    }

    /// Bytes currently charged for the retained decoded container and index.
    pub fn retained_bytes(&self) -> u64 {
        self.charges
            .iter()
            .map(ScalarCharge::bytes)
            .fold(0, u64::saturating_add)
    }

    /// Exact target-layout structural weight excluded from `retained_bytes`.
    /// The returned value is bounded by
    /// [`BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES`] in production builds.
    pub fn excluded_structural_bytes(&self) -> u64 {
        let actual = std::mem::size_of::<Vec<u8>>()
            + 4 * std::mem::size_of::<usize>()
            + crate::scalar_budget::PERMIT_INNER_STRUCTURAL_BYTES
            + self.charges.len() * crate::scalar_budget::SCALAR_CHARGE_STRUCTURAL_BYTES
            + BOUNDED_OBJECT_STREAM_ALLOCATIONS * ALLOCATOR_HEADER_ENVELOPE_BYTES;
        u64::try_from(actual).unwrap_or(u64::MAX)
    }

    /// Parse one declared member while keeping both the prepared container and
    /// returned scalar charged to the original caller permit.
    pub fn resolve_member(&self, id: crate::ObjectId, index: u32) -> IndexedReaderResult<BoundedScalar> {
        self.resolve_member_with_permit(id, index, &self.permit)
    }

    /// Parse one declared member under an independent caller-owned permit.
    ///
    /// The prepared container remains charged to the permit which admitted
    /// [`IndexedReader::prepare_object_stream_with_permit`], while the returned
    /// scalar remains charged to `permit`. This lets downstream caches retain a
    /// decoded container independently from any member objects derived from it.
    pub fn resolve_member_with_permit(
        &self, id: crate::ObjectId, index: u32, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        let member =
            self.selected
                .member_slice(id, index)
                .map_err(|source| IndexedReaderError::ObjectStreamMember {
                    id,
                    container: self.container_id,
                    index,
                    source,
                })?;
        let ast_bound = scalar_ast_preflight(member)
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: self.container_id,
                index,
                source: crate::Error::InvalidObjectStream(
                    "selected object stream member is truncated or invalid".to_string(),
                ),
            })?
            .reserved_bytes(false);
        let mut ast_charge = permit.reserve(id, ast_bound, "object-stream-member-ast")?;
        let object = crate::parser::direct_object(member).ok_or_else(|| IndexedReaderError::ObjectStreamMember {
            id,
            container: self.container_id,
            index,
            source: crate::Error::InvalidObjectStream(
                "selected object stream member is truncated or invalid".to_string(),
            ),
        })?;
        let retained = u64::try_from(scalar_object_retained_bytes(&object)).unwrap_or(u64::MAX);
        if retained > ast_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: retained,
                limit: ast_charge.bytes(),
                phase: "measured-object-stream-member",
            });
        }
        ast_charge.shrink_to(retained);
        let peak = permit.stats().peak_bytes;
        Ok(BoundedScalar::new(object, retained, peak, ast_charge))
    }
}

impl BoundedPreparedObjectStream {
    pub(super) fn prepared(&self) -> &PreparedObjectStream {
        match self {
            Self::Cached(prepared) => prepared,
            Self::CallLocal { prepared, .. } => prepared,
        }
    }
}

impl PreparedObjectStream {
    pub(super) fn retained_bytes(&self) -> usize {
        match self {
            Self::Selected(selected) => selected.retained_bytes(),
            Self::Raw(stream) => stream.content.capacity().saturating_add(std::mem::size_of::<Stream>()),
            Self::NotStream => std::mem::size_of::<Self>(),
        }
    }

    pub(super) fn cache_weight(&self) -> usize {
        self.retained_bytes().saturating_add(OBJECT_STREAM_CACHE_ENTRY_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LimitedObjectStreamEncoding {
    /// The payload is already the object-stream body; no decode runs at all.
    Plain,
    /// The payload decodes in one bounded pass through
    /// [`Stream::decompressed_content_with_limit`], which caps every filter
    /// layer's output at the charged allowance.
    Decoded,
}

/// Longest filter chain admitted for a bounded object-stream decode.
///
/// Each layer holds its predecessor's output alive while producing its own, so
/// the chain length is what turns the per-layer output cap into a peak-memory
/// bound. Real object streams use one or two layers.
const MAX_OBJECT_STREAM_FILTERS: usize = 4;

/// Widest predictor row admitted for a bounded object-stream decode.
///
/// `png::decode_frame` reserves two rows before it reads anything, and that
/// pair is outside the permit's per-layer output accounting, so it is bounded
/// here instead. Cross-reference and object streams predict over a handful of
/// columns; this leaves three orders of magnitude of headroom.
const MAX_OBJECT_STREAM_PREDICTOR_ROW_BYTES: i64 = 64 * 1024;

/// Which encoding of `dictionary`'s payload the bounded object-stream path can
/// reproduce faithfully, or `None` to fail closed and let the caller fall back.
///
/// The envelope is deliberately narrower than [`Stream::decode_filters`]
/// supports: it admits only the forms that decoder decodes *correctly*, and
/// only the ones whose peak memory the permit can charge. `/DecodeParms` is
/// read the same way the decoder reads it — [`Stream::decode_parms`] zips the
/// array form (ISO 32000-1, 7.4.1) onto the filter chain — so the check here
/// looks at whatever entry the *terminal* Flate/LZW layer will actually
/// receive; the ASCIIHex/ASCII85 prefix this envelope admits takes no
/// parameters at all and ignores whatever sits opposite it.
pub(super) fn limited_object_stream_encoding(dictionary: &Dictionary) -> Option<LimitedObjectStreamEncoding> {
    let filters: Vec<&[u8]> = match dictionary.get(b"Filter") {
        Err(_) | Ok(Object::Null) => return Some(LimitedObjectStreamEncoding::Plain),
        Ok(Object::Name(filter)) => vec![filter.as_slice()],
        Ok(Object::Array(items)) => {
            if items.len() > MAX_OBJECT_STREAM_FILTERS {
                return None;
            }
            items.iter().map(|item| item.as_name().ok()).collect::<Option<_>>()?
        }
        Ok(_) => return None,
    };
    // An empty `/Filter []` declares no encoding, which is the plain payload.
    // `decode_filters` would run zero layers and return an empty buffer, so this
    // case must never reach it.
    let Some((terminal, prefix)) = filters.split_last() else {
        return Some(LimitedObjectStreamEncoding::Plain);
    };
    if !matches!(*terminal, b"FlateDecode" | b"LZWDecode") {
        return None;
    }
    if !prefix
        .iter()
        .all(|filter| matches!(*filter, b"ASCIIHexDecode" | b"ASCII85Decode"))
    {
        return None;
    }
    // Whatever the terminal Flate/LZW layer will be handed: a single dictionary
    // reaches every layer, an array hands entry *i* to filter *i*, and a shorter
    // array (or a `null` entry) leaves that layer on its defaults.
    let terminal_params = match dictionary.get(b"DecodeParms") {
        Err(_) | Ok(Object::Null) => None,
        Ok(Object::Dictionary(params)) => Some(params),
        Ok(Object::Array(items)) => {
            if items.len() > MAX_OBJECT_STREAM_FILTERS {
                return None;
            }
            match items.get(prefix.len()) {
                None | Some(Object::Null) => None,
                Some(Object::Dictionary(params)) => Some(params),
                // Anything else — a reference above all — reaches the decoder as
                // "defaults", which is not what the document asked for.
                Some(_) => return None,
            }
        }
        Ok(_) => return None,
    };
    if let Some(params) = terminal_params
        && !limited_decode_parameters_are_reproducible(params)
    {
        return None;
    }
    Some(LimitedObjectStreamEncoding::Decoded)
}

/// Whether `params` names a predictor the bounded path can run within its
/// charged allowance, and states every operand the decoder would otherwise
/// silently default.
fn limited_decode_parameters_are_reproducible(params: &Dictionary) -> bool {
    let predictor = match params.get(b"Predictor") {
        Err(_) | Ok(Object::Null) => 1,
        Ok(Object::Integer(value)) => *value,
        Ok(_) => return false,
    };
    if predictor == 1 {
        return true;
    }
    // TIFF Predictor 2 and the PNG predictors 10-15 are the two families
    // `Stream::decompress_predictor` implements.
    if predictor != 2 && !(10..=15).contains(&predictor) {
        return false;
    }
    // The decoder substitutes its defaults for any operand it cannot read as an
    // integer, so a reference or a real here would decode to the wrong bytes
    // rather than fail.
    let Some(columns) = limited_decode_parameter(params, b"Columns", 1) else {
        return false;
    };
    let Some(colors) = limited_decode_parameter(params, b"Colors", 1) else {
        return false;
    };
    let Some(bits) = limited_decode_parameter(params, b"BitsPerComponent", 8) else {
        return false;
    };
    if columns < 1 || !(1..=32).contains(&colors) || !matches!(bits, 1 | 2 | 4 | 8 | 16) {
        return false;
    }
    columns
        .checked_mul(colors)
        .and_then(|samples| samples.checked_mul(bits))
        .is_some_and(|row_bits| (row_bits + 7) / 8 <= MAX_OBJECT_STREAM_PREDICTOR_ROW_BYTES)
}

fn limited_decode_parameter(params: &Dictionary, key: &[u8], default: i64) -> Option<i64> {
    match params.get(key) {
        Err(_) | Ok(Object::Null) => Some(default),
        Ok(Object::Integer(value)) => Some(*value),
        Ok(_) => None,
    }
}

impl IndexedReader {
    /// Resolve bounded metadata for one ordinary stream without reading its
    /// encoded payload.
    ///
    /// Compressed object-stream members are unavailable through this seam.
    /// Missing or malformed stream lengths and `endstream` framing follow the
    /// same degradation and resource-limit decisions as [`Self::resolve_object`].
    pub fn resolve_stream_descriptor(&self, id: crate::ObjectId) -> IndexedStreamReadResult<IndexedStreamDescriptor> {
        self.ensure_stream_source_len()?;
        if matches!(
            self.index.locations.get(&id.0),
            Some(ObjectLocation64::Compressed { .. })
        ) {
            return Err(IndexedStreamReadError::NotNormalObject { id });
        }
        let mut state = ResolutionState::default();
        let metadata: IndexedStreamReadResult<(FramedStreamMetadata, u64)> = (|| {
            let (body_offset, source_len, parsed) = self.resolve_normal_framed(id)?;
            let metadata_frame_bytes = u64::try_from(parsed.consumed)
                .unwrap_or(u64::MAX)
                .saturating_add(parsed.stream_prefix.unwrap_or(0));
            let metadata = self
                .finish_stream_metadata(
                    id,
                    body_offset,
                    source_len,
                    parsed,
                    &mut state,
                    StreamSpanPolicy::Encoded(self.limits.max_encoded_stream_bytes),
                )
                .map_err(StreamMetadataError::into_stream_error)?;
            Ok((metadata, metadata_frame_bytes))
        })();
        self.ensure_stream_source_len()?;
        let (metadata, metadata_frame_bytes) = metadata?;
        let (dictionary, encoded_start, encoded_length) = match metadata {
            FramedStreamMetadata::Span {
                dictionary,
                encoded_start,
                encoded_len,
            } => (dictionary, encoded_start, EncodedStreamLength::Known(encoded_len)),
            FramedStreamMetadata::MissingLength {
                dictionary,
                encoded_start,
            } => (
                dictionary,
                encoded_start,
                EncodedStreamLength::Unavailable(EncodedStreamLengthUnavailableReason::MissingOrInvalid),
            ),
            FramedStreamMetadata::Scalar(_) => return Err(IndexedStreamReadError::NotStream { id }),
        };
        let protection = self.classify_encoded_stream_protection(&dictionary);
        self.ensure_stream_source_len()?;
        Ok(IndexedStreamDescriptor {
            id,
            dictionary,
            encoded_length,
            protection,
            source: Arc::clone(&self.source),
            source_len: self.index.source_len,
            encoded_start,
            metadata_frame_bytes,
            _metadata_charge: None,
        })
    }

    /// Resolve stream metadata under a call-local simultaneous-allocation
    /// allowance without reading the encoded payload. The returned dictionary
    /// remains charged until the descriptor is dropped.
    pub fn resolve_stream_descriptor_with_permit(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedStreamReadResult<IndexedStreamDescriptor> {
        self.ensure_stream_source_len()?;
        if permit.stats().current_bytes != 0 {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: permit.stats().current_bytes,
                limit: permit.limit_bytes(),
                phase: "permit-not-empty",
            }
            .into());
        }
        if !matches!(self.index.locations.get(&id.0), Some(ObjectLocation64::Normal { .. })) {
            return Err(IndexedStreamReadError::NotNormalObject { id });
        }

        let (body_offset, source_len, parsed, mut dictionary_charge) = self.parse_normal_at_limited(id, permit)?;
        let metadata_frame_bytes = u64::try_from(parsed.consumed)
            .unwrap_or(u64::MAX)
            .saturating_add(parsed.stream_prefix.unwrap_or(0));
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Err(IndexedStreamReadError::NotStream { id });
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            }
            .into());
        };
        let encoded_start = body_offset
            .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?;

        let dictionary_bytes = u64::try_from(dictionary_retained_bytes(&dictionary)).unwrap_or(u64::MAX);
        if dictionary_bytes > dictionary_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: dictionary_bytes,
                limit: dictionary_charge.bytes(),
                phase: "measured-stream-descriptor-dictionary",
            }
            .into());
        }
        dictionary_charge.shrink_to(dictionary_bytes);

        let mut length_state = ResolutionState::default();
        let length = self.resolve_stream_length_limited(&dictionary, permit, &mut length_state)?;
        let encoded_length = match length {
            None => EncodedStreamLength::Unavailable(EncodedStreamLengthUnavailableReason::MissingOrInvalid),
            Some(length) if length < 0 => {
                return Err(IndexedReaderError::NegativeStreamLength { id, length }.into());
            }
            Some(length) => {
                let length =
                    u64::try_from(length).map_err(|_| IndexedReaderError::NegativeStreamLength { id, length })?;
                let Some(encoded_end) = encoded_start.checked_add(length).filter(|end| *end <= source_len) else {
                    return Err(IndexedStreamReadError::NotStream { id });
                };
                if let Some(limit) = self.limits.max_encoded_stream_bytes
                    && length > limit
                {
                    return Err(IndexedStreamReadError::EncodedStreamLimitExceeded { id, length, limit });
                }
                let tail_bytes = self
                    .limits
                    .max_endstream_tail_bytes
                    .min(source_len.saturating_sub(encoded_end));
                let tail_charge = permit.reserve(id, tail_bytes, "stream-descriptor-end-marker")?;
                let status = validate_endstream(
                    self.source.as_ref(),
                    source_len,
                    encoded_end,
                    self.limits.max_endstream_tail_bytes,
                )?;
                drop(tail_charge);
                match status {
                    EndstreamStatus::Found => EncodedStreamLength::Known(length),
                    EndstreamStatus::Missing => return Err(IndexedStreamReadError::NotStream { id }),
                    EndstreamStatus::LimitExceeded => {
                        return Err(IndexedReaderError::MissingEndstream { id }.into());
                    }
                }
            }
        };
        let protection = self.classify_encoded_stream_protection_limited(&dictionary, permit)?;
        self.ensure_stream_source_len()?;
        Ok(IndexedStreamDescriptor {
            id,
            dictionary,
            encoded_length,
            protection,
            source: Arc::clone(&self.source),
            source_len: self.index.source_len,
            encoded_start,
            metadata_frame_bytes,
            _metadata_charge: Some(dictionary_charge),
        })
    }

    fn ensure_stream_source_len(&self) -> IndexedStreamReadResult<()> {
        let actual = self.source.len()?;
        if actual != self.index.source_len {
            return Err(IndexedStreamReadError::SourceLengthChanged {
                expected: self.index.source_len,
                actual,
            });
        }
        Ok(())
    }

    fn classify_encoded_stream_protection(&self, dictionary: &Dictionary) -> EncodedStreamProtection {
        if self.index.encryption_state.is_some() {
            return EncodedStreamProtection::DocumentEncrypted;
        }
        let Ok(filter) = dictionary.get(b"Filter") else {
            return EncodedStreamProtection::Plain;
        };
        let mut state = ResolutionState::default();
        match self.classify_filter_value(filter.clone(), &mut state) {
            Ok(FilterProtection::Plain) => EncodedStreamProtection::Plain,
            Ok(FilterProtection::Crypt) => EncodedStreamProtection::CryptFilter,
            Err(()) => EncodedStreamProtection::UnresolvedFilter,
        }
    }

    fn classify_encoded_stream_protection_limited(
        &self, dictionary: &Dictionary, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<EncodedStreamProtection> {
        if self.index.encryption_state.is_some() {
            return Ok(EncodedStreamProtection::DocumentEncrypted);
        }
        let Ok(filter) = dictionary.get(b"Filter") else {
            return Ok(EncodedStreamProtection::Plain);
        };
        let mut state = ResolutionState::default();
        Ok(match self.classify_filter_value_limited(filter, permit, &mut state)? {
            Some(FilterProtection::Plain) => EncodedStreamProtection::Plain,
            Some(FilterProtection::Crypt) => EncodedStreamProtection::CryptFilter,
            None => EncodedStreamProtection::UnresolvedFilter,
        })
    }

    fn classify_filter_value_limited(
        &self, filter: &Object, permit: &ScalarResolutionPermit, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Option<FilterProtection>> {
        match filter {
            Object::Name(name) if name == b"Crypt" => Ok(Some(FilterProtection::Crypt)),
            Object::Name(_) => Ok(Some(FilterProtection::Plain)),
            Object::Array(filters) => {
                let mut protection = FilterProtection::Plain;
                for filter in filters {
                    match self.classify_filter_value_limited(filter, permit, state)? {
                        Some(FilterProtection::Crypt) => protection = FilterProtection::Crypt,
                        Some(FilterProtection::Plain) => {}
                        None => return Ok(None),
                    }
                }
                Ok(Some(protection))
            }
            Object::Reference(id) => {
                let id = *id;
                if state.depth >= self.limits.max_length_depth {
                    return Ok(None);
                }
                if !state.active.insert(id) {
                    return Ok(None);
                }
                state.depth += 1;
                let result = if matches!(self.index.locations.get(&id.0), Some(ObjectLocation64::Normal { .. })) {
                    match self.resolve_normal_scalar_limited(id, permit) {
                        Ok(object) => self.classify_filter_value_limited(object.as_object(), permit, state),
                        Err(error @ IndexedReaderError::ScalarResourceLimit { .. })
                        | Err(error @ IndexedReaderError::ScalarResolutionCancelled { .. })
                        | Err(error @ IndexedReaderError::ScalarResolutionClosed { .. }) => Err(error),
                        Err(_) => Ok(None),
                    }
                } else {
                    Ok(None)
                };
                state.depth -= 1;
                state.active.remove(&id);
                result
            }
            _ => Ok(None),
        }
    }

    fn classify_filter_value(
        &self, filter: Object, state: &mut ResolutionState,
    ) -> std::result::Result<FilterProtection, ()> {
        let filter = self.resolve_filter_value(filter, state).map_err(|_| ())?;
        match filter {
            Object::Name(name) if name == b"Crypt" => Ok(FilterProtection::Crypt),
            Object::Name(_) => Ok(FilterProtection::Plain),
            Object::Array(filters) => {
                let mut protection = FilterProtection::Plain;
                for filter in filters {
                    if self.classify_filter_value(filter, state)? == FilterProtection::Crypt {
                        protection = FilterProtection::Crypt;
                    }
                }
                Ok(protection)
            }
            _ => Err(()),
        }
    }

    fn resolve_filter_value(&self, object: Object, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        let Object::Reference(id) = object else {
            return Ok(object);
        };
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let resolved = match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => {
                self.resolve_normal_framed(id).and_then(|(body_offset, _, parsed)| {
                    if parsed.stream_prefix.is_some() {
                        Err(IndexedReaderError::InvalidIndirectObject {
                            id,
                            offset: body_offset,
                        })
                    } else {
                        Ok(parsed.object)
                    }
                })
            }
            // Descriptor classification runs before a caller has acquired an
            // encoded-stream lease.  A compressed metadata value would require
            // reading and decoding its object-stream payload, so fail closed
            // without touching the container.
            Some(ObjectLocation64::Compressed { .. }) => Err(IndexedReaderError::MissingNormalObject { id }),
            Some(ObjectLocation64::Free { .. }) | None => Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let resolved = resolved.and_then(|object| self.resolve_filter_value(object, state));
        state.depth -= 1;
        state.active.remove(&id);
        resolved
    }
}
