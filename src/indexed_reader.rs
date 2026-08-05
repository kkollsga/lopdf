//! Bounded structural bootstrap for the staged indexed reader.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use thiserror::Error;

use crate::encryption::{self, EncryptionState, PasswordAlgorithm};
use crate::source::{RandomAccessSource, SourceError};
use crate::{Dictionary, Object, ObjectStream, Stream};

const HEADER_SCAN_LIMIT: u64 = 1_024;
const HEADER_PARSE_OVERLAP: u64 = 64;
const TAIL_SCAN_LIMIT: u64 = 64 * 1_024;
const XREF_INITIAL_WINDOW: u64 = 4 * 1_024;
const XREF_WINDOW_LIMIT: u64 = 16 * 1_024 * 1_024;
const XREF_DECOMPRESSED_LIMIT: usize = 32 * 1_024 * 1_024;
const MAX_XREF_ENTRIES: u64 = 1_000_000;
const MAX_XREF_REVISIONS: usize = 1_024;
const MAX_XREF_FIELD_WIDTH: u64 = 8;
const INDIRECT_HEADER_LIMIT: u64 = 256;
const INITIAL_OBJECT_WINDOW: u64 = 4 * 1_024;
const OBJECT_GROWTH_CHUNK: u64 = 64 * 1_024;
const DEFAULT_OBJECT_LIMIT: u64 = 4 * 1_024 * 1_024;
const DEFAULT_STREAM_LIMIT: u64 = 64 * 1_024 * 1_024;
const DEFAULT_ENDSTREAM_TAIL_LIMIT: u64 = 64;
const DEFAULT_LENGTH_DEPTH_LIMIT: usize = 64;

type IndexResult<T> = std::result::Result<T, IndexError>;

#[cfg(test)]
thread_local! {
    static OBJECT_BODY_PARSE_CALLS: Cell<usize> = const { Cell::new(0) };
}

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
    #[error("missing normal xref entry for object {id:?}")]
    MissingNormalObject { id: crate::ObjectId },
    #[error("xref generation {indexed} does not match requested object {id:?}")]
    GenerationMismatch { id: crate::ObjectId, indexed: u16 },
    #[error("indirect-object header at offset {offset} exceeds the {limit}-byte limit")]
    IndirectHeaderLimitExceeded { offset: u64, limit: u64 },
    #[error("indirect-object header mismatch: expected {expected:?}, found {actual:?}")]
    IndirectObjectMismatch {
        expected: crate::ObjectId,
        actual: crate::ObjectId,
    },
    #[error("invalid indirect object {id:?} at offset {offset}")]
    InvalidIndirectObject { id: crate::ObjectId, offset: u64 },
    #[error("incomplete indirect object {id:?} at offset {offset}")]
    IncompleteObject { id: crate::ObjectId, offset: u64 },
    #[error("object {id:?} exceeds the {limit}-byte parser limit")]
    ObjectLimitExceeded { id: crate::ObjectId, limit: u64 },
    #[error("stream in object {id:?} declares {length} bytes, exceeding the {limit}-byte limit")]
    StreamLimitExceeded {
        id: crate::ObjectId,
        length: u64,
        limit: u64,
    },
    #[error("stream in object {id:?} has negative length {length}")]
    NegativeStreamLength { id: crate::ObjectId, length: i64 },
    #[error("stream in object {id:?} has no bounded endstream marker")]
    MissingEndstream { id: crate::ObjectId },
    #[error("object-resolution cycle at {id:?}")]
    ResolutionCycle { id: crate::ObjectId },
    #[error("object-resolution depth exceeds the {limit}-object limit")]
    ResolutionDepthExceeded { limit: usize },
    #[error("object-stream container {container:?} for object {id:?} is not a stream")]
    ObjectStreamContainerNotStream {
        id: crate::ObjectId,
        container: crate::ObjectId,
    },
    #[error("failed to resolve object {id:?} from object-stream container {container:?} at member index {index}")]
    ObjectStreamMember {
        id: crate::ObjectId,
        container: crate::ObjectId,
        index: u32,
        #[source]
        source: crate::Error,
    },
    #[error("encrypted PDF requires a password")]
    PasswordRequired,
    #[error("invalid password for encrypted PDF")]
    InvalidPassword,
    #[error("invalid encryption bootstrap")]
    Encryption(#[source] crate::Error),
    #[error("the trailer /Encrypt value does not resolve to a dictionary")]
    InvalidEncryptDictionary,
    #[error("failed to decrypt object {id:?}")]
    ObjectDecryption {
        id: crate::ObjectId,
        #[source]
        source: crate::encryption::DecryptionError,
    },
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
    pub(crate) encryption_state: Option<EncryptionState>,
    pub(crate) encrypt_object_id: Option<crate::ObjectId>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolverLimits {
    pub(crate) max_object_bytes: u64,
    pub(crate) max_stream_bytes: u64,
    pub(crate) max_endstream_tail_bytes: u64,
    pub(crate) max_length_depth: usize,
}

impl Default for ResolverLimits {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_OBJECT_LIMIT,
            max_stream_bytes: DEFAULT_STREAM_LIMIT,
            max_endstream_tail_bytes: DEFAULT_ENDSTREAM_TAIL_LIMIT,
            max_length_depth: DEFAULT_LENGTH_DEPTH_LIMIT,
        }
    }
}

pub(crate) struct IndexedReader {
    source: Arc<dyn RandomAccessSource>,
    pub(crate) index: PdfIndex,
    limits: ResolverLimits,
}

impl IndexedReader {
    pub(crate) fn open(source: Arc<dyn RandomAccessSource>, limits: ResolverLimits) -> IndexResult<Self> {
        Self::open_with_password(source, limits, None)
    }

    pub(crate) fn open_with_password(
        source: Arc<dyn RandomAccessSource>, limits: ResolverLimits, password: Option<&[u8]>,
    ) -> IndexResult<Self> {
        let index = PdfIndex::open(Arc::clone(&source))?;
        let mut reader = Self { source, index, limits };
        reader.initialize_encryption(password)?;
        Ok(reader)
    }

    pub(crate) fn resolve(&self, id: crate::ObjectId) -> IndexResult<Object> {
        let mut state = ResolutionState::default();
        self.resolve_inner(id, &mut state)
    }

    fn resolve_inner(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexResult<Object> {
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal(id, state),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed(id, container, index, state)
            }
            Some(ObjectLocation64::Free { .. }) | None => Err(IndexError::MissingNormalObject { id }),
        };
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed(
        &self, id: crate::ObjectId, container: u32, index: u32, state: &mut ResolutionState,
    ) -> IndexResult<Object> {
        if id.1 != 0 {
            return Err(IndexError::GenerationMismatch { id, indexed: 0 });
        }
        let container = (container, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(container) {
            return Err(IndexError::ResolutionCycle { id: container });
        }
        state.depth += 1;
        // Object streams must themselves be ordinary, generation-zero indirect
        // objects. Do not recursively accept a compressed container here.
        let resolved = self.resolve_normal(container, state);
        state.depth -= 1;
        state.active.remove(&container);

        let object = resolved?;
        let Object::Stream(stream) = object else {
            return Err(IndexError::ObjectStreamContainerNotStream { id, container });
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        ObjectStream::parse_selected_member_with_limit(&stream, id, index, Some(limit)).map_err(|source| {
            IndexError::ObjectStreamMember {
                id,
                container,
                index,
                source,
            }
        })
    }

    fn resolve_normal(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexResult<Object> {
        let mut object = self.resolve_normal_plain(id, state)?;
        if self.index.encrypt_object_id != Some(id)
            && let Some(encryption_state) = &self.index.encryption_state
        {
            encryption::decrypt_object(encryption_state, id, &mut object)
                .map_err(|source| IndexError::ObjectDecryption { id, source })?;
        }
        Ok(object)
    }

    fn resolve_normal_plain(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexResult<Object> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexError::MissingNormalObject { id }),
        };
        if indexed_generation != id.1 {
            return Err(IndexError::GenerationMismatch {
                id,
                indexed: indexed_generation,
            });
        }

        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header = read_window(self.source.as_ref(), source_len, physical, INDIRECT_HEADER_LIMIT)?;
        let (actual, header_bytes) = parse_indirect_header(&header).ok_or(IndexError::IndirectHeaderLimitExceeded {
            offset,
            limit: INDIRECT_HEADER_LIMIT,
        })?;
        if actual != id {
            return Err(IndexError::IndirectObjectMismatch { expected: id, actual });
        }
        let header_bytes = u64::try_from(header_bytes).map_err(|_| IndexError::InvalidIndirectObject { id, offset })?;
        let body_offset = physical
            .checked_add(header_bytes)
            .ok_or(IndexError::InvalidIndirectObject { id, offset })?;
        self.resolve_body(id, body_offset, source_len, state)
    }

    fn initialize_encryption(&mut self, password: Option<&[u8]>) -> IndexResult<()> {
        let Ok(encrypt) = self.index.trailer.get(b"Encrypt") else {
            return Ok(());
        };
        let (dictionary, encrypt_object_id) = if let Ok(dictionary) = encrypt.as_dict() {
            (dictionary.clone(), None)
        } else if let Ok(id) = encrypt.as_reference() {
            let mut state = ResolutionState::default();
            let object = self.resolve_normal_plain(id, &mut state)?;
            let dictionary = object
                .as_dict()
                .map_err(|_| IndexError::InvalidEncryptDictionary)?
                .clone();
            (dictionary, Some(id))
        } else {
            return Err(IndexError::InvalidEncryptDictionary);
        };
        let file_id = self
            .index
            .trailer
            .get(b"ID")
            .ok()
            .and_then(|id| id.as_array().ok())
            .and_then(|ids| ids.first())
            .and_then(|id| id.as_str().ok());
        let algorithm = PasswordAlgorithm::try_from(&dictionary).map_err(IndexError::Encryption)?;

        let selected = if let Some(selected) = authenticate_password(&algorithm, file_id, b"")? {
            selected
        } else if let Some(password) = password {
            authenticate_password(&algorithm, file_id, password)?.ok_or(IndexError::InvalidPassword)?
        } else {
            return Err(IndexError::PasswordRequired);
        };
        let state =
            EncryptionState::decode_from_dictionary(&dictionary, file_id, &selected).map_err(IndexError::Encryption)?;
        self.index.encryption_state = Some(state);
        self.index.encrypt_object_id = encrypt_object_id;
        Ok(())
    }

    fn resolve_body(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, state: &mut ResolutionState,
    ) -> IndexResult<Object> {
        let remaining = source_len.checked_sub(body_offset).ok_or(SourceError::OutOfBounds {
            offset: body_offset,
            length: 0,
            source_len,
        })?;
        let maximum = remaining.min(self.limits.max_object_bytes);
        let initial_length = maximum.min(INITIAL_OBJECT_WINDOW);
        let mut window = self.source.read_range(body_offset, initial_length, initial_length)?;
        let mut object_framer = DirectObjectFramer::new();
        loop {
            let frame_status = object_framer.advance(&window);
            if frame_status == FrameStatus::Invalid {
                return Err(IndexError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                });
            }
            if frame_status == FrameStatus::Ready {
                let parsed = parse_object_body(&window, id, body_offset)?;
                return self.finish_object(id, body_offset, source_len, parsed, state);
            }

            let current = u64::try_from(window.len()).map_err(|_| IndexError::ObjectLimitExceeded {
                id,
                limit: self.limits.max_object_bytes,
            })?;
            if current >= maximum {
                return if remaining > self.limits.max_object_bytes {
                    Err(IndexError::ObjectLimitExceeded {
                        id,
                        limit: self.limits.max_object_bytes,
                    })
                } else {
                    Err(IndexError::IncompleteObject {
                        id,
                        offset: body_offset,
                    })
                };
            }
            // Extend the retained prefix instead of rereading it. A fixed
            // upper growth step caps speculative stream-payload reads while
            // still doubling small object probes.
            let target = current
                .saturating_mul(2)
                .min(current.saturating_add(OBJECT_GROWTH_CHUNK))
                .min(maximum);
            let extension_length = target - current;
            let extension_offset = body_offset
                .checked_add(current)
                .ok_or(IndexError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                })?;
            let extension = self
                .source
                .read_range(extension_offset, extension_length, extension_length)?;
            window.extend_from_slice(&extension);
        }
    }

    fn finish_object(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, parsed: ParsedObject,
        state: &mut ResolutionState,
    ) -> IndexResult<Object> {
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Ok(object);
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexError::InvalidIndirectObject {
                id,
                offset: body_offset,
            });
        };

        let stream_start = body_offset
            .checked_add(u64::try_from(consumed).map_err(|_| IndexError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?)
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?;
        let Some(length) = self.resolve_stream_length(&dictionary, state)? else {
            // This matches the eager loader's degradation for a missing,
            // malformed, dangling, cyclic, or over-depth /Length: retain the
            // stream dictionary and expose empty owned content.
            // Stream positions in eager `Document` objects are relative to the
            // PDF header, even when transport junk precedes it. The indexed
            // source offsets are physical, so rebase before exposing parity.
            let relative_stream_start =
                stream_start
                    .checked_sub(self.index.source_origin)
                    .ok_or(IndexError::InvalidIndirectObject {
                        id,
                        offset: stream_start,
                    })?;
            let stream_start =
                usize::try_from(relative_stream_start).map_err(|_| IndexError::InvalidIndirectObject {
                    id,
                    offset: stream_start,
                })?;
            return Ok(Object::Stream(Stream::with_position(dictionary, stream_start)));
        };
        if length < 0 {
            return Err(IndexError::NegativeStreamLength { id, length });
        }
        let length = u64::try_from(length).map_err(|_| IndexError::NegativeStreamLength { id, length })?;
        if length > self.limits.max_stream_bytes {
            return Err(IndexError::StreamLimitExceeded {
                id,
                length,
                limit: self.limits.max_stream_bytes,
            });
        }

        let stream_end = stream_start
            .checked_add(length)
            .ok_or(IndexError::InvalidIndirectObject {
                id,
                offset: stream_start,
            })?;
        if stream_end > source_len {
            return Ok(Object::Dictionary(dictionary));
        }
        match validate_endstream(
            self.source.as_ref(),
            source_len,
            stream_end,
            self.limits.max_endstream_tail_bytes,
        )? {
            EndstreamStatus::Found => {}
            EndstreamStatus::Missing => return Ok(Object::Dictionary(dictionary)),
            EndstreamStatus::LimitExceeded => return Err(IndexError::MissingEndstream { id }),
        }
        let content = self
            .source
            .read_range(stream_start, length, self.limits.max_stream_bytes)?;
        Ok(Object::Stream(Stream::new(dictionary, content)))
    }

    fn resolve_stream_length(&self, dictionary: &Dictionary, state: &mut ResolutionState) -> IndexResult<Option<i64>> {
        let Ok(length) = dictionary.get(b"Length") else {
            return Ok(None);
        };
        if let Ok(value) = length.as_i64() {
            return Ok(Some(value));
        }
        let Ok(reference) = length.as_reference() else {
            return Ok(None);
        };
        // Eager loading treats every failure to dereference /Length as a
        // missing length and retains an empty stream. Keep that degradation,
        // while the bounded state guarantees cycles and deep chains terminate.
        Ok(self.resolve_length_reference(reference, state).ok())
    }

    fn resolve_length_reference(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexResult<i64> {
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = self.resolve_normal(id, state).and_then(|object| match object {
            Object::Integer(value) => Ok(value),
            Object::Reference(next) => self.resolve_length_reference(next, state),
            _ => Err(IndexError::InvalidIndirectObject { id, offset: 0 }),
        });
        state.depth -= 1;
        state.active.remove(&id);
        result
    }
}

#[derive(Default)]
struct ResolutionState {
    active: HashSet<crate::ObjectId>,
    depth: usize,
}

fn authenticate_password(
    algorithm: &PasswordAlgorithm, file_id: Option<&[u8]>, password: &[u8],
) -> IndexResult<Option<Vec<u8>>> {
    let user = algorithm.authenticate_user_password_with_file_id(file_id, password);
    if user.is_ok() {
        return Ok(Some(password.to_vec()));
    }
    let owner = if (2..=4).contains(&algorithm.revision) {
        algorithm.recover_user_password_with_file_id(file_id, password)
    } else {
        algorithm
            .authenticate_owner_password_with_file_id(file_id, password)
            .map(|()| password.to_vec())
    };
    match owner {
        Ok(password) => Ok(Some(password)),
        Err(owner_error) => match user {
            Err(crate::encryption::DecryptionError::IncorrectPassword)
                if matches!(owner_error, crate::encryption::DecryptionError::IncorrectPassword) =>
            {
                Ok(None)
            }
            Err(crate::encryption::DecryptionError::IncorrectPassword) => {
                Err(IndexError::Encryption(crate::Error::Decryption(owner_error)))
            }
            Err(user_error) => Err(IndexError::Encryption(crate::Error::Decryption(user_error))),
            Ok(()) => unreachable!(),
        },
    }
}

struct ParsedObject {
    object: Object,
    consumed: usize,
    stream_prefix: Option<u64>,
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
            encryption_state: None,
            encrypt_object_id: None,
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

fn read_window(source: &dyn RandomAccessSource, source_len: u64, offset: u64, limit: u64) -> IndexResult<Vec<u8>> {
    let remaining = source_len.checked_sub(offset).ok_or(SourceError::OutOfBounds {
        offset,
        length: 0,
        source_len,
    })?;
    let length = remaining.min(limit);
    Ok(source.read_range(offset, length, limit)?)
}

fn parse_indirect_header(input: &[u8]) -> Option<(crate::ObjectId, usize)> {
    let original_len = input.len();
    let mut cursor = TokenCursor::new(input);
    let number = u32::try_from(cursor.unsigned()?).ok()?;
    let generation = u16::try_from(cursor.unsigned()?).ok()?;
    cursor.expect(b"obj")?;
    cursor.skip_space();
    Some(((number, generation), original_len - cursor.remaining().len()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameStatus {
    NeedMore,
    Ready,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameLex {
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
enum FrameResume {
    Normal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameKeyword {
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
struct DirectObjectFramer {
    position: usize,
    containers: [u8; crate::reader::MAX_NESTING_DEPTH],
    container_depth: usize,
    lex: FrameLex,
    status: FrameStatus,
    scanned_work: usize,
}

impl DirectObjectFramer {
    fn new() -> Self {
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
    fn for_dictionary(input: &[u8]) -> Option<Self> {
        if !input.starts_with(b"<<") {
            return None;
        }
        let mut framer = Self::new();
        let _ = framer.advance(input);
        Some(framer)
    }

    fn advance(&mut self, input: &[u8]) -> FrameStatus {
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
                // reference probe immediately so replay is fixed-size.
                self.fallback_integer(scalar_completion);
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

    fn fallback_two_integers(&mut self, scalar_completion: usize, second_completion: usize) {
        let top_level = self.container_depth == 0;
        self.position = scalar_completion;
        self.finish_value(false);
        if top_level || self.status != FrameStatus::NeedMore {
            return;
        }
        self.position = second_completion;
        self.finish_value(false);
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

fn parse_object_body(input: &[u8], id: crate::ObjectId, offset: u64) -> IndexResult<ParsedObject> {
    #[cfg(test)]
    OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let Some((consumed, object)) = crate::parser::direct_object_with_consumed(input) else {
        return if direct_object_may_be_truncated(input) {
            Err(IndexError::IncompleteObject { id, offset })
        } else {
            Err(IndexError::InvalidIndirectObject { id, offset })
        };
    };
    if !matches!(object, Object::Dictionary(_)) {
        let remaining = input
            .get(consumed..)
            .ok_or(IndexError::InvalidIndirectObject { id, offset })?;
        if remaining.is_empty() || integer_reference_may_be_truncated(&object, remaining) {
            return Err(IndexError::IncompleteObject { id, offset });
        }
        return Ok(ParsedObject {
            object,
            consumed,
            stream_prefix: None,
        });
    }

    let remaining = input
        .get(consumed..)
        .ok_or(IndexError::InvalidIndirectObject { id, offset })?;
    let mut cursor = TokenCursor::new(remaining);
    cursor.skip_space();
    if cursor.remaining().is_empty()
        || (cursor.remaining().len() < b"stream".len() && b"stream".starts_with(cursor.remaining()))
    {
        return Err(IndexError::IncompleteObject { id, offset });
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
            Err(IndexError::IncompleteObject { id, offset })
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
        stream_prefix: Some(u64::try_from(prefix).map_err(|_| IndexError::InvalidIndirectObject { id, offset })?),
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
enum EndstreamStatus {
    Found,
    Missing,
    LimitExceeded,
}

fn validate_endstream(
    source: &dyn RandomAccessSource, source_len: u64, offset: u64, limit: u64,
) -> IndexResult<EndstreamStatus> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
    use crate::source::BytesSource;
    use crate::writer::Writer;
    use crate::xref::XrefEntry;
    use crate::{Document, EncryptionState, EncryptionVersion, Permissions, StringFormat};
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    use std::sync::Mutex;

    type ClassicEntry = (u64, u16, bool);
    type ClassicSection = (u32, Vec<ClassicEntry>);

    struct ObjectDef<'a> {
        id: u32,
        object_generation: u16,
        xref_generation: u16,
        body: &'a [u8],
    }

    fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> u64 {
        push_object_with_generation(pdf, id, 0, body)
    }

    fn push_object_with_generation(pdf: &mut Vec<u8>, id: u32, generation: u16, body: &[u8]) -> u64 {
        let offset = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(format!("{id} {generation} obj\n").as_bytes());
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

    fn object_pdf(definitions: &[ObjectDef<'_>]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let max_id = definitions.iter().map(|definition| definition.id).max().unwrap_or(0);
        let mut entries = vec![(0, 65535, false); usize::try_from(max_id).unwrap() + 1];
        for definition in definitions {
            let offset =
                push_object_with_generation(&mut pdf, definition.id, definition.object_generation, definition.body);
            entries[usize::try_from(definition.id).unwrap()] = (offset, definition.xref_generation, true);
        }
        append_classic(
            &mut pdf,
            &[(0, entries)],
            &format!("<< /Size {} /Root 1 0 R >>", u64::from(max_id) + 1),
        );
        pdf
    }

    fn encrypted_pdf(revision: u8, owner: &str, user: &str) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::String(b"encrypted string".to_vec(), StringFormat::Literal),
        );
        document.objects.insert(
            (2, 0),
            Object::Stream(Stream::new(dictionary! {}, b"encrypted stream".to_vec())),
        );
        document.objects.insert(
            (3, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Sentinel" => Object::Reference((1, 0)) }),
        );
        document.max_id = 3;
        document.trailer.set("Root", Object::Reference((3, 0)));
        let id = vec![0x42; 16];
        document.trailer.set(
            "ID",
            Object::Array(vec![
                Object::String(id.clone(), StringFormat::Literal),
                Object::String(id, StringFormat::Literal),
            ]),
        );

        let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        let aes256: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
        let file_key = [0x5a; 32];
        let state = match revision {
            2 => EncryptionState::try_from(EncryptionVersion::V1 {
                document: &document,
                owner_password: owner,
                user_password: user,
                permissions: Permissions::PRINTABLE,
            }),
            3 => EncryptionState::try_from(EncryptionVersion::V2 {
                document: &document,
                owner_password: owner,
                user_password: user,
                key_length: 128,
                permissions: Permissions::PRINTABLE,
            }),
            4 => EncryptionState::try_from(EncryptionVersion::V4 {
                document: &document,
                encrypt_metadata: true,
                crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
                stream_filter: b"StdCF".to_vec(),
                string_filter: b"StdCF".to_vec(),
                owner_password: owner,
                user_password: user,
                permissions: Permissions::PRINTABLE,
            }),
            #[allow(deprecated)]
            5 => EncryptionState::try_from(EncryptionVersion::R5 {
                encrypt_metadata: true,
                crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256.clone())]),
                file_encryption_key: &file_key,
                stream_filter: b"StdCF".to_vec(),
                string_filter: b"StdCF".to_vec(),
                owner_password: owner,
                user_password: user,
                permissions: Permissions::PRINTABLE,
            }),
            6 => EncryptionState::try_from(EncryptionVersion::V5 {
                encrypt_metadata: true,
                crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256)]),
                file_encryption_key: &file_key,
                stream_filter: b"StdCF".to_vec(),
                string_filter: b"StdCF".to_vec(),
                owner_password: owner,
                user_password: user,
                permissions: Permissions::PRINTABLE,
            }),
            _ => unreachable!(),
        }
        .unwrap();
        document.encrypt(&state).unwrap();
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn inline_encrypt_dictionary(mut pdf: Vec<u8>) -> Vec<u8> {
        let trailer_encrypt = rfind(&pdf, b"/Encrypt ").unwrap() + b"/Encrypt ".len();
        let mut cursor = TokenCursor::new(&pdf[trailer_encrypt..]);
        let id = u32::try_from(cursor.unsigned().unwrap()).unwrap();
        let generation = u16::try_from(cursor.unsigned().unwrap()).unwrap();
        cursor.expect(b"R").unwrap();
        let reference_len = pdf[trailer_encrypt..].len() - cursor.remaining().len();
        let header = format!("{id} {generation} obj\n");
        let start = pdf
            .windows(header.len())
            .position(|window| window == header.as_bytes())
            .unwrap()
            + header.len();
        let end = start
            + pdf[start..]
                .windows(b"\nendobj".len())
                .position(|w| w == b"\nendobj")
                .unwrap();
        let dictionary = pdf[start..end].to_vec();
        pdf.splice(trailer_encrypt..trailer_encrypt + reference_len, dictionary);
        pdf
    }

    fn encrypted_object_stream_pdf() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        const CONTAINER_ID: u32 = 5;
        const IMAGE_ID: u32 = 20;
        const STRING_ID: u32 = 21;
        const ENCRYPT_ID: u32 = 30;
        const XREF_ID: u32 = 31;

        let mut document = Document::with_version("1.7");
        let file_id = vec![0x24; 16];
        document.trailer.set(
            "ID",
            Object::Array(vec![
                Object::String(file_id.clone(), StringFormat::Literal),
                Object::String(file_id.clone(), StringFormat::Literal),
            ]),
        );
        let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        let state = EncryptionState::try_from(EncryptionVersion::V4 {
            document: &document,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "user",
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();

        let (first, content) = object_stream_content(&[(
            10,
            b"<< /Type /Catalog /Text (member secret) /Image 20 0 R >>".as_slice(),
        )]);
        let mut container = Object::Stream(Stream::new(
            Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"ObjStm".to_vec())),
                (b"N".to_vec(), Object::Integer(1)),
                (b"First".to_vec(), Object::Integer(i64::try_from(first).unwrap())),
            ]),
            content,
        ));
        encryption::encrypt_object(&state, (CONTAINER_ID, 0), &mut container).unwrap();

        let image_plaintext = b"shared encrypted image".to_vec();
        let mut image = Object::Stream(Stream::new(
            Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
                (b"Subtype".to_vec(), Object::Name(b"Image".to_vec())),
            ]),
            image_plaintext.clone(),
        ));
        encryption::encrypt_object(&state, (IMAGE_ID, 0), &mut image).unwrap();
        let mut string = Object::String(b"normal secret".to_vec(), StringFormat::Literal);
        encryption::encrypt_object(&state, (STRING_ID, 0), &mut string).unwrap();
        let encrypt = Object::Dictionary(state.encode().unwrap());

        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = BTreeMap::new();
        for (id, object) in [
            (CONTAINER_ID, container),
            (IMAGE_ID, image),
            (STRING_ID, string),
            (ENCRYPT_ID, encrypt),
        ] {
            let offset = u64::try_from(pdf.len()).unwrap();
            pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
            Writer::write_object(&mut pdf, &object).unwrap();
            pdf.extend_from_slice(b"\nendobj\n");
            offsets.insert(id, offset);
        }

        let xref_offset = u64::try_from(pdf.len()).unwrap();
        let mut xref_content = Vec::new();
        for id in 0..=XREF_ID {
            if id == 10 {
                encode_field(2, 1, &mut xref_content);
                encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            } else if id == XREF_ID {
                encode_field(1, 1, &mut xref_content);
                encode_field(xref_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            } else if let Some(offset) = offsets.get(&id) {
                encode_field(1, 1, &mut xref_content);
                encode_field(*offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            } else {
                encode_field(0, 1, &mut xref_content);
                encode_field(0, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
        }
        let xref = Object::Stream(Stream::new(
            Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"XRef".to_vec())),
                (b"Size".to_vec(), Object::Integer(i64::from(XREF_ID + 1))),
                (b"Root".to_vec(), Object::Reference((10, 0))),
                (b"Encrypt".to_vec(), Object::Reference((ENCRYPT_ID, 0))),
                (
                    b"ID".to_vec(),
                    Object::Array(vec![
                        Object::String(file_id.clone(), StringFormat::Literal),
                        Object::String(file_id, StringFormat::Literal),
                    ]),
                ),
                (
                    b"W".to_vec(),
                    Object::Array(vec![Object::Integer(1), Object::Integer(8), Object::Integer(4)]),
                ),
            ]),
            xref_content.clone(),
        ));
        pdf.extend_from_slice(format!("{XREF_ID} 0 obj\n").as_bytes());
        Writer::write_object(&mut pdf, &xref).unwrap();
        pdf.extend_from_slice(format!("\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
        (pdf, image_plaintext, xref_content)
    }

    fn open_encrypted(pdf: &[u8], password: Option<&[u8]>) -> IndexResult<IndexedReader> {
        IndexedReader::open_with_password(
            Arc::new(BytesSource::from(pdf.to_vec())),
            ResolverLimits::default(),
            password,
        )
    }

    fn assert_encrypted_fixture_plaintext(reader: &IndexedReader) {
        assert_eq!(
            reader.resolve((1, 0)).unwrap(),
            Object::String(b"encrypted string".to_vec(), StringFormat::Literal)
        );
        assert_eq!(
            reader.resolve((2, 0)).unwrap().as_stream().unwrap().content,
            b"encrypted stream"
        );
        assert_eq!(
            reader
                .resolve((3, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Sentinel")
                .unwrap(),
            &Object::Reference((1, 0))
        );
    }

    fn open_reader(pdf: &[u8], limits: ResolverLimits) -> IndexedReader {
        IndexedReader::open(Arc::new(BytesSource::from(pdf.to_vec())), limits).unwrap()
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

    struct ObjectStreamFixture {
        pdf: Vec<u8>,
        container_stream_start: u64,
        container_stream_length: u64,
    }

    fn object_stream_content(members: &[(u32, &[u8])]) -> (usize, Vec<u8>) {
        let mut header = Vec::new();
        let mut bodies = Vec::new();
        for (id, body) in members {
            header.extend_from_slice(format!("{id} {} ", bodies.len()).as_bytes());
            bodies.extend_from_slice(body);
            bodies.push(b'\n');
        }
        let first = header.len();
        header.extend_from_slice(&bodies);
        (first, header)
    }

    fn object_stream_fixture(
        dictionary: &str, content: &[u8], compressed_entries: &[(u32, u32)],
    ) -> ObjectStreamFixture {
        const CONTAINER_ID: u32 = 5;
        const XREF_ID: u32 = 6;

        let mut pdf = b"%PDF-1.7\n".to_vec();
        let container_offset = u64::try_from(pdf.len()).unwrap();
        let prefix = format!(
            "{CONTAINER_ID} 0 obj\n<< {dictionary} /Length {} >>\nstream\n",
            content.len()
        );
        pdf.extend_from_slice(prefix.as_bytes());
        let container_stream_start = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(content);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_offset = u64::try_from(pdf.len()).unwrap();
        let size = compressed_entries
            .iter()
            .map(|(id, _)| *id)
            .max()
            .unwrap_or(XREF_ID)
            .max(XREF_ID)
            + 1;
        let mut xref_content = Vec::new();
        for id in 0..size {
            if id == CONTAINER_ID {
                encode_field(1, 1, &mut xref_content);
                encode_field(container_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            } else if id == XREF_ID {
                encode_field(1, 1, &mut xref_content);
                encode_field(xref_offset, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            } else if let Some((_, index)) = compressed_entries.iter().find(|(target, _)| *target == id) {
                encode_field(2, 1, &mut xref_content);
                encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
                encode_field(u64::from(*index), 4, &mut xref_content);
            } else {
                encode_field(0, 1, &mut xref_content);
                encode_field(0, 8, &mut xref_content);
                encode_field(0, 4, &mut xref_content);
            }
        }
        let root = compressed_entries.first().map(|(id, _)| *id).unwrap_or(CONTAINER_ID);
        pdf.extend_from_slice(
            format!(
                "{XREF_ID} 0 obj\n<< /Type /XRef /Size {size} /Root {root} 0 R /W [1 8 4] /Length {} >>\nstream\n",
                xref_content.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&xref_content);
        pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());

        ObjectStreamFixture {
            pdf,
            container_stream_start,
            container_stream_length: u64::try_from(content.len()).unwrap(),
        }
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
    fn encrypted_revisions_accept_user_and_owner_passwords_and_reject_missing_or_wrong() {
        for revision in 2..=6 {
            let pdf = encrypted_pdf(revision, "owner", "user");

            assert!(matches!(open_encrypted(&pdf, None), Err(IndexError::PasswordRequired)));
            let wrong_a = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
            let wrong_b = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
            assert!(matches!(wrong_a, IndexError::InvalidPassword));
            assert_eq!(format!("{wrong_a:?}"), format!("{wrong_b:?}"));

            let user = open_encrypted(&pdf, Some(b"user")).unwrap();
            assert_encrypted_fixture_plaintext(&user);
            let eager = Document::load_mem_with_options(&pdf, crate::LoadOptions::with_password("user")).unwrap();
            for id in [(1, 0), (2, 0), (3, 0)] {
                assert_eq!(user.resolve(id).unwrap(), eager.get_object(id).unwrap().clone());
            }

            let encrypt_id = user.index.encrypt_object_id.unwrap();
            assert_eq!(
                user.resolve(encrypt_id)
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get(b"Filter")
                    .unwrap()
                    .as_name()
                    .unwrap(),
                b"Standard"
            );

            let owner = open_encrypted(&pdf, Some(b"owner")).unwrap();
            assert_encrypted_fixture_plaintext(&owner);
        }
    }

    #[test]
    fn empty_user_password_and_inline_encrypt_dictionary_open_without_materializing_document() {
        for revision in 2..=6 {
            let pdf = encrypted_pdf(revision, "owner", "");
            let reader = open_encrypted(&pdf, None).unwrap();
            assert_encrypted_fixture_plaintext(&reader);
        }

        let pdf = inline_encrypt_dictionary(encrypted_pdf(3, "owner", ""));
        let reader = open_encrypted(&pdf, None).unwrap();
        assert!(reader.index.encryption_state.is_some());
        assert_eq!(reader.index.encrypt_object_id, None);
        assert_encrypted_fixture_plaintext(&reader);
    }

    #[test]
    fn encrypted_object_stream_container_is_decrypted_once_and_xref_stays_plain() {
        let (pdf, image_plaintext, xref_plaintext) = encrypted_object_stream_pdf();
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();

        let member = reader.resolve((10, 0)).unwrap();
        let member = member.as_dict().unwrap();
        assert_eq!(
            member.get(b"Text").unwrap(),
            &Object::String(b"member secret".to_vec(), StringFormat::Literal)
        );
        assert_eq!(member.get(b"Image").unwrap(), &Object::Reference((20, 0)));

        for _ in 0..2 {
            assert_eq!(
                reader.resolve((20, 0)).unwrap().as_stream().unwrap().content,
                image_plaintext
            );
        }
        assert_eq!(
            reader.resolve((21, 0)).unwrap(),
            Object::String(b"normal secret".to_vec(), StringFormat::Literal)
        );
        assert_eq!(
            reader.resolve((31, 0)).unwrap().as_stream().unwrap().content,
            xref_plaintext
        );
        assert_eq!(
            reader
                .resolve((30, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Filter")
                .unwrap()
                .as_name()
                .unwrap(),
            b"Standard"
        );
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

    #[test]
    fn normal_and_nested_objects_are_owned_and_match_eager() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Nested << /Values [1 (two) << /Flag true >>] >> >>",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"[1 2 (three) << /Name /owned >>]",
            },
        ]);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let eager = Document::load_mem(&pdf).unwrap();

        for id in [(1, 0), (2, 0)] {
            assert_eq!(
                format!("{:?}", reader.resolve(id).unwrap()),
                format!("{:?}", eager.get_object(id).unwrap())
            );
        }
    }

    #[test]
    fn declared_compressed_objects_match_eager_values() {
        let members = [
            (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
            (11, b"<< /Type /Pages /Count 0 /Kids [] >>".as_slice()),
            (12, b"[1 (two) << /Flag true >>]".as_slice()),
        ];
        let (first, plain) = object_stream_content(&members);
        for compressed in [false, true] {
            let (content, filter) = if compressed {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
                encoder.write_all(&plain).unwrap();
                (encoder.finish().unwrap(), " /Filter /FlateDecode")
            } else {
                (plain.clone(), "")
            };
            let fixture = object_stream_fixture(
                &format!("/Type /ObjStm /N {} /First {first}{filter}", members.len()),
                &content,
                &[(10, 0), (11, 1), (12, 2)],
            );
            let reader = open_reader(&fixture.pdf, ResolverLimits::default());
            let eager = Document::load_mem(&fixture.pdf).unwrap();

            for id in [(10, 0), (11, 0), (12, 0)] {
                assert_eq!(reader.resolve(id).unwrap(), eager.get_object(id).unwrap().clone());
            }
            assert_eq!(reader.resolve((999, 0)).is_err(), eager.get_object((999, 0)).is_err());
        }
    }

    #[test]
    fn compressed_member_enforces_container_index_id_generation_and_shape() {
        let (first, content) = object_stream_content(&[(10, b"(ten)"), (11, b"(eleven)")]);
        let wrong_index = object_stream_fixture(&format!("/Type /ObjStm /N 2 /First {first}"), &content, &[(10, 1)]);
        let reader = open_reader(&wrong_index.pdf, ResolverLimits::default());
        assert!(matches!(
            reader.resolve((10, 0)),
            Err(IndexError::ObjectStreamMember {
                id: (10, 0),
                container: (5, 0),
                index: 1,
                ..
            })
        ));
        assert!(matches!(
            reader.resolve((10, 1)),
            Err(IndexError::GenerationMismatch {
                id: (10, 1),
                indexed: 0
            })
        ));

        let ordinary = object_pdf(&[ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Not /AStream >>",
        }]);
        let mut reader = open_reader(&ordinary, ResolverLimits::default());
        reader
            .index
            .locations
            .insert(10, ObjectLocation64::Compressed { container: 5, index: 0 });
        assert!(matches!(
            reader.resolve((10, 0)),
            Err(IndexError::ObjectStreamContainerNotStream {
                id: (10, 0),
                container: (5, 0)
            })
        ));
    }

    #[test]
    fn malformed_compressed_members_match_eager_and_resource_bounds_still_fail() {
        for n in [2, -1] {
            let fixture = object_stream_fixture(&format!("/Type /ObjStm /N {n} /First 5"), b"10 0 (ten)", &[(10, 0)]);
            let eager = Document::load_mem(&fixture.pdf).unwrap();
            assert_eq!(
                open_reader(&fixture.pdf, ResolverLimits::default())
                    .resolve((10, 0))
                    .unwrap(),
                eager.get_object((10, 0)).unwrap().clone()
            );
        }

        let equal_offsets = object_stream_fixture(
            "/Type /ObjStm /N 2 /First 10",
            b"10 0 11 0 (shared)",
            &[(10, 0), (11, 1)],
        );
        let eager = Document::load_mem(&equal_offsets.pdf).unwrap();
        let reader = open_reader(&equal_offsets.pdf, ResolverLimits::default());
        for id in [(10, 0), (11, 0)] {
            assert_eq!(reader.resolve(id).unwrap(), eager.get_object(id).unwrap().clone());
        }

        let malformed = [
            ("/Type /ObjStm /N 1 /First 1", b"10 0 (ten)".as_slice()),
            ("/Type /ObjStm /N 1 /First 5", b"10 0 << /Broken".as_slice()),
        ];
        for (dictionary, content) in malformed {
            let fixture = object_stream_fixture(dictionary, content, &[(10, 0)]);
            assert!(matches!(
                open_reader(&fixture.pdf, ResolverLimits::default()).resolve((10, 0)),
                Err(IndexError::ObjectStreamMember { .. })
            ));
        }

        let large = format!("({})", "x".repeat(16 * 1_024));
        let (first, decoded) = object_stream_content(&[(10, large.as_bytes())]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < 1_024);
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
            &compressed,
            &[(10, 0)],
        );
        let reader = open_reader(
            &fixture.pdf,
            ResolverLimits {
                max_stream_bytes: 1_024,
                ..ResolverLimits::default()
            },
        );
        assert!(matches!(
            reader.resolve((10, 0)),
            Err(IndexError::ObjectStreamMember { .. })
        ));
    }

    #[test]
    fn fw9_style_compressed_fingerprint_matches_eager() {
        let bodies: Vec<_> = (10..110)
            .map(|id| format!("<< /T (field-{id}) /V ({}) /Rect [0 0 100 20] >>", id * 17))
            .collect();
        let members: Vec<_> = bodies
            .iter()
            .enumerate()
            .map(|(index, body)| (u32::try_from(index).unwrap() + 10, body.as_bytes()))
            .collect();
        let (first, plain) = object_stream_content(&members);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&plain).unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 100 /First {first} /Filter /FlateDecode"),
            &encoder.finish().unwrap(),
            &(10..110).map(|id| (id, id - 10)).collect::<Vec<_>>(),
        );
        let reader = open_reader(&fixture.pdf, ResolverLimits::default());
        let eager = Document::load_mem(&fixture.pdf).unwrap();
        for id in 10..110 {
            assert_eq!(
                reader.resolve((id, 0)).unwrap(),
                eager.get_object((id, 0)).unwrap().clone()
            );
        }
    }

    #[test]
    fn degraded_stream_position_is_relative_to_pdf_origin() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /MissingLength true >>\nstream\nignored\nendstream",
        }]);
        let prefix = b"twenty-nine-byte-prefix.....\n";
        assert_eq!(prefix.len(), 29);
        let mut prefixed = prefix.to_vec();
        prefixed.extend_from_slice(&pdf);

        let resolved = open_reader(&prefixed, ResolverLimits::default())
            .resolve((1, 0))
            .unwrap();
        let eager = Document::load_mem(&prefixed)
            .unwrap()
            .get_object((1, 0))
            .unwrap()
            .clone();

        assert_eq!(resolved, eager);
        assert!(resolved.as_stream().unwrap().content.is_empty());
        let physical_stream_start = prefixed
            .windows(b"stream\n".len())
            .position(|window| window == b"stream\n")
            .unwrap()
            + b"stream\n".len();
        assert_eq!(
            resolved.as_stream().unwrap().start_position.unwrap() + prefix.len(),
            physical_stream_start
        );
    }

    #[test]
    fn xref_and_indirect_header_generations_are_both_validated() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 1,
            body: b"(generation)",
        }]);
        let reader = open_reader(&pdf, ResolverLimits::default());

        assert!(matches!(
            reader.resolve((1, 0)),
            Err(IndexError::GenerationMismatch { id: (1, 0), indexed: 1 })
        ));
        assert!(matches!(
            reader.resolve((1, 1)),
            Err(IndexError::IndirectObjectMismatch {
                expected: (1, 1),
                actual: (1, 0)
            })
        ));
    }

    #[test]
    fn direct_indirect_and_nested_lengths_read_exact_owned_content() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 3 0 R >>\nstream\nworld\nendstream",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"5",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 0 R >>\nstream\nabcde\nendstream",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"6 0 R",
            },
            ObjectDef {
                id: 6,
                object_generation: 0,
                xref_generation: 0,
                body: b"5",
            },
        ]);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let eager = Document::load_mem(&pdf).unwrap();

        for (id, expected) in [((1, 0), b"hello".as_slice()), ((2, 0), b"world"), ((4, 0), b"abcde")] {
            let resolved = reader.resolve(id).unwrap();
            assert_eq!(resolved.as_stream().unwrap().content, expected);
            assert_eq!(
                resolved.as_stream().unwrap().content,
                eager.get_object(id).unwrap().as_stream().unwrap().content
            );
        }
    }

    #[test]
    fn stream_keyword_split_across_initial_window_grows_before_classifying_dictionary() {
        let target = usize::try_from(INITIAL_OBJECT_WINDOW).unwrap() - 3;
        let prefix = b"<< /Pad (";
        let suffix = b") /Length 5 >>\nstream\nhello\nendstream";
        let padding = target - prefix.len() - (b") /Length 5 >>\n").len();
        let mut body = prefix.to_vec();
        body.resize(body.len() + padding, b'x');
        body.extend_from_slice(suffix);
        assert_eq!(
            body.windows(b"stream".len()).position(|window| window == b"stream"),
            Some(target)
        );
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &body,
        }]);

        let stream = open_reader(&pdf, ResolverLimits::default()).resolve((1, 0)).unwrap();
        assert_eq!(stream.as_stream().unwrap().content, b"hello");
    }

    #[test]
    fn missing_malformed_cyclic_and_deep_lengths_degrade_to_empty_streams() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length (bad) >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 99 0 R >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 0 R >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"6 0 R",
            },
            ObjectDef {
                id: 6,
                object_generation: 0,
                xref_generation: 0,
                body: b"5 0 R",
            },
            ObjectDef {
                id: 7,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 8 0 R >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 8,
                object_generation: 0,
                xref_generation: 0,
                body: b"9 0 R",
            },
            ObjectDef {
                id: 9,
                object_generation: 0,
                xref_generation: 0,
                body: b"10 0 R",
            },
            ObjectDef {
                id: 10,
                object_generation: 0,
                xref_generation: 0,
                body: b"7",
            },
        ]);
        let reader = open_reader(
            &pdf,
            ResolverLimits {
                max_length_depth: 3,
                ..ResolverLimits::default()
            },
        );
        let eager = Document::load_mem(&pdf).unwrap();

        for id in [(1, 0), (2, 0), (3, 0), (4, 0)] {
            let resolved = reader.resolve(id).unwrap();
            assert_eq!(&resolved, eager.get_object(id).unwrap());
            assert!(resolved.as_stream().unwrap().content.is_empty());
        }
        assert!(reader.resolve((7, 0)).unwrap().as_stream().unwrap().content.is_empty());
        assert_eq!(
            eager.get_object((7, 0)).unwrap().as_stream().unwrap().content,
            b"ignored"
        );
    }

    #[test]
    fn negative_and_explicit_stream_resource_limits_fail() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length -1 >>\nstream\nvalue\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 3 0 R >>\nstream\nvalue\nendstream",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"-1",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 >>\nstream\nvalue\nendstream",
            },
        ]);
        let default_reader = open_reader(&pdf, ResolverLimits::default());
        for id in [(1, 0), (2, 0)] {
            assert!(matches!(
                default_reader.resolve(id),
                Err(IndexError::NegativeStreamLength { id: actual, length: -1 }) if actual == id
            ));
        }
        let limited_reader = open_reader(
            &pdf,
            ResolverLimits {
                max_stream_bytes: 4,
                ..ResolverLimits::default()
            },
        );
        assert!(matches!(
            limited_reader.resolve((4, 0)),
            Err(IndexError::StreamLimitExceeded {
                id: (4, 0),
                length: 5,
                limit: 4
            })
        ));

        let tail_limited_reader = open_reader(
            &pdf,
            ResolverLimits {
                max_endstream_tail_bytes: 4,
                ..ResolverLimits::default()
            },
        );
        assert!(matches!(
            tail_limited_reader.resolve((4, 0)),
            Err(IndexError::MissingEndstream { id: (4, 0) })
        ));
    }

    #[test]
    fn malformed_stream_syntax_backtracks_to_dictionary_like_eager() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Case /BadHeader >>\nstream % gap\nvalue\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Case /BadEnd >>\nstream\nvalue\nnot-endstream",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Case /MissingEnd >>\nstream\nvalue",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 1000000 /Case /PastSource >>\nstream\nshort",
            },
        ]);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let eager = Document::load_mem(&pdf).unwrap();

        for id in [(1, 0), (2, 0), (3, 0), (4, 0)] {
            let resolved = reader.resolve(id).unwrap();
            assert!(matches!(resolved, Object::Dictionary(_)));
            assert_eq!(&resolved, eager.get_object(id).unwrap());
        }
    }

    #[test]
    fn object_parser_limit_is_explicit() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Payload (abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz) >>",
        }]);
        let reader = open_reader(
            &pdf,
            ResolverLimits {
                max_object_bytes: 32,
                ..ResolverLimits::default()
            },
        );
        assert!(matches!(
            reader.resolve((1, 0)),
            Err(IndexError::ObjectLimitExceeded { id: (1, 0), limit: 32 })
        ));
    }

    struct OverlaySource {
        len: u64,
        regions: Vec<(u64, Vec<u8>)>,
        requests: Mutex<Vec<(u64, usize)>>,
    }

    struct TracingBytesSource {
        bytes: Vec<u8>,
        requests: Mutex<Vec<(u64, usize)>>,
    }

    impl RandomAccessSource for TracingBytesSource {
        fn len(&self) -> Result<u64, SourceError> {
            Ok(u64::try_from(self.bytes.len()).unwrap())
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
            self.requests.lock().unwrap().push((offset, output.len()));
            let offset = usize::try_from(offset).map_err(|_| SourceError::OutOfBounds {
                offset,
                length: u64::try_from(output.len()).unwrap_or(u64::MAX),
                source_len: u64::try_from(self.bytes.len()).unwrap(),
            })?;
            if offset > self.bytes.len() {
                return Ok(0);
            }
            let length = output.len().min(self.bytes.len() - offset);
            output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
            Ok(length)
        }
    }

    #[test]
    fn large_object_stream_is_call_local_owned_and_uncached() {
        let large = format!("({})", "z".repeat(4 * 1_024 * 1_024));
        let (first, content) = object_stream_content(&[(10, b"(tiny)"), (11, large.as_bytes())]);
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {first}"),
            &content,
            &[(10, 0), (11, 1)],
        );
        let source = Arc::new(TracingBytesSource {
            bytes: fixture.pdf,
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let first_value = reader.resolve((10, 0)).unwrap();
        let second_value = reader.resolve((10, 0)).unwrap();
        assert_eq!(first_value, Object::string_literal("tiny"));
        assert_eq!(second_value, first_value);
        let requests = source.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(offset, length)| {
                    *offset == fixture.container_stream_start
                        && u64::try_from(*length).unwrap() == fixture.container_stream_length
                })
                .count(),
            2
        );
        assert!(
            requests
                .iter()
                .all(|(_, length)| u64::try_from(*length).unwrap() <= fixture.container_stream_length)
        );
        drop(requests);
        drop(reader);
        drop(source);
        assert_eq!(first_value, Object::string_literal("tiny"));
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
    fn encrypted_open_and_resolution_are_bounded_on_a_sparse_hundred_megabyte_source() {
        let pdf = encrypted_pdf(4, "owner", "user");
        let pdf_source = BytesSource::from(pdf.clone());
        let pdf_len = u64::try_from(pdf.len()).unwrap();
        let xref = read_startxref(&pdf_source, pdf_len).unwrap();
        let len = 100_u64 * 1_024 * 1_024;
        let tail = format!("startxref\n{xref}\n%%EOF\n").into_bytes();
        let tail_offset = len - u64::try_from(tail.len()).unwrap();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![(0, pdf), (tail_offset, tail)],
            requests: Mutex::new(Vec::new()),
        });

        let reader =
            IndexedReader::open_with_password(source.clone(), ResolverLimits::default(), Some(b"user")).unwrap();
        assert_encrypted_fixture_plaintext(&reader);

        let requests = source.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|(_, length)| u64::try_from(*length).unwrap() <= TAIL_SCAN_LIMIT)
        );
        assert!(
            !requests
                .iter()
                .any(|(offset, length)| { *offset == 0 && u64::try_from(*length).unwrap_or(u64::MAX) == len })
        );
        let total: usize = requests.iter().map(|(_, length)| *length).sum();
        assert!(u64::try_from(total).unwrap() < 1_024 * 1_024);
    }

    #[test]
    fn five_kib_stream_dictionary_extends_without_prefix_or_payload_reread() {
        let len = 100_u64 * 1_024 * 1_024;
        let object_offset = 1_024_u64 * 1_024;
        let stream_length = 8_u64 * 1_024 * 1_024;
        let mut object_prefix = b"1 0 obj\n<< /Pad (".to_vec();
        object_prefix.resize(object_prefix.len() + 5 * 1_024, b'x');
        object_prefix.extend_from_slice(format!(") /Length {stream_length} >>\nstream\n").as_bytes());
        let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
        let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
        let stream_end = stream_start + stream_length;
        let xref = len - 512;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object_prefix),
                (stream_end, b"\nendstream\nendobj\n".to_vec()),
                (xref, xref_bytes),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

        let stream = reader.resolve((1, 0)).unwrap();
        assert_eq!(
            u64::try_from(stream.as_stream().unwrap().content.len()).unwrap(),
            stream_length
        );

        let requests = source.requests.lock().unwrap();
        let body_requests: Vec<_> = requests
            .iter()
            .filter(|(offset, _)| *offset >= body_offset && *offset < stream_start)
            .copied()
            .collect();
        assert_eq!(
            body_requests,
            vec![
                (body_offset, usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()),
                (
                    body_offset + INITIAL_OBJECT_WINDOW,
                    usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()
                )
            ]
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(offset, length)| {
                    *offset == stream_start && u64::try_from(*length).unwrap() == stream_length
                })
                .count(),
            1
        );
        OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));
    }

    #[test]
    fn large_image_stream_is_read_once_at_its_exact_declared_length() {
        let len = 500_u64 * 1_024 * 1_024;
        let object_offset = 1_024_u64 * 1_024;
        let stream_length = 8_u64 * 1_024 * 1_024;
        let object_prefix =
            format!("1 0 obj\n<< /Type /XObject /Subtype /Image /Length {stream_length} >>\nstream\n").into_bytes();
        let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
        let stream_end = stream_start + stream_length;
        let xref = len - 512;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object_prefix),
                (stream_end, b"\nendstream\nendobj\n".to_vec()),
                (xref, xref_bytes),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let image = reader.resolve((1, 0)).unwrap();
        assert_eq!(
            u64::try_from(image.as_stream().unwrap().content.len()).unwrap(),
            stream_length
        );

        let requests = source.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(offset, length)| {
                    *offset == stream_start && u64::try_from(*length).unwrap() == stream_length
                })
                .count(),
            1
        );
        assert!(
            requests
                .iter()
                .all(|(_, length)| u64::try_from(*length).unwrap() <= stream_length)
        );
        let total: u64 = requests.iter().map(|(_, length)| u64::try_from(*length).unwrap()).sum();
        assert!(total <= stream_length + 8 * 1_024);
    }

    #[test]
    fn semantic_invalid_object_stops_after_initial_probe() {
        let len = 100_u64 * 1_024 * 1_024;
        let object_offset = 1_024_u64 * 1_024;
        let object_prefix = b"1 0 obj\n<< /Broken @".to_vec();
        let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
        let xref = len - 512;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object_prefix),
                (xref, xref_bytes),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let error = reader.resolve((1, 0)).unwrap_err();
        assert!(
            matches!(error, IndexError::InvalidIndirectObject { id: (1, 0), .. }),
            "unexpected error: {error:?}"
        );
        let requests = source.requests.lock().unwrap();
        let body_lengths: Vec<_> = requests
            .iter()
            .filter_map(|(offset, length)| (*offset == body_offset).then_some(*length))
            .collect();
        assert_eq!(body_lengths, vec![usize::try_from(INITIAL_OBJECT_WINDOW).unwrap()]);
    }

    #[test]
    fn multi_megabyte_dictionary_caps_payload_lookahead_then_reads_stream_once() {
        let len = 100_u64 * 1_024 * 1_024;
        let object_offset = 1_024_u64 * 1_024;
        let stream_length = 8_u64 * 1_024 * 1_024;
        let mut object_prefix = b"1 0 obj\n<< /Pad (".to_vec();
        object_prefix.resize(object_prefix.len() + 2 * 1_024 * 1_024 + 1, b'x');
        object_prefix.extend_from_slice(format!(") /Length {stream_length} >>\nstream\n").as_bytes());
        let framed_body = object_prefix[b"1 0 obj\n".len()..].to_vec();
        let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
        let stream_end = stream_start + stream_length;
        let xref = len - 512;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object_prefix),
                (stream_end, b"\nendstream\nendobj\n".to_vec()),
                (xref, xref_bytes),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

        let stream = reader.resolve((1, 0)).unwrap();
        assert_eq!(
            u64::try_from(stream.as_stream().unwrap().content.len()).unwrap(),
            stream_length
        );

        let requests = source.requests.lock().unwrap();
        let speculative_payload_bytes: u64 = requests
            .iter()
            .filter_map(|(offset, length)| {
                let end = offset.checked_add(u64::try_from(*length).ok()?)?;
                (*offset < stream_start && end > stream_start).then(|| end - stream_start)
            })
            .sum();
        assert!(speculative_payload_bytes <= OBJECT_GROWTH_CHUNK);
        assert_eq!(
            requests
                .iter()
                .filter(|(offset, length)| {
                    *offset == stream_start && u64::try_from(*length).unwrap() == stream_length
                })
                .count(),
            1
        );
        OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));

        let mut framer = DirectObjectFramer::for_dictionary(&framed_body[..2]).unwrap();
        for end in (2..framed_body.len()).step_by(usize::try_from(OBJECT_GROWTH_CHUNK).unwrap()) {
            let _ = framer.advance(&framed_body[..end]);
        }
        assert_eq!(framer.advance(&framed_body), FrameStatus::Ready);
        assert!(framer.scanned_work <= framed_body.len());
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
        assert!(!dictionary_may_be_truncated(b"<< /Broken @"));
        assert!(!dictionary_may_be_truncated(b"<< /Nested << /Value 1 >> >>"));
        assert!(dictionary_may_be_truncated(b"<< /Text (a >> nested \\) value)"));
        assert!(!dictionary_may_be_truncated(
            b"<< /Text (a >> nested \\) value) /Hex <3e3e> >>"
        ));
    }

    #[test]
    fn dictionary_framer_matches_parser_at_every_split_point() {
        let valid = [
            b"<< /Value 1 >>\nendobj".as_slice(),
            b"<< /Nested << /Array [1 (two \\) >>) <3e3e>] >> /Name /has@sign >>\nendobj".as_slice(),
            b"<< /Length 5 >> % comment\nstream\nhello".as_slice(),
        ];
        for sample in valid {
            assert!(parse_object_body(sample, (1, 0), 0).is_ok());
            for split in 2..sample.len() {
                let mut framer = DirectObjectFramer::for_dictionary(&sample[..2]).unwrap();
                let first = framer.advance(&sample[..split]);
                if first != FrameStatus::Ready {
                    assert_eq!(framer.advance(sample), FrameStatus::Ready, "split {split}: {sample:?}");
                }
                assert!(framer.scanned_work <= sample.len());
            }
        }

        let invalid = b"<< /Broken @";
        assert!(matches!(
            parse_object_body(invalid, (1, 0), 0),
            Err(IndexError::InvalidIndirectObject { .. })
        ));
        for split in 2..=invalid.len() {
            let mut framer = DirectObjectFramer::for_dictionary(&invalid[..2]).unwrap();
            let _ = framer.advance(&invalid[..split]);
            assert_eq!(framer.advance(invalid), FrameStatus::Invalid);
            assert!(framer.scanned_work <= invalid.len());
        }
    }

    #[test]
    fn direct_object_framer_covers_every_object_kind_at_every_split_point() {
        let samples = [
            b"[true false null 1 -2 +3 1. .5 7 0 R /A#20B (x \\) y) <abc> << /K /V >>]\nendobj".as_slice(),
            b"/Name#20with#23escapes\nendobj".as_slice(),
            b"(literal (nested) \\) text)\nendobj".as_slice(),
            b"<0a B>\nendobj".as_slice(),
            b"false\nendobj".as_slice(),
            b"-123.5\nendobj".as_slice(),
            b"4294967295 65535 R\nendobj".as_slice(),
        ];
        for sample in samples {
            assert!(parse_object_body(sample, (1, 0), 0).is_ok(), "{sample:?}");
            for split in 0..sample.len() {
                let mut framer = DirectObjectFramer::new();
                let first = framer.advance(&sample[..split]);
                if first != FrameStatus::Ready {
                    assert_eq!(framer.advance(sample), FrameStatus::Ready, "split {split}: {sample:?}");
                }
                assert!(framer.scanned_work <= sample.len());
            }
        }
    }

    #[test]
    fn direct_object_framer_rejects_invalid_tokens_without_parsing() {
        let invalid = [
            b"@".as_slice(),
            b"truX ".as_slice(),
            b"<0g> ".as_slice(),
            b"[1 0 Q] ".as_slice(),
            b"[>> ".as_slice(),
            b"<< 1 /NotAKey >> ".as_slice(),
            b"<< /MissingValue >> ".as_slice(),
        ];
        for sample in invalid {
            for split in 0..=sample.len() {
                let mut framer = DirectObjectFramer::new();
                let _ = framer.advance(&sample[..split]);
                assert_eq!(
                    framer.advance(sample),
                    FrameStatus::Invalid,
                    "split {split}: {sample:?}"
                );
                assert!(framer.scanned_work <= sample.len());
            }
        }
    }

    #[test]
    fn malformed_top_level_tokens_match_eager_prefix_objects_and_consumption() {
        let cases = [
            (b"trueX".as_slice(), Object::Boolean(true), 4),
            (b"1x".as_slice(), Object::Integer(1), 1),
            (b"1 0 RX".as_slice(), Object::Reference((1, 0)), 5),
            (b"/bad#G0".as_slice(), Object::Name(b"bad".to_vec()), 4),
            (b"1.2.3".as_slice(), Object::Real(1.2), 3),
        ];

        for (body, expected, expected_consumed) in cases {
            let (consumed, parsed) = crate::parser::direct_object_with_consumed(body).unwrap();
            assert_eq!(parsed, expected, "{body:?}");
            assert_eq!(consumed, expected_consumed, "{body:?}");

            for split in 0..body.len() {
                let mut framer = DirectObjectFramer::new();
                let first = framer.advance(&body[..split]);
                if first != FrameStatus::Ready {
                    assert_eq!(framer.advance(body), FrameStatus::Ready, "split {split}: {body:?}");
                }
                assert!(framer.scanned_work <= body.len() + 6, "split {split}: {body:?}");
            }

            let pdf = object_pdf(&[ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body,
            }]);
            let eager = Document::load_mem(&pdf).unwrap();
            let indexed = open_reader(&pdf, ResolverLimits::default()).resolve((1, 0)).unwrap();
            assert_eq!(eager.objects.get(&(1, 0)).unwrap(), &expected, "{body:?}");
            assert_eq!(indexed, expected, "{body:?}");
        }
    }

    #[test]
    fn malformed_prefixes_remain_parser_compatible_when_nested() {
        for body in [
            b"[trueX] ".as_slice(),
            b"[1x] ".as_slice(),
            b"[1 0 RX] ".as_slice(),
            b"[/bad#G0] ".as_slice(),
        ] {
            assert!(crate::parser::direct_object_with_consumed(body).is_none(), "{body:?}");
            let mut framer = DirectObjectFramer::new();
            assert_eq!(framer.advance(body), FrameStatus::Invalid, "{body:?}");
        }

        let body = b"[1.2.3] ";
        let (consumed, expected) = crate::parser::direct_object_with_consumed(body).unwrap();
        assert_eq!(consumed, body.len());
        assert_eq!(expected, Object::Array(vec![Object::Real(1.2), Object::Real(0.3)]));
        let mut framer = DirectObjectFramer::new();
        assert_eq!(framer.advance(body), FrameStatus::Ready);
    }

    #[test]
    fn literal_nesting_limit_matches_eager_at_exact_boundary_and_one_beyond() {
        let literal = |levels: usize| {
            let mut body = Vec::with_capacity(levels * 2 + 1);
            body.extend(std::iter::repeat_n(b'(', levels));
            body.extend(std::iter::repeat_n(b')', levels));
            body.push(b' ');
            body
        };

        let accepted = literal(crate::reader::MAX_BRACKET + 1);
        assert!(crate::parser::direct_object_with_consumed(&accepted).is_some());
        let mut framer = DirectObjectFramer::new();
        assert_eq!(framer.advance(&accepted), FrameStatus::Ready);
        let accepted_pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &accepted,
        }]);
        let eager = Document::load_mem(&accepted_pdf).unwrap();
        assert_eq!(
            open_reader(&accepted_pdf, ResolverLimits::default())
                .resolve((1, 0))
                .unwrap(),
            eager.objects.get(&(1, 0)).unwrap().clone()
        );

        let rejected = literal(crate::reader::MAX_BRACKET + 2);
        assert!(crate::parser::direct_object_with_consumed(&rejected).is_none());
        let mut framer = DirectObjectFramer::new();
        assert_eq!(framer.advance(&rejected), FrameStatus::Invalid);

        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &rejected,
        }]);
        let eager = Document::load_mem(&pdf).unwrap();
        assert!(!eager.objects.contains_key(&(1, 0)));
        assert!(matches!(
            open_reader(&pdf, ResolverLimits::default()).resolve((1, 0)),
            Err(IndexError::InvalidIndirectObject { .. })
        ));
    }

    #[test]
    fn failed_reference_probe_does_not_revisit_long_space_or_comments() {
        let mut body = b"[1 ".to_vec();
        body.extend(std::iter::repeat_n(b' ', 512 * 1_024));
        body.extend_from_slice(b"% gap");
        body.extend(std::iter::repeat_n(b'x', 512 * 1_024));
        body.extend_from_slice(b"\n2 ");
        body.extend(std::iter::repeat_n(b' ', 512 * 1_024));
        body.extend_from_slice(b"% tail");
        body.extend(std::iter::repeat_n(b'y', 512 * 1_024));
        body.extend_from_slice(b"\n3] ");

        let (_, expected) = crate::parser::direct_object_with_consumed(&body).unwrap();
        assert_eq!(
            expected,
            Object::Array(vec![Object::Integer(1), Object::Integer(2), Object::Integer(3)])
        );

        let mut framer = DirectObjectFramer::new();
        for end in (1..body.len()).step_by(7_919) {
            assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
        }
        assert_eq!(framer.advance(&body), FrameStatus::Ready);
        assert!(
            framer.scanned_work <= body.len() + 6,
            "work={} input={}",
            framer.scanned_work,
            body.len()
        );
    }

    #[test]
    fn reference_probe_distinguishes_adjacent_decimal_from_separated_scalar() {
        let mut spaced = b"[1 2".to_vec();
        spaced.extend(std::iter::repeat_n(b' ', 127));
        spaced.extend_from_slice(b".3] ");

        let mut commented = b"[1 2%".to_vec();
        commented.extend(std::iter::repeat_n(b'x', 256 * 1_024));
        commented.extend_from_slice(b"\n.3] ");

        for body in [b"[1 2.3] ".to_vec(), spaced, commented] {
            let expected = if body == b"[1 2.3] " {
                Object::Array(vec![Object::Integer(1), Object::Real(2.3)])
            } else {
                Object::Array(vec![Object::Integer(1), Object::Integer(2), Object::Real(0.3)])
            };
            assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

            let mut framer = DirectObjectFramer::new();
            for end in (1..body.len()).step_by(7_919) {
                assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
            }
            assert_eq!(framer.advance(&body), FrameStatus::Ready);
            assert!(
                framer.scanned_work <= body.len() + 6,
                "work={} input={} expected={expected:?}",
                framer.scanned_work,
                body.len()
            );
        }
    }

    #[test]
    fn leading_zero_reference_probe_continues_adjacent_real_without_replay() {
        let mut body = b"[1 ".to_vec();
        body.extend(std::iter::repeat_n(b'0', 256 * 1_024));
        body.extend_from_slice(b"2.3 -4.5 +.6] ");
        let expected = Object::Array(vec![
            Object::Integer(1),
            Object::Real(2.3),
            Object::Real(-4.5),
            Object::Real(0.6),
        ]);
        assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

        let dot = body.windows(b"2.3".len()).position(|window| window == b"2.3").unwrap() + 1;
        let minus = body
            .windows(b"-4.5".len())
            .position(|window| window == b"-4.5")
            .unwrap();
        let plus = body.windows(b"+.6".len()).position(|window| window == b"+.6").unwrap();
        let mut splits: Vec<_> = (OBJECT_GROWTH_CHUNK as usize..body.len())
            .step_by(OBJECT_GROWTH_CHUNK as usize)
            .collect();
        splits.extend([dot, dot + 1, minus, minus + 1, plus, plus + 1]);
        splits.sort_unstable();
        splits.dedup();

        for split in splits {
            let mut framer = DirectObjectFramer::new();
            assert_ne!(framer.advance(&body[..split]), FrameStatus::Invalid, "split {split}");
            assert_eq!(framer.advance(&body), FrameStatus::Ready, "split {split}");
            assert!(
                framer.scanned_work <= body.len() + 6,
                "split={split} work={} input={}",
                framer.scanned_work,
                body.len()
            );
        }
    }

    #[test]
    fn stream_crlf_split_waits_for_lf_before_fixing_payload_offset() {
        let sample = b"<< /Length 1 >>\nstream\r\nx\nendstream";
        let split = sample.windows(2).position(|window| window == b"\r\n").unwrap() + 1;
        let mut framer = DirectObjectFramer::new();
        assert_eq!(framer.advance(&sample[..split]), FrameStatus::NeedMore);
        assert_eq!(framer.advance(sample), FrameStatus::Ready);
        let parsed = parse_object_body(sample, (1, 0), 0).unwrap();
        let prefix = usize::try_from(parsed.stream_prefix.unwrap()).unwrap();
        assert_eq!(&sample[parsed.consumed + prefix..parsed.consumed + prefix + 1], b"x");
    }

    #[test]
    fn two_mib_literal_is_framed_linearly_and_parsed_once_from_nonoverlapping_growth_reads() {
        let literal_length = 2_usize * 1_024 * 1_024;
        let mut body = Vec::with_capacity(literal_length + 16);
        body.push(b'(');
        body.resize(literal_length + 1, b'x');
        body.extend_from_slice(b")\nendobj\n");

        let mut framer = DirectObjectFramer::new();
        for end in (1..body.len()).step_by(7_919) {
            assert_ne!(framer.advance(&body[..end]), FrameStatus::Invalid);
        }
        assert_eq!(framer.advance(&body), FrameStatus::Ready);
        assert!(framer.scanned_work <= body.len());

        let object_offset = 1_024_u64 * 1_024;
        let mut object = b"1 0 obj\n".to_vec();
        object.extend_from_slice(&body);
        let xref = object_offset + u64::try_from(object.len()).unwrap() + 1_024;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len: xref + u64::try_from(xref_bytes.len()).unwrap(),
            regions: vec![(0, b"%PDF-1.7\n".to_vec()), (object_offset, object), (xref, xref_bytes)],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open(
            source.clone(),
            ResolverLimits {
                max_object_bytes: 3 * 1_024 * 1_024,
                ..ResolverLimits::default()
            },
        )
        .unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));
        assert_eq!(reader.resolve((1, 0)).unwrap().as_str().unwrap().len(), literal_length);
        OBJECT_BODY_PARSE_CALLS.with(|calls| assert_eq!(calls.get(), 1));

        let body_offset = object_offset + u64::try_from(b"1 0 obj\n".len()).unwrap();
        let requests = source.requests.lock().unwrap();
        let mut growth: Vec<_> = requests
            .iter()
            .copied()
            .filter(|(offset, _)| *offset >= body_offset && *offset < xref)
            .collect();
        growth.sort_unstable();
        for pair in growth.windows(2) {
            assert!(pair[0].0 + u64::try_from(pair[0].1).unwrap() <= pair[1].0, "{pair:?}");
        }
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

        for replacement in [b"".as_slice(), b"\x0c\n", b" % gap\n"] {
            let mut rejected = valid.clone();
            let marker = rejected
                .windows(b"stream\n".len())
                .position(|window| window == b"stream\n")
                .unwrap();
            let eol = marker + b"stream".len();
            rejected.splice(eol..eol + 1, replacement.iter().copied());
            assert!(Document::load_mem(&rejected).is_err());
            assert!(matches!(
                PdfIndex::open(Arc::new(BytesSource::from(rejected))),
                Err(IndexError::InvalidXref { .. })
            ));
        }

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
