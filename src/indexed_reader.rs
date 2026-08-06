//! Bounded structural bootstrap for the staged indexed reader.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[cfg(feature = "rayon")]
use rayon::prelude::*;
use thiserror::Error;

use crate::encryption::{self, EncryptionState, PasswordAlgorithm};
use crate::scalar_budget::{BoundedScalar, ScalarCharge, ScalarResolutionPermit};
use crate::source::{RandomAccessSource, SourceError};
use crate::source_cache::CachedSource;
use crate::{Dictionary, Object, ObjectStream, Stream};

pub use crate::source_cache::{IndexedReaderCacheOptions, IndexedReaderSourceCacheStats};

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
const DEFAULT_PAGE_TREE_DEPTH_LIMIT: usize = 256;
const DEFAULT_PAGE_COUNT_LIMIT: usize = 1_000_000;
const ENCODED_STREAM_CHUNK_LIMIT: usize = 64 * 1_024;
const PAGE_TREE_DEREFERENCE_LIMIT: usize = 128;
const SHARED_OBJECT_PROTECTED_PERCENT: usize = 75;

/// Result returned by the indexed random-access reader.
pub type IndexedReaderResult<T> = std::result::Result<T, IndexedReaderError>;

/// Shareable result returned by batched and shared object resolution.
pub type SharedIndexedReaderResult<T> = std::result::Result<T, Arc<IndexedReaderError>>;

/// Result returned while opening or reading a bounded encoded stream.
pub type IndexedStreamReadResult<T> = std::result::Result<T, IndexedStreamReadError>;

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

/// Structured failures while opening or reading encoded stream bytes.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexedStreamReadError {
    /// Stream metadata could not be resolved with the scalar reader's bounds
    /// and degradation rules.
    #[error(transparent)]
    Resolve(#[from] IndexedReaderError),
    /// The object is not a normal xref entry. Compressed objects cannot be
    /// streams in a conforming PDF and are deliberately not materialized here.
    #[error("object {id:?} is not an ordinary indexed object")]
    NotNormalObject { id: crate::ObjectId },
    /// Scalar resolution degrades this object to a non-stream value.
    #[error("object {id:?} does not resolve to a bounded stream")]
    NotStream { id: crate::ObjectId },
    /// The encoded payload is protected and cannot be exposed as plaintext by
    /// the L0 streaming seam.
    #[error("object {id:?} has protected encoded bytes ({protection:?})")]
    Protected {
        id: crate::ObjectId,
        protection: EncodedStreamProtection,
    },
    /// A safe encoded payload span could not be established.
    #[error("object {id:?} has no available encoded stream length ({reason:?})")]
    LengthUnavailable {
        id: crate::ObjectId,
        reason: EncodedStreamLengthUnavailableReason,
    },
    /// The source's reported length changed after the reader captured it.
    #[error("indexed source length changed: expected {expected}, found {actual}")]
    SourceLengthChanged { expected: u64, actual: u64 },
    /// A checked positional source read failed.
    #[error("indexed stream source error")]
    Source(#[from] SourceError),
}

/// Owned metadata and a private checked span for one encoded PDF stream.
///
/// The descriptor never owns payload bytes and intentionally exposes neither
/// the source offset nor encryption key material. It may be moved or shared
/// across threads; each opened reader has an independent cursor. Length checks
/// cannot detect a same-length byte rewrite, so the source's immutable-byte
/// contract remains required for the descriptor's entire lifetime.
pub struct IndexedStreamDescriptor {
    id: crate::ObjectId,
    dictionary: Dictionary,
    encoded_length: EncodedStreamLength,
    protection: EncodedStreamProtection,
    source: Arc<dyn RandomAccessSource>,
    source_len: u64,
    encoded_start: u64,
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
    id: crate::ObjectId,
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

/// A point-in-time snapshot of all bounded indexed-reader caches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedReaderCacheStats {
    source: IndexedReaderSourceCacheStats,
    object: IndexedObjectCacheStats,
    object_stream: IndexedObjectStreamCacheStats,
    current_bytes: u64,
    peak_bytes: u64,
    current_entries: usize,
    peak_entries: usize,
}

impl IndexedReaderCacheStats {
    /// Counters for the aligned positional source-chunk cache.
    pub const fn source(&self) -> &IndexedReaderSourceCacheStats {
        &self.source
    }

    /// Counters for resolved objects.
    pub const fn object(&self) -> &IndexedObjectCacheStats {
        &self.object
    }

    /// Counters for decoded object streams.
    pub const fn object_stream(&self) -> &IndexedObjectStreamCacheStats {
        &self.object_stream
    }

    /// Bytes currently retained or reserved across all cache partitions.
    pub const fn current_bytes(&self) -> u64 {
        self.current_bytes
    }

    /// Sum of the bounded per-partition byte high-water marks.
    pub const fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    /// Entries currently retained or loading across all cache partitions.
    pub const fn current_entries(&self) -> usize {
        self.current_entries
    }

    /// Sum of the bounded per-partition entry high-water marks.
    pub const fn peak_entries(&self) -> usize {
        self.peak_entries
    }
}

/// A point-in-time snapshot of the opt-in shared object cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedObjectCacheStats {
    /// Object-cell cache hits.
    pub object_hits: u64,
    /// Object-cell cache misses.
    pub object_misses: u64,
    /// Calls which waited for an in-flight object resolution.
    pub object_waits: u64,
    /// Object resolutions performed by cache leaders.
    pub object_loads: u64,
    /// Object cells promoted from probation to protected reuse.
    pub object_promotions: u64,
    /// Object cells evicted to enforce fixed caps.
    pub object_evictions: u64,
    /// Objects bypassed because one entry was too large or all cells were loading.
    pub object_bypasses: u64,
    /// Reuses of a cached deterministic object-resolution error.
    pub negative_hits: u64,
    /// Transient source failures shared with current waiters but not retained.
    pub transient_failures: u64,
    /// Resident probationary object entries.
    pub probation_entries: usize,
    /// Approximate resident bytes in probationary object entries.
    pub probation_bytes: usize,
    /// Resident protected object entries.
    pub protected_entries: usize,
    /// Approximate resident bytes in protected object entries.
    pub protected_bytes: usize,
    /// Peak resident entries after enforcing the fixed cache caps.
    pub peak_entries: usize,
    /// Peak approximate resident bytes after enforcing the fixed cache caps.
    pub peak_bytes: usize,
}

/// A point-in-time snapshot of the opt-in decoded object-stream cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedObjectStreamCacheStats {
    /// Decoded object-stream cache hits.
    pub hits: u64,
    /// Decoded object-stream cache misses.
    pub misses: u64,
    /// Calls which waited for an in-flight object-stream decode.
    pub waits: u64,
    /// Object-stream decrypt/decode/index operations performed by leaders.
    pub loads: u64,
    /// Decoded object-stream cells evicted to enforce configured caps.
    pub evictions: u64,
    /// Decoded object streams bypassed because one entry was too large or all cells were loading.
    pub bypasses: u64,
    /// Transient source failures shared with current waiters but not retained.
    pub transient_failures: u64,
    /// Resident decoded object-stream entries.
    pub entries: usize,
    /// Resident decoded object-stream bytes.
    pub bytes: usize,
    /// Peak resident decoded object-stream entries.
    pub peak_entries: usize,
    /// Peak resident decoded object-stream bytes.
    pub peak_bytes: usize,
}

#[cfg(test)]
thread_local! {
    static OBJECT_BODY_PARSE_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Why a normal xref entry has eager-compatible object-not-found semantics.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MissingNormalObjectReason {
    #[error("no indirect-object header appears within the {limit}-byte probe at xref offset {offset}")]
    HeaderProbeLimit { offset: u64, limit: u64 },
    #[error("indirect-object header mismatch: expected {expected:?}, found {actual:?}")]
    HeaderMismatch {
        expected: crate::ObjectId,
        actual: crate::ObjectId,
    },
    #[error(
        "indirect-object generation mismatch: requested {requested:?}, xref generation {indexed}, header {actual:?}"
    )]
    GenerationMismatch {
        requested: crate::ObjectId,
        indexed: u16,
        actual: crate::ObjectId,
    },
}

/// Structured failures produced while opening or resolving an indexed PDF.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexedReaderError {
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
    #[error("normal xref entry does not resolve object {id:?}: {reason}")]
    MissingNormalObjectAtXref {
        id: crate::ObjectId,
        #[source]
        reason: MissingNormalObjectReason,
    },
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
    #[error(
        "scalar resolution for object {id:?} needs {requested} simultaneous bytes during {phase}, exceeding limit {limit}"
    )]
    ScalarResourceLimit {
        id: crate::ObjectId,
        requested: u64,
        limit: u64,
        phase: &'static str,
    },
    #[error("scalar resolution for object {id:?} was cancelled during {phase}")]
    ScalarResolutionCancelled { id: crate::ObjectId, phase: &'static str },
    #[error("scalar-resolution permit is closed for object {id:?} during {phase}")]
    ScalarResolutionClosed { id: crate::ObjectId, phase: &'static str },
    #[error("object {id:?} is not a scalar object")]
    NotScalarObject { id: crate::ObjectId },
    #[error("bounded scalar resolution does not support {reason} for object {id:?}")]
    UnsupportedBoundedScalar { id: crate::ObjectId, reason: &'static str },
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
    #[error("failed to prepare object-stream container {container:?} for batched resolution")]
    ObjectStreamBatchSetup {
        container: crate::ObjectId,
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
    #[error("page tree exceeds the {limit}-page limit")]
    PageCountLimitExceeded { limit: usize },
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

pub(crate) struct PdfIndex {
    pub(crate) version: String,
    pub(crate) source_len: u64,
    pub(crate) source_origin: u64,
    pub(crate) xref_start: u64,
    pub(crate) xref_type: IndexXrefType,
    pub(crate) declared_size: u64,
    pub(crate) locations: BTreeMap<u32, ObjectLocation64>,
    pub(crate) trailer: Dictionary,
    pub(crate) encryption_state: Option<EncryptionState>,
    pub(crate) encrypt_object_id: Option<crate::ObjectId>,
}

/// Resource limits and optional password used by [`IndexedReader`].
///
/// Defaults preserve the indexed reader's bounded compatibility profile.
#[derive(Clone)]
pub struct IndexedReaderOptions {
    /// Maximum bytes parsed while resolving one ordinary object.
    pub object_bytes: u64,
    /// Maximum declared or decoded bytes retained for one stream.
    pub stream_bytes: u64,
    /// Maximum bytes inspected after a declared stream payload.
    pub endstream_tail_bytes: u64,
    /// Maximum recursive object/reference resolution depth.
    pub reference_depth: usize,
    /// Maximum page-tree depth followed while deriving a page map.
    pub page_tree_depth: usize,
    /// Maximum leaf pages retained in a derived page map.
    pub max_pages: usize,
    /// Optional raw PDF password. Debug output always redacts its value.
    pub password: Option<Vec<u8>>,
}

impl std::fmt::Debug for IndexedReaderOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexedReaderOptions")
            .field("object_bytes", &self.object_bytes)
            .field("stream_bytes", &self.stream_bytes)
            .field("endstream_tail_bytes", &self.endstream_tail_bytes)
            .field("reference_depth", &self.reference_depth)
            .field("page_tree_depth", &self.page_tree_depth)
            .field("max_pages", &self.max_pages)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl Default for IndexedReaderOptions {
    fn default() -> Self {
        Self {
            object_bytes: DEFAULT_OBJECT_LIMIT,
            stream_bytes: DEFAULT_STREAM_LIMIT,
            endstream_tail_bytes: DEFAULT_ENDSTREAM_TAIL_LIMIT,
            reference_depth: DEFAULT_LENGTH_DEPTH_LIMIT,
            page_tree_depth: DEFAULT_PAGE_TREE_DEPTH_LIMIT,
            max_pages: DEFAULT_PAGE_COUNT_LIMIT,
            password: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ResolverLimits {
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

impl From<&IndexedReaderOptions> for ResolverLimits {
    fn from(options: &IndexedReaderOptions) -> Self {
        Self {
            max_object_bytes: options.object_bytes,
            max_stream_bytes: options.stream_bytes,
            max_endstream_tail_bytes: options.endstream_tail_bytes,
            max_length_depth: options.reference_depth,
        }
    }
}

type SharedCellResult<T> = std::result::Result<Arc<T>, Arc<IndexedReaderError>>;

enum SharedCellState<T> {
    Loading,
    Ready(SharedCellResult<T>),
}

struct SharedCell<T> {
    state: Mutex<SharedCellState<T>>,
    ready: Condvar,
}

impl<T> SharedCell<T> {
    fn loading() -> Self {
        Self {
            state: Mutex::new(SharedCellState::Loading),
            ready: Condvar::new(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CacheSegment {
    Probation,
    Protected,
}

struct CacheEntry<T> {
    cell: Arc<SharedCell<T>>,
    segment: CacheSegment,
    bytes: usize,
}

struct SharedCacheInner<T> {
    entries: HashMap<crate::ObjectId, CacheEntry<T>>,
    probation: VecDeque<crate::ObjectId>,
    protected: VecDeque<crate::ObjectId>,
    probation_bytes: usize,
    protected_bytes: usize,
    loading_entries: usize,
}

impl<T> Default for SharedCacheInner<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            probation: VecDeque::new(),
            protected: VecDeque::new(),
            probation_bytes: 0,
            protected_bytes: 0,
            loading_entries: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum CacheKind {
    Object,
    ObjectStream,
}

struct SharedCache<T> {
    inner: Mutex<SharedCacheInner<T>>,
    max_bytes: usize,
    max_entries: usize,
    max_entry_bytes: usize,
    protected_percent: usize,
    kind: CacheKind,
    counters: Arc<CacheCounters>,
    #[cfg(test)]
    after_publish_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct CacheCounters {
    object_hits: AtomicU64,
    object_misses: AtomicU64,
    object_waits: AtomicU64,
    object_loads: AtomicU64,
    object_promotions: AtomicU64,
    object_evictions: AtomicU64,
    object_bypasses: AtomicU64,
    negative_hits: AtomicU64,
    object_transient_failures: AtomicU64,
    object_peak_entries: AtomicUsize,
    object_peak_bytes: AtomicUsize,
    objstm_hits: AtomicU64,
    objstm_misses: AtomicU64,
    objstm_waits: AtomicU64,
    objstm_loads: AtomicU64,
    objstm_evictions: AtomicU64,
    objstm_bypasses: AtomicU64,
    objstm_transient_failures: AtomicU64,
    objstm_peak_entries: AtomicUsize,
    objstm_peak_bytes: AtomicUsize,
}

enum PreparedObjectStream {
    Selected(crate::object_stream::SelectedObjectStream),
    Raw(Stream),
    NotStream,
}

impl PreparedObjectStream {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Selected(selected) => selected.retained_bytes(),
            Self::Raw(stream) => stream.content.len().saturating_add(std::mem::size_of::<Stream>()),
            Self::NotStream => std::mem::size_of::<Self>(),
        }
    }
}

impl<T> SharedCache<T> {
    fn new(
        max_bytes: usize, max_entries: usize, max_entry_bytes: usize, protected_percent: usize, kind: CacheKind,
        counters: Arc<CacheCounters>,
    ) -> Self {
        Self {
            inner: Mutex::new(SharedCacheInner::default()),
            max_bytes,
            max_entries,
            max_entry_bytes,
            protected_percent,
            kind,
            counters,
            #[cfg(test)]
            after_publish_hook: Mutex::new(None),
        }
    }

    fn resolve<F, W>(&self, id: crate::ObjectId, load: F, weight: W) -> SharedCellResult<T>
    where
        F: FnOnce() -> SharedCellResult<T>,
        W: FnOnce(&T) -> usize,
    {
        let (cell, leader) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.entries.get(&id) {
                let cell = Arc::clone(&entry.cell);
                self.record_hit();
                self.touch(&mut inner, id);
                (cell, false)
            } else {
                self.record_miss();
                while inner.entries.len() >= self.max_entries
                    && inner.loading_entries < inner.entries.len()
                    && self.evict_one_ready(&mut inner)
                {}
                if inner.entries.len() >= self.max_entries {
                    self.record_bypass();
                    self.record_load();
                    drop(inner);
                    return load();
                }
                let cell = Arc::new(SharedCell::loading());
                inner.probation.push_back(id);
                inner.entries.insert(
                    id,
                    CacheEntry {
                        cell: Arc::clone(&cell),
                        segment: CacheSegment::Probation,
                        bytes: 0,
                    },
                );
                inner.loading_entries = inner.loading_entries.saturating_add(1);
                self.record_residency_peaks(&inner);
                (cell, true)
            }
        };

        if !leader {
            let mut state = cell.state.lock().unwrap();
            if matches!(*state, SharedCellState::Loading) {
                self.record_wait();
            }
            while matches!(*state, SharedCellState::Loading) {
                state = cell.ready.wait(state).unwrap();
            }
            let SharedCellState::Ready(result) = &*state else {
                unreachable!();
            };
            if result
                .as_ref()
                .err()
                .is_some_and(|error| !is_transient_error(error.as_ref()))
            {
                atomic_saturating_increment(&self.counters.negative_hits);
            }
            return result.clone();
        }

        self.record_load();
        let result = load();
        let transient = result
            .as_ref()
            .err()
            .is_some_and(|error| is_transient_error(error.as_ref()));
        let retained_bytes = result.as_ref().ok().map_or(0, |value| weight(value.as_ref()));
        let bypass = retained_bytes > self.max_entry_bytes || retained_bytes > self.max_bytes;
        let mut inner = self.inner.lock().unwrap();
        inner.loading_entries = inner.loading_entries.saturating_sub(1);
        if transient || bypass {
            if transient {
                atomic_saturating_increment(match self.kind {
                    CacheKind::Object => &self.counters.object_transient_failures,
                    CacheKind::ObjectStream => &self.counters.objstm_transient_failures,
                });
            } else {
                self.record_bypass();
            }
            self.remove_if_same(&mut inner, id, &cell, false);
        } else {
            if let Some(entry) = inner.entries.get_mut(&id)
                && Arc::ptr_eq(&entry.cell, &cell)
            {
                entry.bytes = retained_bytes;
                match entry.segment {
                    CacheSegment::Probation => {
                        inner.probation_bytes = inner.probation_bytes.saturating_add(retained_bytes);
                    }
                    CacheSegment::Protected => {
                        inner.protected_bytes = inner.protected_bytes.saturating_add(retained_bytes);
                    }
                }
            }
            self.enforce_caps(&mut inner);
            self.record_residency_peaks(&inner);
        }
        {
            let mut state = cell.state.lock().unwrap();
            *state = SharedCellState::Ready(result.clone());
            cell.ready.notify_all();
        }
        drop(inner);
        #[cfg(test)]
        let hook = self.after_publish_hook.lock().unwrap().take();
        #[cfg(test)]
        if let Some(hook) = hook {
            hook();
        }
        result
    }

    fn touch(&self, inner: &mut SharedCacheInner<T>, id: crate::ObjectId) {
        let Some(segment) = inner.entries.get(&id).map(|entry| entry.segment) else {
            return;
        };
        match segment {
            CacheSegment::Probation => {
                remove_key(&mut inner.probation, id);
                let bytes = inner.entries.get(&id).map_or(0, |entry| entry.bytes);
                inner.probation_bytes = inner.probation_bytes.saturating_sub(bytes);
                inner.protected_bytes = inner.protected_bytes.saturating_add(bytes);
                if let Some(entry) = inner.entries.get_mut(&id) {
                    entry.segment = CacheSegment::Protected;
                }
                inner.protected.push_back(id);
                if matches!(self.kind, CacheKind::Object) {
                    atomic_saturating_increment(&self.counters.object_promotions);
                }
                self.demote_protected(inner);
            }
            // Protected entries are deliberately FIFO. Avoiding an O(n)
            // move-to-back on every hot hit keeps the shared fast path
            // independent of the configured entry cap.
            CacheSegment::Protected => {}
        }
    }

    fn demote_protected(&self, inner: &mut SharedCacheInner<T>) {
        let protected_byte_cap = self.max_bytes.saturating_mul(self.protected_percent) / 100;
        let protected_entry_cap = self.max_entries.saturating_mul(self.protected_percent) / 100;
        while inner.protected_bytes > protected_byte_cap || inner.protected.len() > protected_entry_cap {
            let Some(id) = inner.protected.pop_front() else {
                break;
            };
            let Some(entry) = inner.entries.get_mut(&id) else {
                continue;
            };
            entry.segment = CacheSegment::Probation;
            inner.protected_bytes = inner.protected_bytes.saturating_sub(entry.bytes);
            inner.probation_bytes = inner.probation_bytes.saturating_add(entry.bytes);
            inner.probation.push_back(id);
        }
    }

    fn enforce_caps(&self, inner: &mut SharedCacheInner<T>) {
        self.demote_protected(inner);
        let protected_byte_cap = self.max_bytes.saturating_mul(self.protected_percent) / 100;
        let protected_entry_cap = self.max_entries.saturating_mul(self.protected_percent) / 100;
        let probation_byte_cap = self.max_bytes.saturating_sub(protected_byte_cap);
        let probation_entry_cap = self.max_entries.saturating_sub(protected_entry_cap);
        let mut pinned = 0;
        while inner.entries.len() > self.max_entries
            || inner.probation_bytes.saturating_add(inner.protected_bytes) > self.max_bytes
            || inner.probation_bytes > probation_byte_cap
            || inner.probation.len() > probation_entry_cap
        {
            let candidate = inner.probation.pop_front().or_else(|| inner.protected.pop_front());
            let Some(id) = candidate else {
                break;
            };
            let Some(entry) = inner.entries.get(&id) else {
                continue;
            };
            if matches!(*entry.cell.state.lock().unwrap(), SharedCellState::Loading) {
                match entry.segment {
                    CacheSegment::Probation => inner.probation.push_back(id),
                    CacheSegment::Protected => inner.protected.push_back(id),
                }
                pinned += 1;
                if pinned >= inner.probation.len().max(1) {
                    break;
                }
                continue;
            }
            // Completed cells are safe to unlink even while callers retain
            // their cell or result Arcs. Only Loading cells must stay mapped
            // so a second leader cannot start for the same id.
            self.remove_entry(inner, id, true);
            pinned = 0;
        }
    }

    fn evict_one_ready(&self, inner: &mut SharedCacheInner<T>) -> bool {
        let candidates = inner.probation.len().saturating_add(inner.protected.len());
        for _ in 0..candidates {
            let Some(id) = inner.probation.pop_front().or_else(|| inner.protected.pop_front()) else {
                return false;
            };
            let Some(entry) = inner.entries.get(&id) else {
                continue;
            };
            if matches!(*entry.cell.state.lock().unwrap(), SharedCellState::Loading) {
                match entry.segment {
                    CacheSegment::Probation => inner.probation.push_back(id),
                    CacheSegment::Protected => inner.protected.push_back(id),
                }
                continue;
            }
            self.remove_entry(inner, id, true);
            return true;
        }
        false
    }

    fn remove_if_same(
        &self, inner: &mut SharedCacheInner<T>, id: crate::ObjectId, cell: &Arc<SharedCell<T>>, eviction: bool,
    ) {
        if inner
            .entries
            .get(&id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.cell, cell))
        {
            self.remove_entry(inner, id, eviction);
        }
    }

    fn remove_entry(&self, inner: &mut SharedCacheInner<T>, id: crate::ObjectId, eviction: bool) {
        let Some(entry) = inner.entries.remove(&id) else {
            return;
        };
        match entry.segment {
            CacheSegment::Probation => {
                remove_key(&mut inner.probation, id);
                inner.probation_bytes = inner.probation_bytes.saturating_sub(entry.bytes);
            }
            CacheSegment::Protected => {
                remove_key(&mut inner.protected, id);
                inner.protected_bytes = inner.protected_bytes.saturating_sub(entry.bytes);
            }
        }
        if eviction {
            self.record_eviction();
        }
    }

    fn record_hit(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_hits,
            CacheKind::ObjectStream => &self.counters.objstm_hits,
        });
    }

    fn record_miss(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_misses,
            CacheKind::ObjectStream => &self.counters.objstm_misses,
        });
    }

    fn record_wait(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_waits,
            CacheKind::ObjectStream => &self.counters.objstm_waits,
        });
    }

    fn record_load(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_loads,
            CacheKind::ObjectStream => &self.counters.objstm_loads,
        });
    }

    fn record_eviction(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_evictions,
            CacheKind::ObjectStream => &self.counters.objstm_evictions,
        });
    }

    fn record_bypass(&self) {
        atomic_saturating_increment(match self.kind {
            CacheKind::Object => &self.counters.object_bypasses,
            CacheKind::ObjectStream => &self.counters.objstm_bypasses,
        });
    }

    fn residency(&self) -> (usize, usize, usize, usize) {
        let inner = self.inner.lock().unwrap();
        (
            inner.probation.len(),
            inner.probation_bytes,
            inner.protected.len(),
            inner.protected_bytes,
        )
    }

    fn record_residency_peaks(&self, inner: &SharedCacheInner<T>) {
        let entries = inner.entries.len();
        let bytes = inner.probation_bytes.saturating_add(inner.protected_bytes);
        let (peak_entries, peak_bytes) = match self.kind {
            CacheKind::Object => (&self.counters.object_peak_entries, &self.counters.object_peak_bytes),
            CacheKind::ObjectStream => (&self.counters.objstm_peak_entries, &self.counters.objstm_peak_bytes),
        };
        peak_entries.fetch_max(entries, Ordering::Relaxed);
        peak_bytes.fetch_max(bytes, Ordering::Relaxed);
    }
}

fn atomic_saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn usize_from_u64_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn u64_from_usize_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn remove_key(queue: &mut VecDeque<crate::ObjectId>, id: crate::ObjectId) {
    if let Some(position) = queue.iter().position(|candidate| *candidate == id) {
        queue.remove(position);
    }
}

fn is_transient_error(error: &IndexedReaderError) -> bool {
    match error {
        IndexedReaderError::Source(_) => true,
        IndexedReaderError::ObjectStreamMember { source, .. }
        | IndexedReaderError::ObjectStreamBatchSetup { source, .. } => matches!(source, crate::Error::IO(_)),
        _ => false,
    }
}

fn object_retained_bytes(root: &Object) -> usize {
    let mut bytes = std::mem::size_of::<Object>();
    let mut pending = vec![root];
    while let Some(object) = pending.pop() {
        match object {
            Object::Name(value) | Object::String(value, _) => {
                bytes = bytes.saturating_add(value.capacity());
            }
            Object::Array(values) => {
                bytes = bytes.saturating_add(values.capacity().saturating_mul(std::mem::size_of::<Object>()));
                pending.extend(values);
            }
            Object::Dictionary(dictionary) => {
                for (key, value) in dictionary.iter() {
                    bytes = bytes.saturating_add(key.capacity());
                    bytes = bytes.saturating_add(std::mem::size_of::<(Vec<u8>, Object)>());
                    pending.push(value);
                }
            }
            Object::Stream(stream) => {
                bytes = bytes.saturating_add(stream.content.capacity());
                for (key, value) in stream.dict.iter() {
                    bytes = bytes.saturating_add(key.capacity());
                    bytes = bytes.saturating_add(std::mem::size_of::<(Vec<u8>, Object)>());
                    pending.push(value);
                }
            }
            Object::Null | Object::Boolean(_) | Object::Integer(_) | Object::Real(_) | Object::Reference(_) => {}
        }
    }
    bytes
}

fn dictionary_retained_bytes(dictionary: &Dictionary) -> usize {
    let mut bytes = std::mem::size_of::<Dictionary>();
    let mut pending = Vec::new();
    for (key, value) in dictionary.iter() {
        // IndexMap's hash/index control storage is deliberately charged with a
        // conservative fixed envelope in addition to the visible key/value.
        bytes = bytes
            .saturating_add(key.capacity())
            .saturating_add(std::mem::size_of::<(Vec<u8>, Object)>())
            .saturating_add(64);
        pending.push(value);
    }
    while let Some(object) = pending.pop() {
        bytes = bytes.saturating_add(std::mem::size_of::<Object>());
        match object {
            Object::Name(value) | Object::String(value, _) => {
                bytes = bytes.saturating_add(value.capacity());
            }
            Object::Array(values) => {
                bytes = bytes.saturating_add(values.capacity().saturating_mul(std::mem::size_of::<Object>()));
                pending.extend(values);
            }
            Object::Dictionary(nested) => {
                for (key, value) in nested.iter() {
                    bytes = bytes
                        .saturating_add(key.capacity())
                        .saturating_add(std::mem::size_of::<(Vec<u8>, Object)>())
                        .saturating_add(64);
                    pending.push(value);
                }
            }
            Object::Stream(stream) => {
                bytes = bytes.saturating_add(stream.content.capacity());
                for (key, value) in stream.dict.iter() {
                    bytes = bytes
                        .saturating_add(key.capacity())
                        .saturating_add(std::mem::size_of::<(Vec<u8>, Object)>())
                        .saturating_add(64);
                    pending.push(value);
                }
            }
            Object::Null | Object::Boolean(_) | Object::Integer(_) | Object::Real(_) | Object::Reference(_) => {}
        }
    }
    bytes
}

fn selected_object_stream_member(
    decoded: &[u8], first: usize, declared_members: usize, expected_id: crate::ObjectId, member_index: u32,
) -> crate::Result<&[u8]> {
    let dictionary_error = || crate::Error::InvalidObjectStream("invalid selected object stream header".into());
    if expected_id.1 != 0 {
        return Err(crate::Error::InvalidObjectStream(
            "compressed objects must have generation zero".into(),
        ));
    }
    let header = decoded.get(..first).ok_or_else(dictionary_error)?;
    let text = std::str::from_utf8(header).map_err(|_| dictionary_error())?;
    let selected_index = usize::try_from(member_index).map_err(|_| dictionary_error())?;
    if selected_index >= declared_members {
        return Err(dictionary_error());
    }
    let mut tokens = text.split_whitespace();
    let object_number = tokens
        .nth(selected_index.saturating_mul(2))
        .and_then(|token| token.parse::<u32>().ok())
        .ok_or_else(dictionary_error)?;
    let relative_offset = tokens
        .next()
        .and_then(|token| token.parse::<usize>().ok())
        .ok_or_else(dictionary_error)?;
    if object_number != expected_id.0 {
        return Err(crate::Error::InvalidObjectStream(format!(
            "member index {member_index} declares object {object_number}, not {}",
            expected_id.0
        )));
    }
    let start = first.checked_add(relative_offset).ok_or_else(dictionary_error)?;
    let next = text
        .split_whitespace()
        .skip(1)
        .step_by(2)
        .filter_map(|token| token.parse::<usize>().ok())
        .filter(|offset| *offset > relative_offset)
        .min()
        .and_then(|offset| first.checked_add(offset))
        .unwrap_or(decoded.len())
        .min(decoded.len());
    decoded.get(start..next).ok_or_else(dictionary_error)
}

/// Immutable indexed reader over a cursor-free random-access source.
///
/// All reads are synchronous. Custom sources must be thread-safe and must keep
/// their length and bytes stable for the reader's lifetime. Resolved objects
/// and page maps are owned, so independent calls may run concurrently.
pub struct IndexedReader {
    source: Arc<dyn RandomAccessSource>,
    cached_source: Option<Arc<CachedSource>>,
    index: Arc<PdfIndex>,
    limits: ResolverLimits,
    options: IndexedReaderOptions,
    object_cache: Option<SharedCache<Object>>,
    object_stream_cache: Option<SharedCache<PreparedObjectStream>>,
    cache_counters: Option<Arc<CacheCounters>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InheritedPageAttributeOwners {
    resources: Option<crate::ObjectId>,
    media_box: Option<crate::ObjectId>,
    crop_box: Option<crate::ObjectId>,
    rotate: Option<crate::ObjectId>,
}

impl InheritedPageAttributeOwners {
    pub fn resources(&self) -> Option<crate::ObjectId> {
        self.resources
    }

    pub fn media_box(&self) -> Option<crate::ObjectId> {
        self.media_box
    }

    pub fn crop_box(&self) -> Option<crate::ObjectId> {
        self.crop_box
    }

    pub fn rotate(&self) -> Option<crate::ObjectId> {
        self.rotate
    }

    fn updated(mut self, owner: crate::ObjectId, dictionary: &Dictionary) -> Self {
        if dictionary.has(b"Resources") {
            self.resources = Some(owner);
        }
        if dictionary.has(b"MediaBox") {
            self.media_box = Some(owner);
        }
        if dictionary.has(b"CropBox") {
            self.crop_box = Some(owner);
        }
        if dictionary.has(b"Rotate") {
            self.rotate = Some(owner);
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageMapEntry {
    id: crate::ObjectId,
    inherited: InheritedPageAttributeOwners,
}

impl PageMapEntry {
    pub fn id(&self) -> crate::ObjectId {
        self.id
    }

    pub fn inherited(&self) -> &InheritedPageAttributeOwners {
        &self.inherited
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageMap {
    pages: Vec<PageMapEntry>,
}

impl PageMap {
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&PageMapEntry> {
        self.pages.get(index)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &PageMapEntry> + DoubleEndedIterator + '_ {
        self.pages.iter()
    }
}

#[derive(Clone, Copy, Debug)]
struct PageMapLimits {
    max_depth: usize,
    max_pages: usize,
}

impl Default for PageMapLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_PAGE_TREE_DEPTH_LIMIT,
            max_pages: DEFAULT_PAGE_COUNT_LIMIT,
        }
    }
}

struct PageMapBuilder<'a> {
    reader: &'a IndexedReader,
    limits: PageMapLimits,
    remaining_work: usize,
    consumed_work: usize,
    peak_pending_items: usize,
}

#[derive(Clone)]
struct PendingKid {
    id: Option<crate::ObjectId>,
    inherited: Rc<InheritedPageAttributeOwners>,
    depth: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PageMapWork {
    consumed: usize,
    peak_pending_items: usize,
    peak_pending_bytes: usize,
}

impl PageMap {
    fn from_reader(reader: &IndexedReader) -> IndexedReaderResult<Self> {
        Self::from_reader_with_limits(
            reader,
            PageMapLimits {
                max_depth: reader.options.page_tree_depth,
                max_pages: reader.options.max_pages,
            },
        )
    }

    fn from_reader_with_limits(reader: &IndexedReader, limits: PageMapLimits) -> IndexedReaderResult<Self> {
        Self::from_reader_with_limits_and_work(reader, limits).map(|(page_map, _)| page_map)
    }

    fn from_reader_with_limits_and_work(
        reader: &IndexedReader, limits: PageMapLimits,
    ) -> IndexedReaderResult<(Self, usize)> {
        Self::from_reader_with_limits_and_stats(reader, limits).map(|(page_map, work)| (page_map, work.consumed))
    }

    fn from_reader_with_limits_and_stats(
        reader: &IndexedReader, limits: PageMapLimits,
    ) -> IndexedReaderResult<(Self, PageMapWork)> {
        let work_budget = reader
            .index
            .locations
            .values()
            .filter(|location| !matches!(location, ObjectLocation64::Free { .. }))
            .count();
        Self::from_reader_with_work_budget_and_stats(reader, limits, work_budget)
    }

    fn from_reader_with_work_budget_and_stats(
        reader: &IndexedReader, limits: PageMapLimits, work_budget: usize,
    ) -> IndexedReaderResult<(Self, PageMapWork)> {
        let Some(root_id) = reader
            .index
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| root.as_reference().ok())
        else {
            return Ok((Self::default(), PageMapWork::default()));
        };
        let Some(catalog) = reader.resolve_dictionary_deref(root_id)? else {
            return Ok((Self::default(), PageMapWork::default()));
        };
        let Some(pages_id) = catalog.get(b"Pages").ok().and_then(|pages| pages.as_reference().ok()) else {
            return Ok((Self::default(), PageMapWork::default()));
        };

        let mut page_map = Self::default();
        let mut builder = PageMapBuilder {
            reader,
            limits,
            remaining_work: work_budget,
            consumed_work: 0,
            peak_pending_items: 0,
        };
        builder.walk_page_tree(&mut page_map, pages_id)?;
        Ok((
            page_map,
            PageMapWork {
                consumed: builder.consumed_work,
                peak_pending_items: builder.peak_pending_items,
                peak_pending_bytes: builder
                    .peak_pending_items
                    .saturating_mul(std::mem::size_of::<PendingKid>()),
            },
        ))
    }
}

impl PageMapBuilder<'_> {
    fn walk_page_tree(&mut self, page_map: &mut PageMap, root_id: crate::ObjectId) -> IndexedReaderResult<()> {
        let Some(mut root) = self.reader.resolve_dictionary_deref(root_id)? else {
            return Ok(());
        };
        let inherited = InheritedPageAttributeOwners::default().updated(root_id, &root);
        let kids_value = root.remove(b"Kids");
        // Do not retain the resolved dictionary alongside its potentially wide
        // `/Kids`; only compact pending slots survive into traversal.
        drop(root);
        let Some(kids) = self.reader.resolve_array_value(kids_value)? else {
            return Ok(());
        };
        let mut pending = VecDeque::new();
        self.prepend_kids(&mut pending, kids, Rc::new(inherited), 1);

        while let Some(kid) = pending.pop_front() {
            self.remaining_work -= 1;
            self.consumed_work += 1;

            let Some(id) = kid.id else {
                continue;
            };
            if usize::try_from(kid.depth).unwrap_or(usize::MAX) > self.limits.max_depth {
                continue;
            }
            let Some(mut dictionary) = self.reader.resolve_dictionary_deref(id)? else {
                continue;
            };
            let inherited = (*kid.inherited).updated(id, &dictionary);
            match dictionary.get_type() {
                Ok(b"Page") => {
                    if page_map.pages.len() >= self.limits.max_pages {
                        return Err(IndexedReaderError::PageCountLimitExceeded {
                            limit: self.limits.max_pages,
                        });
                    }
                    page_map.pages.push(PageMapEntry { id, inherited });
                }
                Ok(b"Pages") => {
                    let kids_value = dictionary.remove(b"Kids");
                    drop(dictionary);
                    if let Some(kids) = self.reader.resolve_array_value(kids_value)? {
                        self.prepend_kids(&mut pending, kids, Rc::new(inherited), kid.depth.saturating_add(1));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn prepend_kids(
        &mut self, pending: &mut VecDeque<PendingKid>, mut kids: Vec<Object>,
        inherited: Rc<InheritedPageAttributeOwners>, depth: u32,
    ) {
        // Only the first `remaining_work` DFS slots can ever be observed. Drop
        // later siblings before prepending children, then convert every owned
        // Object into a fixed-size slot as it leaves the temporary Kids array.
        kids.truncate(self.remaining_work);
        pending.truncate(self.remaining_work - kids.len());
        for kid in kids.into_iter().rev() {
            pending.push_front(PendingKid {
                id: kid.as_reference().ok(),
                inherited: Rc::clone(&inherited),
                depth,
            });
        }
        debug_assert!(pending.len() <= self.remaining_work);
        self.peak_pending_items = self.peak_pending_items.max(pending.len());
    }
}

impl IndexedReader {
    /// Open a source with the default bounded options.
    pub fn open<S: RandomAccessSource>(source: S) -> IndexedReaderResult<Self> {
        Self::open_with_options(source, IndexedReaderOptions::default())
    }

    /// Open a source with explicit resource limits and password handling.
    pub fn open_with_options<S: RandomAccessSource>(
        source: S, options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        Self::open_shared(Arc::new(source), options)
    }

    /// Open a shared source without adding another source allocation.
    pub fn open_shared(
        source: Arc<dyn RandomAccessSource>, options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        Self::from_erased_source(source, options)
    }

    fn from_erased_source(
        source: Arc<dyn RandomAccessSource>, mut options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        let index = PdfIndex::open(Arc::clone(&source))?;
        let limits = ResolverLimits::from(&options);
        let password = options.password.take();
        let mut reader = Self {
            source,
            cached_source: None,
            index: Arc::new(index),
            limits,
            options,
            object_cache: None,
            object_stream_cache: None,
            cache_counters: None,
        };
        reader.initialize_encryption(password.as_deref())?;
        Ok(reader)
    }

    /// Open a source with explicit reader limits and a bounded cache budget.
    pub fn open_cached<S: RandomAccessSource>(
        source: S, options: IndexedReaderOptions, cache_options: IndexedReaderCacheOptions,
    ) -> IndexedReaderResult<Self> {
        Self::open_shared_cached(Arc::new(source), options, cache_options)
    }

    /// Open a shared source with explicit reader limits and a bounded cache budget.
    pub fn open_shared_cached(
        source: Arc<dyn RandomAccessSource>, options: IndexedReaderOptions, cache_options: IndexedReaderCacheOptions,
    ) -> IndexedReaderResult<Self> {
        let cached_source = Arc::new(CachedSource::new(source, cache_options)?);
        let erased: Arc<dyn RandomAccessSource> = cached_source.clone();
        let mut reader = Self::from_erased_source(erased, options)?;
        reader.cached_source = Some(cached_source);
        reader.configure_resolution_caches(
            usize_from_u64_saturating(cache_options.object_max_bytes()),
            cache_options.object_max_entries(),
            usize_from_u64_saturating(cache_options.object_stream_max_bytes()),
            cache_options.object_stream_max_entries(),
        );
        Ok(reader)
    }

    /// Return a snapshot of cache counters, or zero counters for an uncached reader.
    pub fn cache_stats(&self) -> IndexedReaderCacheStats {
        let Some(source) = self.cached_source.as_ref() else {
            return IndexedReaderCacheStats::default();
        };
        let source = source.stats();
        let object = self.object_cache_stats();
        let object_stream = self.object_stream_cache_stats();
        let object_bytes = object.probation_bytes.saturating_add(object.protected_bytes);
        let object_entries = object.probation_entries.saturating_add(object.protected_entries);
        IndexedReaderCacheStats {
            current_bytes: source
                .retained_bytes()
                .saturating_add(source.in_flight_bytes())
                .saturating_add(u64_from_usize_saturating(object_bytes))
                .saturating_add(u64_from_usize_saturating(object_stream.bytes)),
            peak_bytes: source
                .peak_bytes()
                .saturating_add(u64_from_usize_saturating(object.peak_bytes))
                .saturating_add(u64_from_usize_saturating(object_stream.peak_bytes)),
            current_entries: source
                .retained_entries()
                .saturating_add(source.in_flight_entries())
                .saturating_add(object_entries)
                .saturating_add(object_stream.entries),
            peak_entries: source
                .peak_entries()
                .saturating_add(object.peak_entries)
                .saturating_add(object_stream.peak_entries),
            source,
            object,
            object_stream,
        }
    }

    fn open_with_limits(source: Arc<dyn RandomAccessSource>, limits: ResolverLimits) -> IndexedReaderResult<Self> {
        let options = IndexedReaderOptions {
            object_bytes: limits.max_object_bytes,
            stream_bytes: limits.max_stream_bytes,
            endstream_tail_bytes: limits.max_endstream_tail_bytes,
            reference_depth: limits.max_length_depth,
            ..IndexedReaderOptions::default()
        };
        Self::from_erased_source(source, options)
    }

    fn open_with_password(
        source: Arc<dyn RandomAccessSource>, limits: ResolverLimits, password: Option<&[u8]>,
    ) -> IndexedReaderResult<Self> {
        let options = IndexedReaderOptions {
            object_bytes: limits.max_object_bytes,
            stream_bytes: limits.max_stream_bytes,
            endstream_tail_bytes: limits.max_endstream_tail_bytes,
            reference_depth: limits.max_length_depth,
            password: password.map(<[u8]>::to_vec),
            ..IndexedReaderOptions::default()
        };
        Self::from_erased_source(source, options)
    }

    /// Resolve one full object id into an owned value.
    pub fn resolve_object(&self, id: crate::ObjectId) -> IndexedReaderResult<Object> {
        let mut state = ResolutionState::default();
        self.resolve_inner(id, &mut state)
    }

    /// Resolve one scalar object under one process-external, call-local memory
    /// allowance. This seam never returns a stream. The ordinary indexed-reader
    /// APIs retain their existing behavior and do not pay for this accounting.
    pub fn resolve_scalar_with_permit(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        if permit.stats().current_bytes != 0 {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: permit.stats().current_bytes,
                limit: permit.limit_bytes(),
                phase: "permit-not-empty",
            });
        }
        if self.index.encryption_state.is_some() {
            return Err(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "encrypted scalar objects",
            });
        }
        match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal_scalar_limited(id, permit),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed_scalar_limited(id, container, index, permit)
            }
            Some(ObjectLocation64::Free { .. }) | None => Err(IndexedReaderError::MissingNormalObject { id }),
        }
    }

    fn resolve_normal_scalar_limited(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexedReaderError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header_len = (source_len.saturating_sub(physical)).min(INDIRECT_HEADER_LIMIT);
        let header = ChargedBytes::read(
            self.source.as_ref(),
            physical,
            header_len,
            permit,
            id,
            "indirect-header",
        )?;
        let (actual, header_bytes) =
            parse_indirect_header(&header.bytes).ok_or(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit {
                    offset,
                    limit: INDIRECT_HEADER_LIMIT,
                },
            })?;
        if actual.0 != id.0 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderMismatch { expected: id, actual },
            });
        }
        if actual.1 != id.1 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: id,
                    indexed: indexed_generation,
                    actual,
                },
            });
        }
        let body_offset = physical
            .checked_add(
                u64::try_from(header_bytes).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?,
            )
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        drop(header);
        let (object, mut object_charge) = self.parse_normal_scalar_limited(id, body_offset, source_len, permit)?;
        debug_assert!(self.index.encryption_state.is_none());
        let retained = u64::try_from(object_retained_bytes(&object)).unwrap_or(u64::MAX);
        if retained > object_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: retained,
                limit: object_charge.bytes(),
                phase: "measured-scalar",
            });
        }
        object_charge.shrink_to(retained);
        let peak = permit.stats().peak_bytes;
        Ok(BoundedScalar::new(object, retained, peak, object_charge))
    }

    fn parse_normal_scalar_limited(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(Object, ScalarCharge)> {
        let remaining = source_len.checked_sub(body_offset).ok_or(SourceError::OutOfBounds {
            offset: body_offset,
            length: 0,
            source_len,
        })?;
        let maximum = remaining.min(self.limits.max_object_bytes).min(permit.limit_bytes());
        let initial_length = maximum.min(INITIAL_OBJECT_WINDOW);
        let mut window = ChargedBytes::read(
            self.source.as_ref(),
            body_offset,
            initial_length,
            permit,
            id,
            "scalar-frame",
        )?;
        let mut object_framer = DirectObjectFramer::new();
        loop {
            match object_framer.advance(&window.bytes) {
                FrameStatus::Invalid => {
                    return Err(IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: body_offset,
                    });
                }
                FrameStatus::Ready => break,
                FrameStatus::NeedMore => {}
            }
            let current = u64::try_from(window.bytes.len())
                .map_err(|_| IndexedReaderError::ObjectLimitExceeded { id, limit: maximum })?;
            if current >= maximum {
                return Err(IndexedReaderError::ObjectLimitExceeded { id, limit: maximum });
            }
            let target = current
                .saturating_mul(2)
                .min(current.saturating_add(OBJECT_GROWTH_CHUNK))
                .min(maximum);
            window = ChargedBytes::grow_exact(
                window,
                self.source.as_ref(),
                body_offset,
                target,
                permit,
                id,
                "scalar-frame-growth",
            )?;
        }

        let ast_bound = conservative_ast_envelope(window.bytes.len());
        let ast_charge = permit.reserve(id, ast_bound, "scalar-ast-envelope")?;
        let parsed = parse_object_body(&window.bytes, id, body_offset)?;
        if parsed.stream_prefix.is_some() {
            return Err(IndexedReaderError::NotScalarObject { id });
        }
        drop(window);
        Ok((parsed.object, ast_charge))
    }

    fn resolve_compressed_scalar_limited(
        &self, id: crate::ObjectId, container: u32, index: u32, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        if self.index.encryption_state.is_some() {
            return Err(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "encrypted object streams",
            });
        }
        let container_id = (container, 0);
        let descriptor =
            self.resolve_stream_descriptor(container_id)
                .map_err(|error| IndexedReaderError::ObjectStreamMember {
                    id,
                    container: container_id,
                    index,
                    source: crate::Error::InvalidObjectStream(error.to_string()),
                })?;
        if descriptor.protection() != EncodedStreamProtection::Plain {
            return Err(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "protected object streams",
            });
        }
        if !limited_object_stream_filter_supported(descriptor.dictionary()) {
            return Err(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "object-stream filter chains or predictors outside plain/FlateDecode",
            });
        }
        let encoded_len = descriptor
            .encoded_len()
            .ok_or(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "object streams without a proven encoded length",
            })?;
        let dictionary_bytes = u64::try_from(dictionary_retained_bytes(&descriptor.dictionary)).unwrap_or(u64::MAX);
        let dictionary_charge = permit.reserve(id, dictionary_bytes, "object-stream-dictionary")?;
        let dictionary = descriptor.dictionary.clone();
        let first = dictionary
            .get(b"First")
            .and_then(Object::as_i64)
            .ok()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source: crate::Error::InvalidObjectStream("invalid object stream /First".into()),
            })?;
        let declared_members = dictionary
            .get(b"N")
            .and_then(Object::as_i64)
            .ok()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value <= crate::MAX_SELECTED_OBJECT_STREAM_MEMBERS)
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source: crate::Error::InvalidObjectStream("invalid object stream /N".into()),
            })?;
        let encoded_charge = permit.reserve(id, encoded_len, "object-stream-encoded")?;
        let encoded_usize = usize::try_from(encoded_len).map_err(|_| IndexedReaderError::ScalarResourceLimit {
            id,
            requested: encoded_len,
            limit: permit.limit_bytes(),
            phase: "object-stream-encoded",
        })?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(encoded_usize)
            .map_err(|_| SourceError::AllocationFailed { requested: encoded_len })?;
        encoded.resize(encoded_usize, 0);
        let mut encoded_reader =
            descriptor
                .open_plain_encoded()
                .map_err(|error| IndexedReaderError::ObjectStreamMember {
                    id,
                    container: container_id,
                    index,
                    source: crate::Error::InvalidObjectStream(error.to_string()),
                })?;
        let mut completed = 0;
        while completed < encoded.len() {
            let read = encoded_reader.read_chunk(&mut encoded[completed..]).map_err(|error| {
                IndexedReaderError::ObjectStreamMember {
                    id,
                    container: container_id,
                    index,
                    source: crate::Error::InvalidObjectStream(error.to_string()),
                }
            })?;
            if read == 0 {
                return Err(IndexedReaderError::ObjectStreamMember {
                    id,
                    container: container_id,
                    index,
                    source: crate::Error::InvalidObjectStream("truncated object stream".to_string()),
                });
            }
            completed += read;
        }
        let stream = Stream {
            dict: dictionary,
            content: encoded,
            allows_compression: true,
            start_position: None,
        };

        let current = permit.stats().current_bytes;
        let decode_envelope = permit.limit_bytes().saturating_sub(current);
        const DECODER_FIXED_BYTES: u64 = 128 * 1024;
        if decode_envelope <= DECODER_FIXED_BYTES {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: current.saturating_add(DECODER_FIXED_BYTES).saturating_add(1),
                limit: permit.limit_bytes(),
                phase: "object-stream-decode-envelope",
            });
        }
        let mut decoded_charge = permit.reserve(id, decode_envelope, "object-stream-decode-envelope")?;
        let decoded_limit = usize::try_from((decode_envelope - DECODER_FIXED_BYTES) / 4).unwrap_or(usize::MAX);
        let decoded = stream
            .decompressed_content_with_limit(decoded_limit)
            .map_err(|source| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source,
            })?;
        let decoded_capacity = u64::try_from(decoded.capacity()).unwrap_or(u64::MAX);
        if decoded_capacity > decode_envelope {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: current.saturating_add(decoded_capacity),
                limit: permit.limit_bytes(),
                phase: "object-stream-decoded-capacity",
            });
        }
        drop(stream);
        drop(encoded_charge);
        drop(dictionary_charge);
        decoded_charge.shrink_to(decoded_capacity);

        let member = selected_object_stream_member(&decoded, first, declared_members, id, index).map_err(|source| {
            IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source,
            }
        })?;
        let ast_bound = conservative_ast_envelope(member.len());
        let mut ast_charge = permit.reserve(id, ast_bound, "object-stream-member-ast")?;
        let object = crate::parser::direct_object(member).ok_or_else(|| IndexedReaderError::ObjectStreamMember {
            id,
            container: container_id,
            index,
            source: crate::Error::InvalidObjectStream(
                "selected object stream member is truncated or invalid".to_string(),
            ),
        })?;
        drop(decoded);
        drop(decoded_charge);
        let retained = u64::try_from(object_retained_bytes(&object)).unwrap_or(u64::MAX);
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
        let metadata = (|| {
            let (body_offset, source_len, parsed) = self.resolve_normal_framed(id)?;
            self.finish_stream_metadata(id, body_offset, source_len, parsed, &mut state)
        })();
        self.ensure_stream_source_len()?;
        let metadata = metadata?;
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

    /// Resolve one full object id into a shareable owned value.
    ///
    /// Readers opened by the legacy constructors do not retain this value:
    /// the `Arc` only makes the returned ownership shareable. When a cached
    /// constructor configures the bounded resolver caches, concurrent calls
    /// for the same id are single-flighted and successful or deterministic
    /// failing results remain reusable until eviction. Source I/O failures are
    /// shared by calls already waiting on that cell, then discarded so a later
    /// operation can retry.
    pub fn resolve_object_shared(&self, id: crate::ObjectId) -> SharedIndexedReaderResult<Arc<Object>> {
        let Some(cache) = &self.object_cache else {
            let mut state = ResolutionState::default();
            return self.resolve_inner(id, &mut state).map(Arc::new).map_err(Arc::new);
        };
        cache.resolve(id, || self.resolve_object_shared_uncached(id), object_retained_bytes)
    }

    fn resolve_object_shared_uncached(&self, id: crate::ObjectId) -> SharedIndexedReaderResult<Arc<Object>> {
        let mut state = ResolutionState::default();
        match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Compressed { container, index }) if self.object_stream_cache.is_some() => {
                self.resolve_compressed_shared(id, container, index, &mut state)
            }
            _ => self.resolve_inner(id, &mut state).map(Arc::new).map_err(Arc::new),
        }
    }

    /// Resolve each unique requested id into a shareable owned value.
    ///
    /// Results retain the first-occurrence order of `ids`; duplicate ids are
    /// resolved once and omitted from later positions. Independent ordinary
    /// objects may complete out of order internally. Compressed requests are
    /// grouped by object-stream container so each successful container is
    /// resolved, decoded and header-indexed once for this call.
    pub fn resolve_many_shared(
        &self, ids: &[crate::ObjectId],
    ) -> Vec<(crate::ObjectId, SharedIndexedReaderResult<Arc<Object>>)> {
        let mut seen = HashSet::with_capacity(ids.len());
        let unique: Vec<_> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        let mut normal = Vec::new();
        let mut compressed: BTreeMap<u32, Vec<CompressedBatchRequest>> = BTreeMap::new();

        for (position, id) in unique.iter().copied().enumerate() {
            match self.index.locations.get(&id.0) {
                Some(ObjectLocation64::Compressed { container, index }) => {
                    compressed.entry(*container).or_default().push(CompressedBatchRequest {
                        position,
                        id,
                        index: *index,
                    });
                }
                _ => normal.push((position, id)),
            }
        }

        #[cfg(feature = "rayon")]
        let normal_results: Vec<_> = normal
            .into_par_iter()
            .map(|(position, id)| (position, self.resolve_object_shared(id)))
            .collect();
        #[cfg(not(feature = "rayon"))]
        let normal_results: Vec<_> = normal
            .into_iter()
            .map(|(position, id)| (position, self.resolve_object_shared(id)))
            .collect();

        let mut results: Vec<Option<SharedIndexedReaderResult<Arc<Object>>>> =
            std::iter::repeat_with(|| None).take(unique.len()).collect();
        for (position, result) in normal_results {
            results[position] = Some(result);
        }
        for (container, requests) in compressed {
            for (position, result) in self.resolve_compressed_group(container, &requests) {
                results[position] = Some(result);
            }
        }

        unique
            .into_iter()
            .zip(results)
            .map(|(id, result)| (id, result.expect("every unique batch id is classified")))
            .collect()
    }

    /// Derive the actual ordered leaf-page map by walking `/Kids`.
    pub fn page_map(&self) -> IndexedReaderResult<PageMap> {
        PageMap::from_reader(self)
    }

    /// PDF header version, for example `"1.7"`.
    pub fn version(&self) -> &str {
        &self.index.version
    }

    /// Stable source length captured while the index was opened.
    pub fn source_len(&self) -> u64 {
        self.index.source_len
    }

    /// Whether the trailer declares an encryption dictionary.
    pub fn is_encrypted(&self) -> bool {
        self.index.trailer.has(b"Encrypt")
    }

    /// Whether an encrypted document has an authenticated decryption state.
    pub fn is_authenticated(&self) -> bool {
        self.index.encryption_state.is_some()
    }

    /// Derive and count actual leaf pages without trusting `/Count`.
    pub fn page_count(&self) -> IndexedReaderResult<usize> {
        self.page_map().map(|pages| pages.len())
    }

    /// Snapshot counters and residency for the opt-in shared object cache.
    pub fn object_cache_stats(&self) -> IndexedObjectCacheStats {
        let Some(counters) = self.cache_counters.as_ref() else {
            return IndexedObjectCacheStats::default();
        };
        let (probation_entries, probation_bytes, protected_entries, protected_bytes) =
            self.object_cache.as_ref().map_or((0, 0, 0, 0), SharedCache::residency);
        IndexedObjectCacheStats {
            object_hits: counters.object_hits.load(Ordering::Relaxed),
            object_misses: counters.object_misses.load(Ordering::Relaxed),
            object_waits: counters.object_waits.load(Ordering::Relaxed),
            object_loads: counters.object_loads.load(Ordering::Relaxed),
            object_promotions: counters.object_promotions.load(Ordering::Relaxed),
            object_evictions: counters.object_evictions.load(Ordering::Relaxed),
            object_bypasses: counters.object_bypasses.load(Ordering::Relaxed),
            negative_hits: counters.negative_hits.load(Ordering::Relaxed),
            transient_failures: counters.object_transient_failures.load(Ordering::Relaxed),
            probation_entries,
            probation_bytes,
            protected_entries,
            protected_bytes,
            peak_entries: counters.object_peak_entries.load(Ordering::Relaxed),
            peak_bytes: counters.object_peak_bytes.load(Ordering::Relaxed),
        }
    }

    /// Snapshot counters and residency for the opt-in decoded object-stream cache.
    pub fn object_stream_cache_stats(&self) -> IndexedObjectStreamCacheStats {
        let Some(counters) = self.cache_counters.as_ref() else {
            return IndexedObjectStreamCacheStats::default();
        };
        let (entries, bytes) = self
            .object_stream_cache
            .as_ref()
            .map(|cache| {
                let (probation_entries, probation_bytes, protected_entries, protected_bytes) = cache.residency();
                (
                    probation_entries.saturating_add(protected_entries),
                    probation_bytes.saturating_add(protected_bytes),
                )
            })
            .unwrap_or((0, 0));
        IndexedObjectStreamCacheStats {
            hits: counters.objstm_hits.load(Ordering::Relaxed),
            misses: counters.objstm_misses.load(Ordering::Relaxed),
            waits: counters.objstm_waits.load(Ordering::Relaxed),
            loads: counters.objstm_loads.load(Ordering::Relaxed),
            evictions: counters.objstm_evictions.load(Ordering::Relaxed),
            bypasses: counters.objstm_bypasses.load(Ordering::Relaxed),
            transient_failures: counters.objstm_transient_failures.load(Ordering::Relaxed),
            entries,
            bytes,
            peak_entries: counters.objstm_peak_entries.load(Ordering::Relaxed),
            peak_bytes: counters.objstm_peak_bytes.load(Ordering::Relaxed),
        }
    }

    #[allow(dead_code)] // Called by the cached constructors in the integration commit.
    pub(crate) fn configure_resolution_caches(
        &mut self, object_bytes: usize, object_entries: usize, object_stream_bytes: usize, object_stream_entries: usize,
    ) {
        let counters = Arc::new(CacheCounters::default());
        self.cache_counters = Some(Arc::clone(&counters));
        self.object_cache = (object_bytes > 0 && object_entries > 0).then(|| {
            SharedCache::new(
                object_bytes,
                object_entries,
                // A new item enters probation; do not allow one item to
                // consume more than the entire one-quarter probation budget.
                object_bytes.saturating_mul(100 - SHARED_OBJECT_PROTECTED_PERCENT) / 100,
                SHARED_OBJECT_PROTECTED_PERCENT,
                CacheKind::Object,
                Arc::clone(&counters),
            )
        });
        self.object_stream_cache = (object_stream_bytes > 0 && object_stream_entries > 0).then(|| {
            SharedCache::new(
                object_stream_bytes,
                object_stream_entries,
                object_stream_bytes,
                0,
                CacheKind::ObjectStream,
                counters,
            )
        });
    }

    fn resolve_dictionary_deref(&self, id: crate::ObjectId) -> IndexedReaderResult<Option<Dictionary>> {
        let Some(object) = self.resolve_page_tree_object(id)? else {
            return Ok(None);
        };
        Ok(match self.resolve_deref_value(object)? {
            Some(Object::Dictionary(dictionary)) => Some(dictionary),
            _ => None,
        })
    }

    fn resolve_array_value(&self, value: Option<Object>) -> IndexedReaderResult<Option<Vec<Object>>> {
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(match self.resolve_deref_value(value)? {
            Some(Object::Array(array)) => Some(array),
            _ => None,
        })
    }

    fn resolve_deref_value(&self, mut object: Object) -> IndexedReaderResult<Option<Object>> {
        let mut seen = HashSet::new();
        let mut dereferences = 0;
        while let Object::Reference(id) = object {
            if dereferences >= PAGE_TREE_DEREFERENCE_LIMIT || !seen.insert(id) {
                return Ok(None);
            }
            let Some(resolved) = self.resolve_page_tree_object(id)? else {
                return Ok(None);
            };
            object = resolved;
            dereferences += 1;
        }
        Ok(Some(object))
    }

    fn resolve_page_tree_object(&self, id: crate::ObjectId) -> IndexedReaderResult<Option<Object>> {
        match self.resolve_object(id) {
            Ok(object) => Ok(Some(object)),
            Err(
                IndexedReaderError::MissingNormalObject { .. } | IndexedReaderError::MissingNormalObjectAtXref { .. },
            ) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn resolve_inner(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal(id, state),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed(id, container, index, state)
            }
            Some(ObjectLocation64::Free { .. }) | None => Err(IndexedReaderError::MissingNormalObject { id }),
        };
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed(
        &self, id: crate::ObjectId, container: u32, index: u32, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Object> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        let container = (container, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(container) {
            return Err(IndexedReaderError::ResolutionCycle { id: container });
        }
        state.depth += 1;
        // Object streams must themselves be ordinary, generation-zero indirect
        // objects. Do not recursively accept a compressed container here.
        let resolved = self.resolve_normal(container, state);
        state.depth -= 1;
        state.active.remove(&container);

        let object = resolved?;
        let Object::Stream(stream) = object else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream { id, container });
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        ObjectStream::parse_selected_member_with_limit(&stream, id, index, Some(limit)).map_err(|source| {
            IndexedReaderError::ObjectStreamMember {
                id,
                container,
                index,
                source,
            }
        })
    }

    fn resolve_compressed_shared(
        &self, id: crate::ObjectId, container_number: u32, index: u32, state: &mut ResolutionState,
    ) -> SharedIndexedReaderResult<Arc<Object>> {
        if state.depth >= self.limits.max_length_depth {
            return Err(Arc::new(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            }));
        }
        if !state.active.insert(id) {
            return Err(Arc::new(IndexedReaderError::ResolutionCycle { id }));
        }
        state.depth += 1;
        let result = self.resolve_compressed_shared_inner(id, container_number, index, state);
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed_shared_inner(
        &self, id: crate::ObjectId, container_number: u32, index: u32, state: &mut ResolutionState,
    ) -> SharedIndexedReaderResult<Arc<Object>> {
        if id.1 != 0 {
            return Err(Arc::new(IndexedReaderError::GenerationMismatch { id, indexed: 0 }));
        }
        let container = (container_number, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(Arc::new(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            }));
        }
        if !state.active.insert(container) {
            return Err(Arc::new(IndexedReaderError::ResolutionCycle { id: container }));
        }
        state.depth += 1;
        let cache = self
            .object_stream_cache
            .as_ref()
            .expect("shared compressed resolution requires its configured cache");
        let prepared = cache.resolve(
            container,
            || {
                let prepared = match self.resolve_normal(container, state) {
                    Ok(Object::Stream(stream)) => {
                        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
                        match ObjectStream::selected_members_with_limit(&stream, Some(limit)) {
                            Ok(selected) => PreparedObjectStream::Selected(selected),
                            Err(_) => PreparedObjectStream::Raw(stream),
                        }
                    }
                    Ok(_) => PreparedObjectStream::NotStream,
                    Err(error) => return Err(Arc::new(error)),
                };
                Ok(Arc::new(prepared))
            },
            PreparedObjectStream::retained_bytes,
        )?;
        state.depth -= 1;
        state.active.remove(&container);

        match prepared.as_ref() {
            PreparedObjectStream::Selected(selected) => {
                selected.parse_member(id, index).map(Arc::new).map_err(|source| {
                    Arc::new(IndexedReaderError::ObjectStreamMember {
                        id,
                        container,
                        index,
                        source,
                    })
                })
            }
            PreparedObjectStream::Raw(stream) => {
                let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
                ObjectStream::parse_selected_member_with_limit(stream, id, index, Some(limit))
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id,
                            container,
                            index,
                            source,
                        })
                    })
            }
            PreparedObjectStream::NotStream => Err(Arc::new(IndexedReaderError::ObjectStreamContainerNotStream {
                id,
                container,
            })),
        }
    }

    fn resolve_compressed_group(
        &self, container_number: u32, requests: &[CompressedBatchRequest],
    ) -> Vec<(usize, SharedIndexedReaderResult<Arc<Object>>)> {
        // Very small depth limits make the active root id observable. Preserve
        // exact scalar behavior rather than sharing container setup there.
        if self.limits.max_length_depth < 2 {
            return requests
                .iter()
                .map(|request| (request.position, self.resolve_object_shared(request.id)))
                .collect();
        }

        let mut results = Vec::with_capacity(requests.len());
        let valid: Vec<_> = requests
            .iter()
            .filter(|request| {
                if request.id.1 == 0 {
                    true
                } else {
                    results.push((
                        request.position,
                        Err(Arc::new(IndexedReaderError::GenerationMismatch {
                            id: request.id,
                            indexed: 0,
                        })),
                    ));
                    false
                }
            })
            .collect();
        if valid.is_empty() {
            return results;
        }

        let container = (container_number, 0);
        let mut state = ResolutionState {
            active: HashSet::from([container]),
            // Reserve the same target-object and container depths used by the
            // scalar compressed path.
            depth: 2,
        };
        let object = match self.resolve_normal(container, &mut state) {
            Ok(object) => object,
            Err(error) => {
                let error = Arc::new(error);
                results.extend(
                    valid
                        .into_iter()
                        .map(|request| (request.position, Err(Arc::clone(&error)))),
                );
                return results;
            }
        };
        let Object::Stream(stream) = object else {
            results.extend(valid.into_iter().map(|request| {
                (
                    request.position,
                    Err(Arc::new(IndexedReaderError::ObjectStreamContainerNotStream {
                        id: request.id,
                        container,
                    })),
                )
            }));
            return results;
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        let selected = match ObjectStream::selected_members_with_limit(&stream, Some(limit)) {
            Ok(selected) => selected,
            Err(source) => {
                let error = Arc::new(IndexedReaderError::ObjectStreamBatchSetup { container, source });
                results.extend(
                    valid
                        .into_iter()
                        .map(|request| (request.position, Err(Arc::clone(&error)))),
                );
                return results;
            }
        };

        #[cfg(feature = "rayon")]
        let parsed: Vec<_> = valid
            .into_par_iter()
            .map(|request| {
                let result = selected
                    .parse_member(request.id, request.index)
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id: request.id,
                            container,
                            index: request.index,
                            source,
                        })
                    });
                (request.position, result)
            })
            .collect();
        #[cfg(not(feature = "rayon"))]
        let parsed: Vec<_> = valid
            .into_iter()
            .map(|request| {
                let result = selected
                    .parse_member(request.id, request.index)
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id: request.id,
                            container,
                            index: request.index,
                            source,
                        })
                    });
                (request.position, result)
            })
            .collect();
        results.extend(parsed);
        results
    }

    fn resolve_normal(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        let mut object = self.resolve_normal_plain(id, state)?;
        if self.index.encrypt_object_id != Some(id)
            && let Some(encryption_state) = &self.index.encryption_state
        {
            encryption::decrypt_object(encryption_state, id, &mut object)
                .map_err(|source| IndexedReaderError::ObjectDecryption { id, source })?;
        }
        Ok(object)
    }

    fn resolve_normal_plain(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        let (body_offset, source_len, parsed) = self.resolve_normal_framed(id)?;
        self.finish_object(id, body_offset, source_len, parsed, state)
    }

    fn resolve_normal_framed(&self, id: crate::ObjectId) -> IndexedReaderResult<(u64, u64, ParsedObject)> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexedReaderError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header = read_window(self.source.as_ref(), source_len, physical, INDIRECT_HEADER_LIMIT)?;
        let (actual, header_bytes) =
            parse_indirect_header(&header).ok_or(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit {
                    offset,
                    limit: INDIRECT_HEADER_LIMIT,
                },
            })?;
        if actual.0 != id.0 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderMismatch { expected: id, actual },
            });
        }
        if actual.1 != id.1 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: id,
                    indexed: indexed_generation,
                    actual,
                },
            });
        }
        let header_bytes =
            u64::try_from(header_bytes).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let body_offset = physical
            .checked_add(header_bytes)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let parsed = self.resolve_body_frame(id, body_offset, source_len)?;
        Ok((body_offset, source_len, parsed))
    }

    fn initialize_encryption(&mut self, password: Option<&[u8]>) -> IndexedReaderResult<()> {
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
                .map_err(|_| IndexedReaderError::InvalidEncryptDictionary)?
                .clone();
            (dictionary, Some(id))
        } else {
            return Err(IndexedReaderError::InvalidEncryptDictionary);
        };
        let file_id = self
            .index
            .trailer
            .get(b"ID")
            .ok()
            .and_then(|id| id.as_array().ok())
            .and_then(|ids| ids.first())
            .and_then(|id| id.as_str().ok());
        let algorithm = PasswordAlgorithm::try_from(&dictionary).map_err(IndexedReaderError::Encryption)?;

        let selected = if let Some(selected) = authenticate_password(&algorithm, file_id, b"")? {
            selected
        } else if let Some(password) = password {
            authenticate_password(&algorithm, file_id, password)?.ok_or(IndexedReaderError::InvalidPassword)?
        } else {
            return Err(IndexedReaderError::PasswordRequired);
        };
        let state = EncryptionState::decode_from_dictionary(&dictionary, file_id, &selected)
            .map_err(IndexedReaderError::Encryption)?;
        let index = Arc::get_mut(&mut self.index).expect("index is not shared during reader construction");
        index.encryption_state = Some(state);
        index.encrypt_object_id = encrypt_object_id;
        Ok(())
    }

    fn resolve_body_frame(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64,
    ) -> IndexedReaderResult<ParsedObject> {
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
                return Err(IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                });
            }
            if frame_status == FrameStatus::Ready {
                return parse_object_body(&window, id, body_offset);
            }

            let current = u64::try_from(window.len()).map_err(|_| IndexedReaderError::ObjectLimitExceeded {
                id,
                limit: self.limits.max_object_bytes,
            })?;
            if current >= maximum {
                return if remaining > self.limits.max_object_bytes {
                    Err(IndexedReaderError::ObjectLimitExceeded {
                        id,
                        limit: self.limits.max_object_bytes,
                    })
                } else {
                    Err(IndexedReaderError::IncompleteObject {
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
            let extension_offset =
                body_offset
                    .checked_add(current)
                    .ok_or(IndexedReaderError::InvalidIndirectObject {
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
    ) -> IndexedReaderResult<Object> {
        match self.finish_stream_metadata(id, body_offset, source_len, parsed, state)? {
            FramedStreamMetadata::Scalar(object) => Ok(object),
            FramedStreamMetadata::MissingLength {
                dictionary,
                encoded_start,
            } => {
                let relative_stream_start = encoded_start.checked_sub(self.index.source_origin).ok_or(
                    IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: encoded_start,
                    },
                )?;
                let stream_start =
                    usize::try_from(relative_stream_start).map_err(|_| IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: encoded_start,
                    })?;
                Ok(Object::Stream(Stream::with_position(dictionary, stream_start)))
            }
            FramedStreamMetadata::Span {
                dictionary,
                encoded_start,
                encoded_len,
            } => {
                let content = self
                    .source
                    .read_range(encoded_start, encoded_len, self.limits.max_stream_bytes)?;
                Ok(Object::Stream(Stream::new(dictionary, content)))
            }
        }
    }

    fn finish_stream_metadata(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, parsed: ParsedObject,
        state: &mut ResolutionState,
    ) -> IndexedReaderResult<FramedStreamMetadata> {
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Ok(FramedStreamMetadata::Scalar(object));
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            });
        };

        let stream_start = body_offset
            .checked_add(
                u64::try_from(consumed).map_err(|_| IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                })?,
            )
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?;
        let Some(length) = self.resolve_stream_length(&dictionary, state)? else {
            // This matches the eager loader's degradation for a missing,
            // malformed, dangling, cyclic, or over-depth /Length: retain the
            // stream dictionary and expose empty owned content.
            return Ok(FramedStreamMetadata::MissingLength {
                dictionary,
                encoded_start: stream_start,
            });
        };
        if length < 0 {
            return Err(IndexedReaderError::NegativeStreamLength { id, length });
        }
        let length = u64::try_from(length).map_err(|_| IndexedReaderError::NegativeStreamLength { id, length })?;
        if length > self.limits.max_stream_bytes {
            return Err(IndexedReaderError::StreamLimitExceeded {
                id,
                length,
                limit: self.limits.max_stream_bytes,
            });
        }

        let stream_end = stream_start
            .checked_add(length)
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: stream_start,
            })?;
        if stream_end > source_len {
            return Ok(FramedStreamMetadata::Scalar(Object::Dictionary(dictionary)));
        }
        match validate_endstream(
            self.source.as_ref(),
            source_len,
            stream_end,
            self.limits.max_endstream_tail_bytes,
        )? {
            EndstreamStatus::Found => {}
            EndstreamStatus::Missing => {
                return Ok(FramedStreamMetadata::Scalar(Object::Dictionary(dictionary)));
            }
            EndstreamStatus::LimitExceeded => return Err(IndexedReaderError::MissingEndstream { id }),
        }
        Ok(FramedStreamMetadata::Span {
            dictionary,
            encoded_start: stream_start,
            encoded_len: length,
        })
    }

    fn resolve_stream_length(
        &self, dictionary: &Dictionary, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Option<i64>> {
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

    fn resolve_length_reference(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<i64> {
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = self.resolve_normal(id, state).and_then(|object| match object {
            Object::Integer(value) => Ok(value),
            Object::Reference(next) => self.resolve_length_reference(next, state),
            _ => Err(IndexedReaderError::InvalidIndirectObject { id, offset: 0 }),
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

#[derive(Clone, Copy)]
struct CompressedBatchRequest {
    position: usize,
    id: crate::ObjectId,
    index: u32,
}

fn authenticate_password(
    algorithm: &PasswordAlgorithm, file_id: Option<&[u8]>, password: &[u8],
) -> IndexedReaderResult<Option<Vec<u8>>> {
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
                Err(IndexedReaderError::Encryption(crate::Error::Decryption(owner_error)))
            }
            Err(user_error) => Err(IndexedReaderError::Encryption(crate::Error::Decryption(user_error))),
            Ok(()) => unreachable!(),
        },
    }
}

struct ParsedObject {
    object: Object,
    consumed: usize,
    stream_prefix: Option<u64>,
}

enum FramedStreamMetadata {
    Scalar(Object),
    MissingLength {
        dictionary: Dictionary,
        encoded_start: u64,
    },
    Span {
        dictionary: Dictionary,
        encoded_start: u64,
        encoded_len: u64,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FilterProtection {
    Plain,
    Crypt,
}

impl PdfIndex {
    pub(crate) fn open(source: Arc<dyn RandomAccessSource>) -> IndexedReaderResult<Self> {
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
            revisions = revisions
                .checked_add(1)
                .ok_or(IndexedReaderError::RevisionLimitExceeded {
                    limit: MAX_XREF_REVISIONS,
                })?;
            if revisions > MAX_XREF_REVISIONS {
                return Err(IndexedReaderError::RevisionLimitExceeded {
                    limit: MAX_XREF_REVISIONS,
                });
            }

            let physical = source_origin
                .checked_add(offset)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let section = read_xref_section(source.as_ref(), source_len, physical)?;
            if newest_trailer.is_none() {
                newest_type = Some(section.kind);
                newest_trailer = Some(section.trailer.clone());
                declared_size =
                    trailer_unsigned(&section.trailer, b"Size").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
            }
            merge_newest(&mut locations, section.entries);

            // A hybrid-reference table supplements its own revision. Its
            // entries fill holes but never replace entries from that revision.
            if let Some(hybrid) = trailer_offset(&section.trailer, b"XRefStm", "XRefStm")? {
                let hybrid_physical = source_origin
                    .checked_add(hybrid)
                    .ok_or(IndexedReaderError::InvalidTrailerOffset { key: "XRefStm" })?;
                let supplement = read_xref_section(source.as_ref(), source_len, hybrid_physical)?;
                if supplement.kind != IndexXrefType::Stream {
                    return Err(IndexedReaderError::InvalidXref { offset: hybrid });
                }
                merge_newest(&mut locations, supplement.entries);
            }

            next = trailer_offset(&section.trailer, b"Prev", "Prev")?;
        }

        let trailer = newest_trailer.ok_or(IndexedReaderError::InvalidXref { offset: xref_start })?;
        Ok(Self {
            version,
            source_len,
            source_origin,
            xref_start,
            xref_type: newest_type.ok_or(IndexedReaderError::InvalidXref { offset: xref_start })?,
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

fn read_header(source: &dyn RandomAccessSource, source_len: u64) -> IndexedReaderResult<(u64, String)> {
    let read_limit = HEADER_SCAN_LIMIT
        .checked_add(HEADER_PARSE_OVERLAP)
        .ok_or(IndexedReaderError::InvalidHeader {
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
        .ok_or(IndexedReaderError::InvalidHeader {
            limit: HEADER_SCAN_LIMIT,
        })?;
    let version = crate::parser::header(&bytes[origin..], false).ok_or(IndexedReaderError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    let origin = u64::try_from(origin).map_err(|_| IndexedReaderError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    Ok((origin, version))
}

fn read_startxref(source: &dyn RandomAccessSource, source_len: u64) -> IndexedReaderResult<u64> {
    let length = source_len.min(TAIL_SCAN_LIMIT);
    let offset = source_len
        .checked_sub(length)
        .ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let tail = source.read_range(offset, length, TAIL_SCAN_LIMIT)?;
    let eof = rfind(&tail, b"%%EOF").ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let marker =
        rfind(&tail[..eof], b"startxref").ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let mut cursor = TokenCursor::new(&tail[marker + b"startxref".len()..]);
    cursor
        .unsigned()
        .ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })
}

fn read_xref_section(
    source: &dyn RandomAccessSource, source_len: u64, physical_offset: u64,
) -> IndexedReaderResult<XrefSection64> {
    if physical_offset >= source_len {
        return Err(IndexedReaderError::InvalidXref {
            offset: physical_offset,
        });
    }
    let remaining = source_len
        .checked_sub(physical_offset)
        .ok_or(IndexedReaderError::InvalidXref {
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
            Err(IndexedReaderError::IncompleteXref { .. }) if length < maximum => {
                length = length.saturating_mul(2).min(maximum);
            }
            Err(IndexedReaderError::IncompleteXref { .. }) if remaining > XREF_WINDOW_LIMIT => {
                return Err(IndexedReaderError::StructureLimitExceeded {
                    structure: "cross-reference section",
                    limit: XREF_WINDOW_LIMIT,
                });
            }
            Err(IndexedReaderError::IncompleteXref { .. }) => {
                return Err(IndexedReaderError::InvalidXref {
                    offset: physical_offset,
                });
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_classic_xref(window: &[u8], offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_token(&mut cursor, b"xref", offset)?;
    let mut entries = BTreeMap::new();
    let mut entry_count = 0_u64;

    loop {
        cursor.skip_space();
        if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"trailer".len() && b"trailer".starts_with(cursor.remaining()))
        {
            return Err(IndexedReaderError::IncompleteXref { offset });
        }
        if cursor.consume(b"trailer") {
            let trailer = match cursor.direct_object() {
                Some(Object::Dictionary(dictionary)) => dictionary,
                Some(_) => return Err(IndexedReaderError::InvalidTrailer { offset }),
                None if dictionary_may_be_truncated(cursor.remaining()) => {
                    return Err(IndexedReaderError::IncompleteXref { offset });
                }
                None => return Err(IndexedReaderError::InvalidTrailer { offset }),
            };
            return Ok(XrefSection64 {
                kind: IndexXrefType::Table,
                entries,
                trailer,
            });
        }

        let start = required_xref_unsigned(&mut cursor, offset)?;
        let count = required_xref_unsigned(&mut cursor, offset)?;
        entry_count = entry_count
            .checked_add(count)
            .ok_or(IndexedReaderError::EntryLimitExceeded {
                count: u64::MAX,
                limit: MAX_XREF_ENTRIES,
            })?;
        check_entry_limit(entry_count)?;

        for index in 0..count {
            let object_number = start
                .checked_add(index)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            let field = required_xref_unsigned(&mut cursor, offset)?;
            let generation = required_xref_unsigned(&mut cursor, offset)?;
            let generation = u16::try_from(generation).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            cursor.skip_space();
            if cursor.remaining().is_empty() {
                return Err(IndexedReaderError::IncompleteXref { offset });
            }
            let state = cursor.token().ok_or(IndexedReaderError::InvalidXref { offset })?;
            let location = match state {
                b"n" => ObjectLocation64::Normal {
                    offset: field,
                    generation,
                },
                b"f" => ObjectLocation64::Free {
                    next: field,
                    generation,
                },
                _ => return Err(IndexedReaderError::InvalidXref { offset }),
            };
            entries.insert(object_number, location);
        }
    }
}

fn parse_xref_stream(window: &[u8], offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_token(&mut cursor, b"obj", offset)?;
    let dictionary = match cursor.direct_object() {
        Some(Object::Dictionary(dictionary)) => dictionary,
        Some(_) => return Err(IndexedReaderError::InvalidTrailer { offset }),
        None if dictionary_may_be_truncated(cursor.remaining()) => {
            return Err(IndexedReaderError::IncompleteXref { offset });
        }
        None => return Err(IndexedReaderError::InvalidTrailer { offset }),
    };
    required_xref_token(&mut cursor, b"stream", offset)?;
    if cursor.consume_stream_eol().is_none() {
        return if cursor.remaining().is_empty() {
            Err(IndexedReaderError::IncompleteXref { offset })
        } else {
            Err(IndexedReaderError::InvalidXref { offset })
        };
    }

    let stream_len = trailer_unsigned(&dictionary, b"Length").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
    let stream_len = usize::try_from(stream_len).map_err(|_| IndexedReaderError::StructureLimitExceeded {
        structure: "cross-reference stream",
        limit: XREF_WINDOW_LIMIT,
    })?;
    if stream_len > cursor.remaining().len() {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    let content = cursor
        .take(stream_len)
        .ok_or(IndexedReaderError::IncompleteXref { offset })?
        .to_vec();
    cursor.consume_optional_eol();
    if !cursor.consume_exact(b"endstream") {
        return if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"endstream".len() && b"endstream".starts_with(cursor.remaining()))
        {
            Err(IndexedReaderError::IncompleteXref { offset })
        } else {
            Err(IndexedReaderError::InvalidXref { offset })
        };
    }
    let mut stream = Stream::new(dictionary.clone(), content);
    if stream.is_compressed() {
        stream
            .decompress_with_limit(XREF_DECOMPRESSED_LIMIT)
            .map_err(IndexedReaderError::XrefDecompression)?;
    }
    decode_xref_stream64(stream, offset)
}

fn decode_xref_stream64(stream: Stream, offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut trailer = stream.dict;
    let size = trailer_unsigned(&trailer, b"Size").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
    let widths = integer_array(&trailer, b"W").ok_or(IndexedReaderError::InvalidXref { offset })?;
    if widths.len() < 3 || widths[..3].iter().any(|width| *width > MAX_XREF_FIELD_WIDTH) {
        return Err(IndexedReaderError::InvalidXref { offset });
    }
    let indices = integer_array(&trailer, b"Index").unwrap_or_else(|| vec![0, size]);
    if !indices.chunks_exact(2).remainder().is_empty() {
        return Err(IndexedReaderError::InvalidXref { offset });
    }

    let mut total = 0_u64;
    for pair in indices.chunks_exact(2) {
        total = total
            .checked_add(pair[1])
            .ok_or(IndexedReaderError::EntryLimitExceeded {
                count: u64::MAX,
                limit: MAX_XREF_ENTRIES,
            })?;
    }
    check_entry_limit(total)?;

    let entry_width = widths[..3]
        .iter()
        .try_fold(0_u64, |sum, width| sum.checked_add(*width))
        .ok_or(IndexedReaderError::InvalidXref { offset })?;
    let required = total
        .checked_mul(entry_width)
        .ok_or(IndexedReaderError::InvalidXref { offset })?;
    let content_len = u64::try_from(stream.content.len()).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
    if required > content_len {
        return Err(IndexedReaderError::InvalidXref { offset });
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
            let object_number = start
                .checked_add(index)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            let location = match kind {
                0 => ObjectLocation64::Free {
                    next: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                },
                1 => ObjectLocation64::Normal {
                    offset: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                },
                2 => ObjectLocation64::Compressed {
                    container: u32::try_from(field2).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                    index: u32::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
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

fn read_be(input: &mut &[u8], width: u64, offset: u64) -> IndexedReaderResult<u64> {
    let width = usize::try_from(width).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
    let bytes = input.get(..width).ok_or(IndexedReaderError::InvalidXref { offset })?;
    *input = input.get(width..).ok_or(IndexedReaderError::InvalidXref { offset })?;
    Ok(bytes.iter().fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn check_entry_limit(count: u64) -> IndexedReaderResult<()> {
    if count > MAX_XREF_ENTRIES {
        return Err(IndexedReaderError::EntryLimitExceeded {
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

fn trailer_offset(dictionary: &Dictionary, key: &[u8], name: &'static str) -> IndexedReaderResult<Option<u64>> {
    match dictionary.get(key) {
        Ok(object) => object
            .as_i64()
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .map(Some)
            .ok_or(IndexedReaderError::InvalidTrailerOffset { key: name }),
        Err(_) => Ok(None),
    }
}

fn required_xref_token(cursor: &mut TokenCursor<'_>, expected: &[u8], offset: u64) -> IndexedReaderResult<()> {
    cursor.skip_space();
    if cursor.remaining().is_empty()
        || (cursor.remaining().len() < expected.len() && expected.starts_with(cursor.remaining()))
    {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    cursor
        .expect(expected)
        .ok_or(IndexedReaderError::InvalidXref { offset })
}

fn required_xref_unsigned(cursor: &mut TokenCursor<'_>, offset: u64) -> IndexedReaderResult<u64> {
    cursor.skip_space();
    if cursor.remaining().is_empty() {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    let token = cursor.token().ok_or(IndexedReaderError::InvalidXref { offset })?;
    if !token.iter().all(u8::is_ascii_digit) {
        return Err(IndexedReaderError::InvalidXref { offset });
    }
    std::str::from_utf8(token)
        .ok()
        .and_then(|token| token.parse().ok())
        .ok_or(IndexedReaderError::InvalidXref { offset })
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

struct ChargedBytes {
    bytes: Box<[u8]>,
    _charge: ScalarCharge,
}

impl ChargedBytes {
    fn read(
        source: &dyn RandomAccessSource, offset: u64, length: u64, permit: &ScalarResolutionPermit,
        id: crate::ObjectId, phase: &'static str,
    ) -> IndexedReaderResult<Self> {
        let charge = permit.reserve(id, length, phase)?;
        let bytes = source.read_range(offset, length, length)?.into_boxed_slice();
        Ok(Self { bytes, _charge: charge })
    }

    fn grow_exact(
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

fn conservative_ast_envelope(framed_bytes: usize) -> u64 {
    // One input byte can introduce at most one token/container transition.
    // 256 bytes per input byte covers two-times Vec growth, `Object` array
    // slots, IndexMap key/value slots plus hash/control storage, and the
    // maximum simultaneous nested-literal parent/child buffers (depth 128).
    // The fixed addition covers empty containers and allocator headers.
    u64::try_from(framed_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(256)
        .saturating_add(4096)
}

fn limited_object_stream_filter_supported(dictionary: &Dictionary) -> bool {
    match dictionary.get(b"Filter") {
        Err(_) => true,
        Ok(Object::Name(filter)) if filter == b"FlateDecode" => match dictionary.get(b"DecodeParms") {
            Err(_) | Ok(Object::Null) => true,
            Ok(Object::Dictionary(params)) => params
                .get(b"Predictor")
                .and_then(Object::as_i64)
                .map_or(true, |predictor| predictor <= 1),
            Ok(_) => false,
        },
        Ok(_) => false,
    }
}

fn read_window(
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

fn parse_object_body(input: &[u8], id: crate::ObjectId, offset: u64) -> IndexedReaderResult<ParsedObject> {
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
enum EndstreamStatus {
    Found,
    Missing,
    LimitExceeded,
}

fn validate_endstream(
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
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

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
        object_pdf_with_root(definitions, (1, 0))
    }

    fn object_pdf_with_root(definitions: &[ObjectDef<'_>], root: crate::ObjectId) -> Vec<u8> {
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
            &format!("<< /Size {} /Root {} {} R >>", u64::from(max_id) + 1, root.0, root.1),
        );
        pdf
    }

    fn xref_number_header_mismatch_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let offset = push_object(&mut pdf, 2, b"<< /Type /Catalog >>");
        append_classic(&mut pdf, &[(1, vec![(offset, 0, true)])], "<< /Size 3 /Root 1 0 R >>");
        pdf
    }

    fn malformed_normal_xref_fixture(object_id: u32, offset_delta: u64, body: &[u8]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let object_offset = push_object(&mut pdf, object_id, body);
        let malformed_offset = object_offset.checked_add(offset_delta).unwrap();
        append_classic(
            &mut pdf,
            &[(object_id, vec![(malformed_offset, 0, true)])],
            &format!("<< /Size {} >>", u64::from(object_id) + 1),
        );
        pdf
    }

    fn nul_header_probe_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        push_object(&mut pdf, 1, b"(unreachable)");
        let malformed_offset = u64::try_from(pdf.len()).unwrap();
        pdf.extend(std::iter::repeat_n(
            b'\0',
            usize::try_from(INDIRECT_HEADER_LIMIT).unwrap() + 1,
        ));
        append_classic(&mut pdf, &[(1, vec![(malformed_offset, 0, true)])], "<< /Size 2 >>");
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
            Object::Stream(Stream::new(
                dictionary! { "Type" => "Metadata" },
                b"encrypted stream".to_vec(),
            )),
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

    fn generated_page_tree_pdf(page_count: u32, declared_count: i64) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let kids: Vec<_> = (0..page_count)
            .map(|index| {
                let id = (index + 3, 0);
                document.objects.insert(
                    id,
                    Object::Dictionary(dictionary! {
                        "Type" => "Page",
                        "Parent" => Object::Reference((2, 0)),
                        "Index" => i64::from(index),
                    }),
                );
                Object::Reference(id)
            })
            .collect();
        document.objects.insert(
            (2, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => declared_count,
                "Resources" => Object::Dictionary(dictionary! { "Marker" => "root" }),
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        document.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
        );
        document.max_id = page_count + 2;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn encrypted_page_tree_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
        );
        document.objects.insert(
            (2, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference((3, 0)), Object::Reference((4, 0))],
                "Count" => 99,
                "Rotate" => 90,
            }),
        );
        for id in [3, 4] {
            document.objects.insert(
                (id, 0),
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => Object::Reference((2, 0)),
                    "Secret" => format!("page-{id}"),
                }),
            );
        }
        document.max_id = 4;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let file_id = vec![0x61; 16];
        document.trailer.set(
            "ID",
            Object::Array(vec![
                Object::String(file_id.clone(), StringFormat::Literal),
                Object::String(file_id, StringFormat::Literal),
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
        document.encrypt(&state).unwrap();
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn generated_deep_page_tree_pdf(leaf_depth: usize) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
        );
        for depth in 0..=leaf_depth {
            let id = u32::try_from(depth).unwrap() + 2;
            let object = if depth == leaf_depth {
                Object::Dictionary(dictionary! { "Type" => "Page" })
            } else {
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => vec![Object::Reference((id + 1, 0))],
                    "Count" => 1,
                })
            };
            document.objects.insert((id, 0), object);
        }
        document.max_id = u32::try_from(leaf_depth).unwrap() + 2;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn repeated_page_dag_pdf(levels: u32) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
        );
        for level in 0..levels {
            let id = level + 2;
            let child = (id + 1, 0);
            document.objects.insert(
                (id, 0),
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => vec![Object::Reference(child), Object::Reference(child)],
                    "Count" => 1_i64 << levels.min(30),
                }),
            );
        }
        let leaf = (levels + 2, 0);
        document
            .objects
            .insert(leaf, Object::Dictionary(dictionary! { "Type" => "Page" }));
        document.max_id = leaf.0;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn wide_page_tree_pdf(distinct_nodes: u32, non_reference_kids: usize, self_cycle: bool) -> Vec<u8> {
        assert!(distinct_nodes > 0);
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
        );
        for node in 0..distinct_nodes {
            let id = node + 2;
            let child = if self_cycle || node + 1 == distinct_nodes {
                (id, 0)
            } else {
                (id + 1, 0)
            };
            let mut kids = Vec::with_capacity(non_reference_kids + 1);
            kids.push(Object::Reference(child));
            kids.extend(std::iter::repeat_n(Object::Null, non_reference_kids));
            document.objects.insert(
                (id, 0),
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => kids,
                    "Count" => 0,
                }),
            );
        }
        document.max_id = distinct_nodes + 1;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        pdf
    }

    fn open_encrypted(pdf: &[u8], password: Option<&[u8]>) -> IndexedReaderResult<IndexedReader> {
        IndexedReader::open_with_password(
            Arc::new(BytesSource::from(pdf.to_vec())),
            ResolverLimits::default(),
            password,
        )
    }

    fn assert_encrypted_fixture_plaintext(reader: &IndexedReader) {
        assert_eq!(
            reader.resolve_object((1, 0)).unwrap(),
            Object::String(b"encrypted string".to_vec(), StringFormat::Literal)
        );
        assert_eq!(
            reader.resolve_object((2, 0)).unwrap().as_stream().unwrap().content,
            b"encrypted stream"
        );
        assert_eq!(
            reader
                .resolve_object((3, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Sentinel")
                .unwrap(),
            &Object::Reference((1, 0))
        );
    }

    fn open_reader(pdf: &[u8], limits: ResolverLimits) -> IndexedReader {
        IndexedReader::open_with_limits(Arc::new(BytesSource::from(pdf.to_vec())), limits).unwrap()
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

            assert!(matches!(
                open_encrypted(&pdf, None),
                Err(IndexedReaderError::PasswordRequired)
            ));
            let wrong_a = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
            let wrong_b = open_encrypted(&pdf, Some(b"wrong")).err().unwrap();
            assert!(matches!(wrong_a, IndexedReaderError::InvalidPassword));
            assert_eq!(format!("{wrong_a:?}"), format!("{wrong_b:?}"));

            let user = open_encrypted(&pdf, Some(b"user")).unwrap();
            assert_encrypted_fixture_plaintext(&user);
            let eager = Document::load_mem_with_options(&pdf, crate::LoadOptions::with_password("user")).unwrap();
            for id in [(1, 0), (2, 0), (3, 0)] {
                assert_eq!(user.resolve_object(id).unwrap(), eager.get_object(id).unwrap().clone());
            }

            let encrypt_id = user.index.encrypt_object_id.unwrap();
            assert_eq!(
                user.resolve_object(encrypt_id)
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
    fn page_map_propagates_a_direct_malformed_root_object() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Catalog /Pages 2 0 R",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
        ]);
        let eager = Document::load_mem(&pdf).unwrap();
        assert!(matches!(
            eager.get_object((1, 0)),
            Err(crate::Error::ObjectNotFound((1, 0)))
        ));
        assert!(eager.page_iter().next().is_none());

        let reader = open_reader(&pdf, ResolverLimits::default());
        let direct = reader.resolve_object((1, 0)).unwrap_err();
        assert!(matches!(
            direct,
            IndexedReaderError::InvalidIndirectObject { id: (1, 0), .. }
                | IndexedReaderError::IncompleteObject { id: (1, 0), .. }
        ));
        let page_map = reader.page_map().unwrap_err();
        assert_eq!(page_map.to_string(), direct.to_string());
    }

    #[test]
    fn page_map_uses_physical_kids_order_ignores_count_and_tracks_inheritance() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Catalog /Pages 2 0 R >>",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Count 999 /Resources 8 0 R /MediaBox [0 0 600 800] /Kids 10 0 R >>",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page /Parent 2 0 R /CropBox [0 0 300 400] >>",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Count 0 /Rotate 90 /Resources << /Nested true >> /Kids [7 0 R] >>",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
            ObjectDef {
                id: 6,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /NotPage >>",
            },
            ObjectDef {
                id: 7,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page /MediaBox [0 0 200 200] >>",
            },
            ObjectDef {
                id: 8,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /RootResource true >>",
            },
            ObjectDef {
                id: 10,
                object_generation: 0,
                xref_generation: 0,
                body: b"[3 0 R << /Type /Page >> 4 0 R 9 0 R 5 1 R 3 0 R 6 0 R]",
            },
        ]);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let page_map = PageMap::from_reader(&reader).unwrap();
        let eager: Vec<_> = Document::load_mem(&pdf).unwrap().page_iter().collect();

        assert_eq!(page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(), eager);
        assert_eq!(eager, vec![(3, 0), (7, 0), (3, 0)]);
        assert_eq!(
            page_map.pages[0].inherited,
            InheritedPageAttributeOwners {
                resources: Some((2, 0)),
                media_box: Some((2, 0)),
                crop_box: Some((3, 0)),
                rotate: None,
            }
        );
        assert_eq!(
            page_map.pages[1].inherited,
            InheritedPageAttributeOwners {
                resources: Some((4, 0)),
                media_box: Some((7, 0)),
                crop_box: None,
                rotate: Some((4, 0)),
            }
        );
        assert_eq!(page_map.pages[2], page_map.pages[0]);
    }

    #[test]
    fn page_map_bounds_cycles_depth_and_page_count_without_trusting_count() {
        let cyclic = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Catalog /Pages 2 0 R >>",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 999999 >>",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Kids [2 0 R 5 0 R] /Count -1 >>",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
        ]);
        let reader = open_reader(&cyclic, ResolverLimits::default());
        let eager = Document::load_mem(&cyclic).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();
        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            eager_pages
        );
        assert_eq!(work, eager.objects.len());

        let depth_limited = PageMap::from_reader_with_limits(
            &reader,
            PageMapLimits {
                max_depth: 0,
                max_pages: 10,
            },
        )
        .unwrap();
        assert!(depth_limited.pages.is_empty());

        let two_pages = generated_page_tree_pdf(2, 1);
        let reader = open_reader(&two_pages, ResolverLimits::default());
        assert!(matches!(
            PageMap::from_reader_with_limits(
                &reader,
                PageMapLimits {
                    max_depth: 256,
                    max_pages: 1,
                }
            ),
            Err(IndexedReaderError::PageCountLimitExceeded { limit: 1 })
        ));
    }

    #[test]
    fn page_map_depth_limit_is_inclusive_and_bounded() {
        let at_limit = generated_deep_page_tree_pdf(DEFAULT_PAGE_TREE_DEPTH_LIMIT);
        let reader = open_reader(&at_limit, ResolverLimits::default());
        assert_eq!(PageMap::from_reader(&reader).unwrap().pages.len(), 1);

        let over_limit = generated_deep_page_tree_pdf(DEFAULT_PAGE_TREE_DEPTH_LIMIT + 1);
        let reader = open_reader(&over_limit, ResolverLimits::default());
        assert!(PageMap::from_reader(&reader).unwrap().pages.is_empty());
    }

    #[test]
    fn public_options_wire_every_exposed_resolver_and_page_limit() {
        let options = IndexedReaderOptions {
            object_bytes: 101,
            stream_bytes: 102,
            endstream_tail_bytes: 103,
            reference_depth: 7,
            page_tree_depth: 0,
            max_pages: 11,
            password: None,
        };
        let limits = ResolverLimits::from(&options);
        assert_eq!(limits.max_object_bytes, 101);
        assert_eq!(limits.max_stream_bytes, 102);
        assert_eq!(limits.max_endstream_tail_bytes, 103);
        assert_eq!(limits.max_length_depth, 7);

        let pdf = generated_deep_page_tree_pdf(1);
        let reader = IndexedReader::open_with_options(BytesSource::from(pdf), options).unwrap();
        assert!(reader.page_map().unwrap().is_empty());
    }

    #[test]
    fn repeated_page_dag_uses_eager_global_work_budget() {
        let pdf = repeated_page_dag_pdf(15);
        let eager = Document::load_mem(&pdf).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        let source = Arc::new(TracingBytesSource {
            bytes: pdf,
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();
        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            eager_pages
        );
        assert_eq!(eager_pages.len(), 3);
        assert_eq!(work, eager.objects.len());
        assert!(source.requests.lock().unwrap().len() <= (work + 2) * 4);
    }

    #[test]
    fn wide_self_cycle_pending_metadata_is_capped_by_near_maximum_work() {
        const NON_REFERENCE_KIDS: usize = 14_000;
        let pdf = wide_page_tree_pdf(1, NON_REFERENCE_KIDS, true);
        assert!(pdf.len() > 64 * 1_024);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let work_budget = usize::try_from(MAX_XREF_ENTRIES - 1).unwrap();
        let (page_map, work) =
            PageMap::from_reader_with_work_budget_and_stats(&reader, PageMapLimits::default(), work_budget).unwrap();

        assert!(page_map.pages.is_empty());
        assert_eq!(work.consumed, work_budget);
        assert!(work.peak_pending_items <= work_budget);
        assert!(work.peak_pending_items > work_budget * 9 / 10);
        assert_eq!(
            work.peak_pending_bytes,
            work.peak_pending_items * std::mem::size_of::<PendingKid>()
        );
        assert!(std::mem::size_of::<PendingKid>() <= 32);
    }

    #[test]
    fn repeated_distinct_wide_nodes_keep_only_compact_reachable_work() {
        const DISTINCT_NODES: u32 = 24;
        const NON_REFERENCE_KIDS: usize = 14_000;
        let pdf = wide_page_tree_pdf(DISTINCT_NODES, NON_REFERENCE_KIDS, false);
        assert!(pdf.len() > usize::try_from(DISTINCT_NODES).unwrap() * 64 * 1_024);
        let eager = Document::load_mem(&pdf).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        let reader = open_reader(&pdf, ResolverLimits::default());
        let (page_map, work) = PageMap::from_reader_with_limits_and_stats(&reader, PageMapLimits::default()).unwrap();

        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            eager_pages
        );
        assert_eq!(work.consumed, eager.objects.len());
        assert!(work.peak_pending_items <= work.consumed);
        assert!(work.peak_pending_bytes < 64 * 1_024);
        assert_eq!(
            work.peak_pending_bytes,
            work.peak_pending_items * std::mem::size_of::<PendingKid>()
        );
    }

    #[test]
    fn page_map_work_budget_counts_non_reference_kids_like_eager() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Catalog /Pages 2 0 R >>",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Kids [null 3 0 R 17 (bad)] /Count 1 >>",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
        ]);
        let eager = Document::load_mem(&pdf).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        let reader = open_reader(&pdf, ResolverLimits::default());
        let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();

        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            eager_pages
        );
        assert_eq!(eager_pages, vec![(3, 0)]);
        assert_eq!(work, eager.objects.len());
    }

    #[test]
    fn page_map_propagates_malformed_and_decompression_object_stream_failures() {
        let malformed_members = [
            (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
            (11, b"<< /Type /Pages /Kids [12 0 R]".as_slice()),
            (12, b"<< /Type /Page >>".as_slice()),
        ];
        let (first, malformed_content) = object_stream_content(&malformed_members);
        let malformed = object_stream_fixture(
            &format!("/Type /ObjStm /N 3 /First {first}"),
            &malformed_content,
            &[(10, 0), (11, 1), (12, 2)],
        );
        let eager = Document::load_mem(&malformed.pdf).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        let reader = open_reader(&malformed.pdf, ResolverLimits::default());
        assert!(eager_pages.is_empty());
        assert!(matches!(
            reader.resolve_object((11, 0)),
            Err(IndexedReaderError::ObjectStreamMember { .. })
        ));
        assert!(matches!(
            PageMap::from_reader(&reader),
            Err(IndexedReaderError::ObjectStreamMember { .. })
        ));

        let invalid_filter = object_stream_fixture(
            "/Type /ObjStm /N 1 /First 0 /Filter /ASCII85Decode",
            b"uuuuu",
            &[(10, 0)],
        );
        let eager = Document::load_mem(&invalid_filter.pdf).unwrap();
        assert!(eager.page_iter().next().is_none());
        let reader = open_reader(&invalid_filter.pdf, ResolverLimits::default());
        let direct_error = reader.resolve_object((10, 0)).unwrap_err();
        assert!(
            matches!(
                &direct_error,
                IndexedReaderError::ObjectStreamMember {
                    source: crate::Error::Decompress(_),
                    ..
                }
            ),
            "unexpected direct error: {direct_error:?}"
        );
        assert!(matches!(
            PageMap::from_reader(&reader),
            Err(IndexedReaderError::ObjectStreamMember {
                source: crate::Error::Decompress(_),
                ..
            })
        ));

        let valid_members = [
            (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
            (11, b"<< /Type /Pages /Kids [12 0 R] /Count 1 >>".as_slice()),
            (12, b"<< /Type /Page >>".as_slice()),
        ];
        let (first, decoded) = object_stream_content(&valid_members);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < decoded.len());
        let limited = object_stream_fixture(
            &format!("/Type /ObjStm /N 3 /First {first} /Filter /FlateDecode"),
            &compressed,
            &[(10, 0), (11, 1), (12, 2)],
        );
        let limit = compressed.len();
        let reader = open_reader(
            &limited.pdf,
            ResolverLimits {
                max_stream_bytes: u64::try_from(limit).unwrap(),
                ..ResolverLimits::default()
            },
        );
        assert!(matches!(
            PageMap::from_reader(&reader),
            Err(IndexedReaderError::ObjectStreamMember {
                source: crate::Error::Decompress(crate::DecompressError::MemoryLimitExceeded {
                    limit: actual
                }),
                ..
            }) if actual == limit
        ));
    }

    #[test]
    fn encrypted_page_map_matches_authenticated_eager_order() {
        let pdf = encrypted_page_tree_pdf();
        assert!(matches!(
            open_encrypted(&pdf, None),
            Err(IndexedReaderError::PasswordRequired)
        ));
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let page_map = PageMap::from_reader(&reader).unwrap();
        let eager = Document::load_mem_with_options(&pdf, crate::LoadOptions::with_password("user")).unwrap();
        let eager_pages: Vec<_> = eager.page_iter().collect();
        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            eager_pages
        );
        assert_eq!(eager_pages, vec![(3, 0), (4, 0)]);
        assert_eq!(page_map.pages[0].inherited.rotate, Some((2, 0)));
    }

    #[test]
    fn page_map_enumerates_five_thousand_unique_pages() {
        let pdf = generated_page_tree_pdf(5_000, 1);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let page_map = PageMap::from_reader(&reader).unwrap();
        assert_eq!(page_map.pages.len(), 5_000);
        assert_eq!(page_map.pages.first().unwrap().id, (3, 0));
        assert_eq!(page_map.pages.last().unwrap().id, (5_002, 0));
        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<HashSet<_>>().len(),
            5_000
        );
        assert!(
            page_map
                .pages
                .iter()
                .all(|page| { page.inherited.resources == Some((2, 0)) && page.inherited.media_box == Some((2, 0)) })
        );
    }

    #[test]
    fn encrypted_object_stream_container_is_decrypted_once_and_xref_stays_plain() {
        let (pdf, image_plaintext, xref_plaintext) = encrypted_object_stream_pdf();
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();

        let member = reader.resolve_object((10, 0)).unwrap();
        let member = member.as_dict().unwrap();
        assert_eq!(
            member.get(b"Text").unwrap(),
            &Object::String(b"member secret".to_vec(), StringFormat::Literal)
        );
        assert_eq!(member.get(b"Image").unwrap(), &Object::Reference((20, 0)));

        for _ in 0..2 {
            assert_eq!(
                reader.resolve_object((20, 0)).unwrap().as_stream().unwrap().content,
                image_plaintext
            );
        }
        assert_eq!(
            reader.resolve_object((21, 0)).unwrap(),
            Object::String(b"normal secret".to_vec(), StringFormat::Literal)
        );
        assert_eq!(
            reader.resolve_object((31, 0)).unwrap().as_stream().unwrap().content,
            xref_plaintext
        );
        assert_eq!(
            reader
                .resolve_object((30, 0))
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
            Err(IndexedReaderError::InvalidHeader { .. })
        ));
    }

    #[test]
    fn malformed_startxref_and_prev_fail_without_fallback() {
        let mut missing = classic_pdf();
        let marker = rfind(&missing, b"startxref").unwrap();
        missing[marker..marker + b"startxref".len()].fill(b'x');
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(missing))),
            Err(IndexedReaderError::InvalidStartXref { .. })
        ));

        let mut out_of_bounds = classic_pdf();
        let marker = rfind(&out_of_bounds, b"startxref\n").unwrap() + b"startxref\n".len();
        let end = out_of_bounds[marker..].iter().position(|byte| *byte == b'\n').unwrap() + marker;
        out_of_bounds.splice(marker..end, b"999999999".iter().copied());
        assert!(matches!(
            PdfIndex::open(Arc::new(BytesSource::from(out_of_bounds))),
            Err(IndexedReaderError::InvalidXref { .. })
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
            Err(IndexedReaderError::InvalidTrailerOffset { key: "Prev" })
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
            Err(IndexedReaderError::StructureLimitExceeded {
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
            Err(IndexedReaderError::XrefDecompression(_))
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
            Err(IndexedReaderError::InvalidXref { .. })
        ));
        assert!(check_entry_limit(MAX_XREF_ENTRIES).is_ok());
        assert!(matches!(
            check_entry_limit(MAX_XREF_ENTRIES + 1),
            Err(IndexedReaderError::EntryLimitExceeded {
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
                format!("{:?}", reader.resolve_object(id).unwrap()),
                format!("{:?}", eager.get_object(id).unwrap())
            );
        }
    }

    #[test]
    fn shared_batch_deduplicates_and_restores_first_occurrence_order_with_errors() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"(one)",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"[2 (two)]",
            },
        ]);
        let reader = open_reader(&pdf, ResolverLimits::default());
        let resolved = reader.resolve_many_shared(&[(2, 0), (99, 0), (1, 0), (2, 0), (1, 1)]);

        assert_eq!(
            resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [(2, 0), (99, 0), (1, 0), (1, 1)]
        );
        assert_eq!(resolved[0].1.as_ref().unwrap().as_array().unwrap().len(), 2);
        assert!(matches!(
            resolved[1].1.as_ref().unwrap_err().as_ref(),
            IndexedReaderError::MissingNormalObject { id: (99, 0) }
        ));
        assert_eq!(resolved[2].1.as_ref().unwrap().as_str().unwrap(), b"one");
        assert!(matches!(
            resolved[3].1.as_ref().unwrap_err().as_ref(),
            IndexedReaderError::MissingNormalObjectAtXref {
                id: (1, 1),
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: (1, 1),
                    indexed: 0,
                    actual: (1, 0)
                }
            }
        ));
        assert!(reader.resolve_many_shared(&[]).is_empty());
    }

    #[test]
    fn shared_batch_groups_object_stream_reads_and_preserves_member_errors() {
        let members = [
            (10, b"(ten)".as_slice()),
            (11, b"(eleven)".as_slice()),
            (12, b"[12]".as_slice()),
        ];
        let (first, decoded) = object_stream_content(&members);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        let content = encoder.finish().unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 3 /First {first} /Filter /FlateDecode"),
            &content,
            &[(10, 0), (11, 1), (12, 2), (13, 1)],
        );
        let source = Arc::new(TracingBytesSource {
            bytes: fixture.pdf,
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0), (13, 0), (11, 0), (10, 0)]);
        let batch_reads = source.requests.lock().unwrap().len();
        assert_eq!(
            resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [(12, 0), (10, 0), (13, 0), (11, 0)]
        );
        assert_eq!(
            resolved[0].1.as_ref().unwrap().as_array().unwrap()[0].as_i64().unwrap(),
            12
        );
        assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"ten");
        assert!(matches!(
            resolved[2].1.as_ref().unwrap_err().as_ref(),
            IndexedReaderError::ObjectStreamMember {
                id: (13, 0),
                container: (5, 0),
                index: 1,
                ..
            }
        ));
        assert_eq!(resolved[3].1.as_ref().unwrap().as_str().unwrap(), b"eleven");

        source.requests.lock().unwrap().clear();
        for id in [(12, 0), (10, 0), (13, 0), (11, 0)] {
            let _ = reader.resolve_object(id);
        }
        let scalar_reads = source.requests.lock().unwrap().len();
        assert!(batch_reads < scalar_reads, "batch={batch_reads}, scalar={scalar_reads}");

        let malformed_header = b"10 0 bad nope 12 6 ";
        let mut malformed_content = malformed_header.to_vec();
        malformed_content.extend_from_slice(b"(ten) (twelve)");
        let malformed = object_stream_fixture(
            &format!("/Type /ObjStm /N 3 /First {}", malformed_header.len()),
            &malformed_content,
            &[(10, 0), (12, 2)],
        );
        let reader = open_reader(&malformed.pdf, ResolverLimits::default());
        let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0)]);
        assert_eq!(resolved[0].1.as_ref().unwrap().as_str().unwrap(), b"twelve");
        assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"ten");

        let duplicate_header = b"10 0 10 6 ";
        let mut duplicate_content = duplicate_header.to_vec();
        duplicate_content.extend_from_slice(b"(one) (two)");
        let duplicate = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {}", duplicate_header.len()),
            &duplicate_content,
            &[(10, 1)],
        );
        let reader = open_reader(&duplicate.pdf, ResolverLimits::default());
        let resolved = reader.resolve_many_shared(&[(10, 0), (10, 0)]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1.as_ref().unwrap().as_str().unwrap(), b"two");
    }

    fn configure_test_caches(reader: &mut IndexedReader, total_bytes: usize, total_entries: usize) {
        reader.configure_resolution_caches(total_bytes / 2, total_entries / 2, total_bytes / 4, total_entries / 4);
    }

    #[test]
    fn shared_scalar_is_uncached_by_default_and_cached_values_promote_when_configured() {
        let source = Arc::new(TracingBytesSource {
            bytes: classic_pdf(),
            requests: Mutex::new(Vec::new()),
        });
        let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();
        let first = reader.resolve_object_shared((4, 0)).unwrap();
        let first_reads = source.requests.lock().unwrap().len();
        let second = reader.resolve_object_shared((4, 0)).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(source.requests.lock().unwrap().len() > first_reads);
        assert_eq!(reader.object_cache_stats(), IndexedObjectCacheStats::default());

        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        source.requests.lock().unwrap().clear();
        let cached_first = reader.resolve_object_shared((4, 0)).unwrap();
        let cached_reads = source.requests.lock().unwrap().len();
        let cached_second = reader.resolve_object_shared((4, 0)).unwrap();
        assert!(Arc::ptr_eq(&cached_first, &cached_second));
        assert_eq!(source.requests.lock().unwrap().len(), cached_reads);
        assert_eq!(cached_first.as_ref(), &reader.resolve_object((4, 0)).unwrap());
        let stats = reader.object_cache_stats();
        assert_eq!(stats.object_misses, 1);
        assert_eq!(stats.object_hits, 1);
        assert_eq!(stats.object_loads, 1);
        assert_eq!(stats.object_promotions, 1);
        assert_eq!(stats.probation_entries, 0);
        assert_eq!(stats.protected_entries, 1);
    }

    #[test]
    fn shared_scalar_negative_caches_fatal_errors_but_retries_transient_sources() {
        let source = Arc::new(SwitchableFailureSource {
            bytes: classic_pdf(),
            mode: AtomicU8::new(0),
        });
        let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);

        let first = reader.resolve_object_shared((99, 0)).unwrap_err();
        let second = reader.resolve_object_shared((99, 0)).unwrap_err();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(matches!(
            first.as_ref(),
            IndexedReaderError::MissingNormalObject { id: (99, 0) }
        ));

        source.mode.store(1, Ordering::SeqCst);
        assert!(matches!(
            reader.resolve_object_shared((4, 0)).unwrap_err().as_ref(),
            IndexedReaderError::Source(_)
        ));
        source.mode.store(0, Ordering::SeqCst);
        assert_eq!(
            reader
                .resolve_object_shared((4, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Title")
                .unwrap(),
            &Object::String(b"base".to_vec(), StringFormat::Literal)
        );
        let stats = reader.object_cache_stats();
        assert_eq!(stats.negative_hits, 1);
        assert_eq!(stats.transient_failures, 1);
        assert_eq!(stats.object_misses, 3);
    }

    #[test]
    fn shared_scalar_reuses_one_decoded_object_stream_across_members() {
        let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
        let (first, decoded) = object_stream_content(&members);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        let content = encoder.finish().unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {first} /Filter /FlateDecode"),
            &content,
            &[(10, 0), (11, 1)],
        );
        let source = Arc::new(TracingBytesSource {
            bytes: fixture.pdf,
            requests: Mutex::new(Vec::new()),
        });
        let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        source.requests.lock().unwrap().clear();

        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
        let first_reads = source.requests.lock().unwrap().len();
        assert_eq!(
            reader.resolve_object_shared((11, 0)).unwrap().as_str().unwrap(),
            b"eleven"
        );
        assert_eq!(source.requests.lock().unwrap().len(), first_reads);
        assert_eq!(
            reader.resolve_object((10, 0)).unwrap(),
            *reader.resolve_object_shared((10, 0)).unwrap()
        );
        let stats = reader.object_stream_cache_stats();
        assert_eq!(stats.loads, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.entries, 1);
        assert!(stats.bytes >= decoded.len());
    }

    #[test]
    fn cached_constructor_wires_every_partition_into_unified_stats() {
        let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
        let (first, decoded) = object_stream_content(&members);
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {first}"),
            &decoded,
            &[(10, 0), (11, 1)],
        );
        let options = IndexedReaderCacheOptions::new(8 * 1024 * 1024, 1024);
        let reader =
            IndexedReader::open_cached(BytesSource::from(fixture.pdf), IndexedReaderOptions::default(), options)
                .unwrap();

        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
        assert_eq!(
            reader.resolve_object_shared((11, 0)).unwrap().as_str().unwrap(),
            b"eleven"
        );
        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");

        let stats = reader.cache_stats();
        assert!(stats.source().loads() > 0);
        assert_eq!(stats.object().object_loads, 2);
        assert_eq!(stats.object().object_hits, 1);
        assert_eq!(stats.object_stream().loads, 1);
        assert_eq!(stats.object_stream().hits, 1);
        assert_eq!(stats.object_stream().entries, 1);
        assert!(stats.object_stream().bytes >= decoded.len());
        assert_eq!(*stats.object(), reader.object_cache_stats());
        assert_eq!(*stats.object_stream(), reader.object_stream_cache_stats());
        assert!(stats.current_bytes() <= options.max_bytes());
        assert!(stats.peak_bytes() <= options.max_bytes());
        assert!(stats.current_entries() <= options.max_entries());
        assert!(stats.peak_entries() <= options.max_entries());
    }

    #[test]
    fn oversized_streams_bypass_probation_retention() {
        let mut document = Document::with_version("1.7");
        document.objects.insert(
            (1, 0),
            Object::Stream(Stream::new(Dictionary::new(), vec![b'x'; 8 * 1024])),
        );
        document.max_id = 1;
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        let mut reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
        // Object half is 4 KiB and probation is one quarter of that (1 KiB).
        configure_test_caches(&mut reader, 8 * 1024, 64);
        assert_eq!(
            reader
                .resolve_object_shared((1, 0))
                .unwrap()
                .as_stream()
                .unwrap()
                .content
                .len(),
            8 * 1024
        );
        assert_eq!(
            reader
                .resolve_object_shared((1, 0))
                .unwrap()
                .as_stream()
                .unwrap()
                .content
                .len(),
            8 * 1024
        );
        let stats = reader.object_cache_stats();
        assert_eq!(stats.object_bypasses, 2);
        assert_eq!(stats.probation_entries + stats.protected_entries, 0);
    }

    #[test]
    fn segmented_cache_promotes_churns_and_stays_within_a_five_thousand_entry_cap() {
        let counters = Arc::new(CacheCounters::default());
        let cache = SharedCache::new(
            2 * 1024 * 1024,
            5_000,
            512 * 1024,
            75,
            CacheKind::Object,
            Arc::clone(&counters),
        );
        for id in 1..=5_100 {
            assert_eq!(
                *cache
                    .resolve((id, 0), || Ok(Arc::new(Object::Integer(i64::from(id)))), |_| 64)
                    .unwrap(),
                Object::Integer(i64::from(id))
            );
        }
        let (probation_entries, probation_bytes, protected_entries, protected_bytes) = cache.residency();
        assert_eq!(probation_entries, 1_250);
        assert_eq!(protected_entries, 0);
        assert!(probation_bytes + protected_bytes <= 2 * 1024 * 1024);
        assert_eq!(counters.object_evictions.load(Ordering::Relaxed), 3_850);

        let promoted = cache
            .resolve((5_100, 0), || panic!("resident entry must not reload"), |_| 64)
            .unwrap();
        assert_eq!(*promoted, Object::Integer(5_100));
        let (_, _, protected_entries, _) = cache.residency();
        assert_eq!(protected_entries, 1);
    }

    #[test]
    fn single_flight_shares_one_load_and_one_arc_with_waiters() {
        let counters = Arc::new(CacheCounters::default());
        let cache = Arc::new(SharedCache::new(
            1024,
            8,
            256,
            75,
            CacheKind::Object,
            Arc::clone(&counters),
        ));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let cache = Arc::clone(&cache);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            threads.push(std::thread::spawn(move || {
                cache
                    .resolve(
                        (1, 0),
                        || {
                            entered.wait();
                            release.wait();
                            Ok(Arc::new(Object::Integer(42)))
                        },
                        |_| 64,
                    )
                    .unwrap()
            }));
        }
        entered.wait();
        while counters.object_waits.load(Ordering::SeqCst) < 3 {
            std::thread::yield_now();
        }
        release.wait();
        let values: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert!(values.iter().all(|value| Arc::ptr_eq(&values[0], value)));
        assert_eq!(counters.object_loads.load(Ordering::Relaxed), 1);
        assert_eq!(counters.object_waits.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn transient_failure_is_shared_only_with_waiters_then_post_publication_retries() {
        let counters = Arc::new(CacheCounters::default());
        let cache = Arc::new(SharedCache::new(
            1024,
            8,
            256,
            75,
            CacheKind::Object,
            Arc::clone(&counters),
        ));
        let load_entered = Arc::new(std::sync::Barrier::new(2));
        let release_load = Arc::new(std::sync::Barrier::new(2));
        let published = Arc::new(std::sync::Barrier::new(2));
        let release_publisher = Arc::new(std::sync::Barrier::new(2));
        {
            let published = Arc::clone(&published);
            let release_publisher = Arc::clone(&release_publisher);
            *cache.after_publish_hook.lock().unwrap() = Some(Arc::new(move || {
                published.wait();
                release_publisher.wait();
            }));
        }

        let leader_cache = Arc::clone(&cache);
        let leader_entered = Arc::clone(&load_entered);
        let leader_release = Arc::clone(&release_load);
        let leader = std::thread::spawn(move || {
            leader_cache.resolve(
                (1, 0),
                || {
                    leader_entered.wait();
                    leader_release.wait();
                    Err(Arc::new(IndexedReaderError::Source(SourceError::Io(
                        std::io::Error::other("transient"),
                    ))))
                },
                |_| 64,
            )
        });
        load_entered.wait();

        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    cache.resolve((1, 0), || panic!("waiter must not become a second leader"), |_| 64)
                })
            })
            .collect();
        while counters.object_waits.load(Ordering::SeqCst) < 2 {
            std::thread::yield_now();
        }
        release_load.wait();
        published.wait();

        let waiter_errors: Vec<_> = waiters
            .into_iter()
            .map(|waiter| waiter.join().unwrap().unwrap_err())
            .collect();
        assert!(Arc::ptr_eq(&waiter_errors[0], &waiter_errors[1]));

        let retried = cache
            .resolve((1, 0), || Ok(Arc::new(Object::Integer(42))), |_| 64)
            .unwrap();
        assert_eq!(*retried, Object::Integer(42));

        release_publisher.wait();
        let leader_error = leader.join().unwrap().unwrap_err();
        assert!(Arc::ptr_eq(&leader_error, &waiter_errors[0]));
        assert_eq!(counters.object_loads.load(Ordering::Relaxed), 2);
        assert_eq!(counters.object_misses.load(Ordering::Relaxed), 2);
        assert_eq!(counters.object_hits.load(Ordering::Relaxed), 2);
        assert_eq!(counters.object_waits.load(Ordering::Relaxed), 2);
        assert_eq!(counters.object_transient_failures.load(Ordering::Relaxed), 1);
        assert_eq!(counters.negative_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn fatal_failure_remains_negative_cached_with_shared_identity_and_count() {
        let counters = Arc::new(CacheCounters::default());
        let cache = SharedCache::<Object>::new(1024, 8, 256, 75, CacheKind::Object, Arc::clone(&counters));
        let first = cache
            .resolve(
                (99, 0),
                || Err(Arc::new(IndexedReaderError::MissingNormalObject { id: (99, 0) })),
                |_| 64,
            )
            .unwrap_err();
        let second = cache
            .resolve((99, 0), || panic!("fatal negative entry must not reload"), |_| 64)
            .unwrap_err();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(counters.object_loads.load(Ordering::Relaxed), 1);
        assert_eq!(counters.object_misses.load(Ordering::Relaxed), 1);
        assert_eq!(counters.object_hits.load(Ordering::Relaxed), 1);
        assert_eq!(counters.negative_hits.load(Ordering::Relaxed), 1);
        assert_eq!(counters.object_transient_failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn public_cache_counters_saturate_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        atomic_saturating_increment(&counter);
        atomic_saturating_increment(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn loading_cells_are_not_evicted_while_pinned() {
        let counters = Arc::new(CacheCounters::default());
        let cache = SharedCache::<Object>::new(64, 1, 64, 75, CacheKind::Object, counters);
        let pinned = Arc::new(SharedCell::loading());
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.probation.push_back((1, 0));
            inner.entries.insert(
                (1, 0),
                CacheEntry {
                    cell: Arc::clone(&pinned),
                    segment: CacheSegment::Probation,
                    bytes: 0,
                },
            );
            cache.enforce_caps(&mut inner);
            assert!(inner.entries.contains_key(&(1, 0)));
        }
    }

    #[test]
    fn loading_entry_saturation_bypasses_without_exceeding_the_entry_cap() {
        let counters = Arc::new(CacheCounters::default());
        let cache = Arc::new(SharedCache::<Object>::new(
            1024,
            2,
            512,
            75,
            CacheKind::Object,
            Arc::clone(&counters),
        ));
        let entered = Arc::new(std::sync::Barrier::new(5));
        let release = Arc::new(std::sync::Barrier::new(5));
        let threads: Vec<_> = (1..=4)
            .map(|id| {
                let cache = Arc::clone(&cache);
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                std::thread::spawn(move || {
                    cache.resolve(
                        (id, 0),
                        || {
                            entered.wait();
                            release.wait();
                            Ok(Arc::new(Object::Integer(i64::from(id))))
                        },
                        |_| 64,
                    )
                })
            })
            .collect();
        entered.wait();
        let (probation_entries, _, protected_entries, _) = cache.residency();
        assert_eq!(probation_entries + protected_entries, 2);
        assert_eq!(counters.object_peak_entries.load(Ordering::Relaxed), 2);
        assert_eq!(counters.object_bypasses.load(Ordering::Relaxed), 2);
        release.wait();
        for thread in threads {
            assert!(thread.join().unwrap().is_ok());
        }
        assert_eq!(counters.object_loads.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn cached_shared_resolution_preserves_encryption_revisions_two_through_six() {
        for revision in 2..=6 {
            let pdf = encrypted_pdf(revision, "owner", "user");
            let mut reader = open_encrypted(&pdf, Some(b"user")).unwrap();
            configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
            let legacy = reader.resolve_object((1, 0)).unwrap();
            let first = reader.resolve_object_shared((1, 0)).unwrap();
            let second = reader.resolve_object_shared((1, 0)).unwrap();
            assert_eq!(legacy, *first, "revision {revision}");
            assert!(Arc::ptr_eq(&first, &second), "revision {revision}");
        }

        let (pdf, _, _) = encrypted_object_stream_pdf();
        let mut reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        for id in [(10, 0), (20, 0), (21, 0)] {
            assert_eq!(
                reader.resolve_object(id).unwrap(),
                *reader.resolve_object_shared(id).unwrap()
            );
        }
        assert_eq!(reader.object_stream_cache_stats().loads, 1);
    }

    #[test]
    fn cached_object_streams_preserve_duplicate_mismatch_and_malformed_errors() {
        let duplicate_header = b"10 0 10 6 ";
        let mut duplicate_content = duplicate_header.to_vec();
        duplicate_content.extend_from_slice(b"(one) (two)");
        let duplicate = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {}", duplicate_header.len()),
            &duplicate_content,
            &[(10, 1)],
        );
        let mut reader = open_reader(&duplicate.pdf, ResolverLimits::default());
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"two");

        let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
        let (first, content) = object_stream_content(&members);
        let mismatch = object_stream_fixture(&format!("/Type /ObjStm /N 2 /First {first}"), &content, &[(10, 1)]);
        let mut reader = open_reader(&mismatch.pdf, ResolverLimits::default());
        let legacy = reader.resolve_object((10, 0)).unwrap_err().to_string();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        let first_error = reader.resolve_object_shared((10, 0)).unwrap_err();
        let second_error = reader.resolve_object_shared((10, 0)).unwrap_err();
        assert_eq!(first_error.to_string(), legacy);
        assert!(Arc::ptr_eq(&first_error, &second_error));

        let malformed = object_stream_fixture("/Type /ObjStm /N 1 /First 999", b"10 0 (ten)", &[(10, 0)]);
        let mut reader = open_reader(&malformed.pdf, ResolverLimits::default());
        let legacy = reader.resolve_object((10, 0)).unwrap_err().to_string();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap_err().to_string(), legacy);
    }

    #[test]
    fn cached_object_stream_transient_source_failure_is_retried() {
        let members = [(10, b"(ten)".as_slice())];
        let (first, content) = object_stream_content(&members);
        let fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
        let source = Arc::new(FailOnceSource {
            bytes: fixture.pdf,
            armed: AtomicBool::new(false),
            armed_reads: AtomicUsize::new(0),
        });
        let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        source.armed.store(true, Ordering::SeqCst);
        assert!(matches!(
            reader.resolve_object_shared((10, 0)).unwrap_err().as_ref(),
            IndexedReaderError::Source(_)
        ));
        assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
        assert_eq!(reader.object_stream_cache_stats().transient_failures, 1);
        assert_eq!(reader.object_stream_cache_stats().loads, 2);
    }

    #[test]
    fn shared_batch_propagates_one_container_read_failure_once_with_shared_identity() {
        let members = [
            (10, b"(ten)".as_slice()),
            (11, b"(eleven)".as_slice()),
            (12, b"(twelve)".as_slice()),
        ];
        let (first, content) = object_stream_content(&members);
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 3 /First {first}"),
            &content,
            &[(10, 0), (11, 1), (12, 2)],
        );
        let source = Arc::new(FailOnceSource {
            bytes: fixture.pdf,
            armed: AtomicBool::new(false),
            armed_reads: AtomicUsize::new(0),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.armed_reads.store(0, Ordering::SeqCst);
        source.armed.store(true, Ordering::SeqCst);

        let resolved = reader.resolve_many_shared(&[(12, 0), (10, 0), (11, 0)]);
        let errors: Vec<_> = resolved
            .iter()
            .map(|(_, result)| result.as_ref().unwrap_err())
            .collect();
        assert!(matches!(errors[0].as_ref(), IndexedReaderError::Source(_)));
        assert!(Arc::ptr_eq(errors[0], errors[1]));
        assert!(Arc::ptr_eq(errors[0], errors[2]));
        assert_eq!(source.armed_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shared_batch_preserves_encryption_for_revisions_two_through_six() {
        for revision in 2..=6 {
            let pdf = encrypted_pdf(revision, "owner", "user");
            let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
            let resolved = reader.resolve_many_shared(&[(2, 0), (1, 0), (2, 0)]);
            assert_eq!(resolved.len(), 2);
            assert_eq!(
                resolved[0].1.as_ref().unwrap().as_stream().unwrap().content,
                b"encrypted stream"
            );
            assert_eq!(resolved[1].1.as_ref().unwrap().as_str().unwrap(), b"encrypted string");
        }

        let (pdf, image_plaintext, _) = encrypted_object_stream_pdf();
        let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        let resolved = reader.resolve_many_shared(&[(10, 0), (20, 0), (21, 0)]);
        assert_eq!(
            resolved[0]
                .1
                .as_ref()
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Text")
                .unwrap()
                .as_str()
                .unwrap(),
            b"member secret"
        );
        assert_eq!(
            resolved[1].1.as_ref().unwrap().as_stream().unwrap().content,
            image_plaintext
        );
        assert_eq!(resolved[2].1.as_ref().unwrap().as_str().unwrap(), b"normal secret");
    }

    #[test]
    fn shared_batch_limit_errors_match_scalar_and_types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<IndexedReader>();
        assert_send_sync::<Arc<Object>>();

        let large = format!("({})", "x".repeat(16 * 1_024));
        let (first, decoded) = object_stream_content(&[(10, large.as_bytes()), (11, b"(small)")]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        let compressed = encoder.finish().unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 2 /First {first} /Filter /FlateDecode"),
            &compressed,
            &[(10, 0), (11, 1)],
        );
        let reader = open_reader(
            &fixture.pdf,
            ResolverLimits {
                max_stream_bytes: 1_024,
                ..ResolverLimits::default()
            },
        );
        let batch = reader.resolve_many_shared(&[(11, 0), (10, 0)]);
        for (_, result) in batch {
            assert!(matches!(
                result.as_ref().unwrap_err().as_ref(),
                IndexedReaderError::ObjectStreamBatchSetup { .. }
            ));
        }
        assert!(matches!(
            reader.resolve_object((11, 0)),
            Err(IndexedReaderError::ObjectStreamMember { .. })
        ));
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
                assert_eq!(
                    reader.resolve_object(id).unwrap(),
                    eager.get_object(id).unwrap().clone()
                );
            }
            assert_eq!(
                reader.resolve_object((999, 0)).is_err(),
                eager.get_object((999, 0)).is_err()
            );
        }
    }

    #[test]
    fn compressed_member_enforces_container_index_id_generation_and_shape() {
        let (first, content) = object_stream_content(&[(10, b"(ten)"), (11, b"(eleven)")]);
        let wrong_index = object_stream_fixture(&format!("/Type /ObjStm /N 2 /First {first}"), &content, &[(10, 1)]);
        let reader = open_reader(&wrong_index.pdf, ResolverLimits::default());
        assert!(matches!(
            reader.resolve_object((10, 0)),
            Err(IndexedReaderError::ObjectStreamMember {
                id: (10, 0),
                container: (5, 0),
                index: 1,
                ..
            })
        ));
        assert!(matches!(
            reader.resolve_object((10, 1)),
            Err(IndexedReaderError::GenerationMismatch {
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
        Arc::get_mut(&mut reader.index)
            .unwrap()
            .locations
            .insert(10, ObjectLocation64::Compressed { container: 5, index: 0 });
        assert!(matches!(
            reader.resolve_object((10, 0)),
            Err(IndexedReaderError::ObjectStreamContainerNotStream {
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
                    .resolve_object((10, 0))
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
            assert_eq!(
                reader.resolve_object(id).unwrap(),
                eager.get_object(id).unwrap().clone()
            );
        }

        let malformed = [
            ("/Type /ObjStm /N 1 /First 1", b"10 0 (ten)".as_slice()),
            ("/Type /ObjStm /N 1 /First 5", b"10 0 << /Broken".as_slice()),
        ];
        for (dictionary, content) in malformed {
            let fixture = object_stream_fixture(dictionary, content, &[(10, 0)]);
            assert!(matches!(
                open_reader(&fixture.pdf, ResolverLimits::default()).resolve_object((10, 0)),
                Err(IndexedReaderError::ObjectStreamMember { .. })
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
            reader.resolve_object((10, 0)),
            Err(IndexedReaderError::ObjectStreamMember { .. })
        ));
    }

    #[test]
    fn encrypted_and_object_stream_errors_are_deterministic_across_threads() {
        let encrypted = encrypted_page_tree_pdf();
        for _ in 0..3 {
            let mut threads = Vec::new();
            for _ in 0..4 {
                let encrypted = encrypted.clone();
                threads.push(std::thread::spawn(move || {
                    matches!(
                        open_encrypted(&encrypted, Some(b"wrong")),
                        Err(IndexedReaderError::InvalidPassword)
                    )
                }));
            }
            assert!(threads.into_iter().all(|thread| thread.join().unwrap()));
        }
        let authenticated = open_encrypted(&encrypted, Some(b"user")).unwrap();
        assert!(authenticated.is_encrypted());
        assert!(authenticated.is_authenticated());
        assert_eq!(authenticated.page_count().unwrap(), 2);
        assert_eq!(authenticated.source_len(), u64::try_from(encrypted.len()).unwrap());

        let malformed = object_stream_fixture("/Type /ObjStm /N 1 /First 5", b"10 0 << /Broken", &[(10, 0)]);
        let reader = Arc::new(open_reader(&malformed.pdf, ResolverLimits::default()));
        let classify = |reader: &IndexedReader| match reader.resolve_object((10, 0)).unwrap_err() {
            IndexedReaderError::ObjectStreamMember {
                id,
                container,
                index,
                source,
            } => (id, container, index, source.to_string()),
            other => panic!("unexpected object-stream error: {other}"),
        };
        let expected = classify(&reader);
        for _ in 0..3 {
            let mut threads = Vec::new();
            for _ in 0..4 {
                let reader = Arc::clone(&reader);
                threads.push(std::thread::spawn(move || classify(&reader)));
            }
            assert!(threads.into_iter().all(|thread| thread.join().unwrap() == expected));
        }
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
                reader.resolve_object((id, 0)).unwrap(),
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
            .resolve_object((1, 0))
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
    fn parsed_normal_header_is_authoritative_over_xref_generation() {
        let definitions = [
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 1,
                body: b"<< /Type /Catalog /Pages 2 0 R >>",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Type /Page >>",
            },
        ];
        let pdf = object_pdf(&definitions);
        let eager = Document::load_mem(&pdf).unwrap();
        assert!(eager.get_object((1, 0)).is_ok());
        assert!(matches!(
            eager.get_object((1, 1)),
            Err(crate::Error::ObjectNotFound((1, 1)))
        ));
        assert_eq!(eager.page_iter().collect::<Vec<_>>(), vec![(3, 0)]);

        let reader = Arc::new(open_reader(&pdf, ResolverLimits::default()));
        for _ in 0..3 {
            assert_eq!(
                reader.resolve_object((1, 0)).unwrap(),
                eager.get_object((1, 0)).unwrap().clone()
            );
            assert_eq!(
                *reader.resolve_object_shared((1, 0)).unwrap(),
                eager.get_object((1, 0)).unwrap().clone()
            );
            assert_eq!(reader.page_map().unwrap().pages[0].id, (3, 0));
            assert!(matches!(
                reader.resolve_object((1, 1)),
                Err(IndexedReaderError::MissingNormalObjectAtXref {
                    id: (1, 1),
                    reason: MissingNormalObjectReason::GenerationMismatch {
                        requested: (1, 1),
                        indexed: 1,
                        actual: (1, 0)
                    }
                })
            ));
            assert!(matches!(
                reader.resolve_object_shared((1, 1)).unwrap_err().as_ref(),
                IndexedReaderError::MissingNormalObjectAtXref {
                    id: (1, 1),
                    reason: MissingNormalObjectReason::GenerationMismatch {
                        requested: (1, 1),
                        indexed: 1,
                        actual: (1, 0)
                    }
                }
            ));
        }

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let reader = Arc::clone(&reader);
                std::thread::spawn(move || {
                    (
                        reader.resolve_object((1, 0)).unwrap(),
                        reader.page_map().unwrap().pages[0].id,
                        reader.resolve_object((1, 1)).unwrap_err().to_string(),
                    )
                })
            })
            .collect();
        let expected_missing = reader.resolve_object((1, 1)).unwrap_err().to_string();
        for thread in threads {
            let (object, page, missing) = thread.join().unwrap();
            assert_eq!(object, eager.get_object((1, 0)).unwrap().clone());
            assert_eq!(page, (3, 0));
            assert_eq!(missing, expected_missing);
        }

        let missing_root = object_pdf_with_root(&definitions, (1, 1));
        let eager = Document::load_mem(&missing_root).unwrap();
        assert!(eager.page_iter().next().is_none());
        let reader = open_reader(&missing_root, ResolverLimits::default());
        assert!(matches!(
            reader.resolve_object((1, 1)),
            Err(IndexedReaderError::MissingNormalObjectAtXref {
                id: (1, 1),
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: (1, 1),
                    indexed: 1,
                    actual: (1, 0)
                }
            })
        ));
        assert!(reader.page_map().unwrap().is_empty());

        let mismatch = xref_number_header_mismatch_fixture();
        let eager = Document::load_mem(&mismatch).unwrap();
        assert!(matches!(
            eager.get_object((1, 0)),
            Err(crate::Error::ObjectNotFound((1, 0)))
        ));
        let reader = open_reader(&mismatch, ResolverLimits::default());
        assert!(matches!(
            reader.resolve_object((1, 0)),
            Err(IndexedReaderError::MissingNormalObjectAtXref {
                id: (1, 0),
                reason: MissingNormalObjectReason::HeaderMismatch {
                    expected: (1, 0),
                    actual: (2, 0)
                }
            })
        ));
        assert!(reader.page_map().unwrap().is_empty());
    }

    #[test]
    fn raw_compressed_generation_mismatch_is_not_a_semantic_page_map_omission() {
        let (first, content) = object_stream_content(&[(10, b"<< /Type /Catalog >>")]);
        let mut fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
        let marker = b"/Root 10 0 R";
        let root = fixture
            .pdf
            .windows(marker.len())
            .position(|window| window == marker)
            .unwrap();
        fixture.pdf[root + b"/Root 10 ".len()] = b'1';

        let eager = Document::load_mem(&fixture.pdf).unwrap();
        assert!(eager.page_iter().next().is_none());
        let reader = open_reader(&fixture.pdf, ResolverLimits::default());
        assert!(matches!(
            reader.resolve_object((10, 1)),
            Err(IndexedReaderError::GenerationMismatch {
                id: (10, 1),
                indexed: 0
            })
        ));
        assert!(matches!(
            reader.page_map(),
            Err(IndexedReaderError::GenerationMismatch {
                id: (10, 1),
                indexed: 0
            })
        ));
    }

    #[test]
    fn malformed_normal_xref_header_probe_matches_eager_object_omission() {
        let pdf = nul_header_probe_fixture();
        let eager = Document::load_mem(&pdf).unwrap();
        assert!(matches!(
            eager.get_object((1, 0)),
            Err(crate::Error::ObjectNotFound((1, 0)))
        ));

        let classify = |error: &IndexedReaderError| match error {
            IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit { offset, limit },
            } => (*id, *offset, *limit),
            other => panic!("unexpected malformed-normal-object error: {other}"),
        };
        let reader = Arc::new(open_reader(&pdf, ResolverLimits::default()));
        let expected = classify(&reader.resolve_object((1, 0)).unwrap_err());
        assert_eq!(expected.0, (1, 0));
        assert_eq!(expected.2, INDIRECT_HEADER_LIMIT);
        for _ in 0..3 {
            assert_eq!(classify(&reader.resolve_object((1, 0)).unwrap_err()), expected);
            assert_eq!(
                classify(reader.resolve_object_shared((1, 0)).unwrap_err().as_ref()),
                expected
            );
        }
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let reader = Arc::clone(&reader);
                std::thread::spawn(move || classify(&reader.resolve_object((1, 0)).unwrap_err()))
            })
            .collect();
        assert!(threads.into_iter().all(|thread| thread.join().unwrap() == expected));
    }

    #[test]
    fn xref_offset_inside_object_body_matches_eager_object_omission() {
        const NASA_OBJECT_ID: u32 = 34_472;
        // The observed file records an offset exactly twelve bytes after the
        // matching `34472 0 obj` header, at the first byte of the object body.
        let pdf = malformed_normal_xref_fixture(NASA_OBJECT_ID, 12, b"[/Indexed 34471 0 R 255 34473 0 R]");
        let eager = Document::load_mem(&pdf).unwrap();
        assert!(matches!(
            eager.get_object((NASA_OBJECT_ID, 0)),
            Err(crate::Error::ObjectNotFound((NASA_OBJECT_ID, 0)))
        ));

        let reader = open_reader(&pdf, ResolverLimits::default());
        for _ in 0..3 {
            assert!(matches!(
                reader.resolve_object((NASA_OBJECT_ID, 0)),
                Err(IndexedReaderError::MissingNormalObjectAtXref {
                    id: (NASA_OBJECT_ID, 0),
                    reason: MissingNormalObjectReason::HeaderProbeLimit { .. }
                })
            ));
            assert!(matches!(
                reader.resolve_object_shared((NASA_OBJECT_ID, 0)).unwrap_err().as_ref(),
                IndexedReaderError::MissingNormalObjectAtXref {
                    id: (NASA_OBJECT_ID, 0),
                    reason: MissingNormalObjectReason::HeaderProbeLimit { .. }
                }
            ));
        }
    }

    #[test]
    fn scalar_before_reference_in_array_matches_eager_nasa_shape() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"[/Indexed 2 0 R 255 3 0 R]",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"/DeviceRGB",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 0 >>\nstream\n\nendstream",
            },
        ]);
        let eager = Document::load_mem(&pdf).unwrap();
        let reader = open_reader(&pdf, ResolverLimits::default());
        assert_eq!(
            reader.resolve_object((1, 0)).unwrap(),
            eager.get_object((1, 0)).unwrap().clone()
        );
        assert!(matches!(
            reader.resolve_stream_descriptor((1, 0)),
            Err(IndexedStreamReadError::NotStream { id: (1, 0) })
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
            let resolved = reader.resolve_object(id).unwrap();
            assert_eq!(resolved.as_stream().unwrap().content, expected);
            assert_eq!(
                resolved.as_stream().unwrap().content,
                eager.get_object(id).unwrap().as_stream().unwrap().content
            );
        }
    }

    fn read_all_encoded(descriptor: &IndexedStreamDescriptor, chunk_bytes: usize) -> Vec<u8> {
        let mut reader = descriptor.open_plain_encoded().unwrap();
        let mut chunk = vec![0; chunk_bytes];
        let mut bytes = Vec::new();
        loop {
            let read = reader.read_chunk(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        bytes
    }

    #[test]
    fn stream_descriptors_distinguish_known_zero_from_unavailable_lengths() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Kind /Direct >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 3 0 R /Kind /Indirect >>\nstream\nworld\nendstream",
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
                body: b"<< /Length 0 /Kind /Zero >>\nstream\n\nendstream",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Kind /Missing >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 6,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length (bad) /Kind /Malformed >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 7,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 8 0 R /Kind /Cycle >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 8,
                object_generation: 0,
                xref_generation: 0,
                body: b"7 0 R",
            },
            ObjectDef {
                id: 9,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 99 >>\nstream\nshort\nendstream",
            },
            ObjectDef {
                id: 10,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 99 0 R /Kind /Dangling >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 11,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 12 0 R /Kind /Depth >>\nstream\nignored\nendstream",
            },
            ObjectDef {
                id: 12,
                object_generation: 0,
                xref_generation: 0,
                body: b"13 0 R",
            },
            ObjectDef {
                id: 13,
                object_generation: 0,
                xref_generation: 0,
                body: b"7",
            },
            ObjectDef {
                id: 14,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Kind /MissingEndstream >>\nstream\nhello",
            },
        ]);
        let eager = Document::load_mem(&pdf).unwrap();
        let source = Arc::new(LengthTracingBytesSource {
            bytes: pdf.clone(),
            len_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        for id in [1, 2, 4] {
            let scalar = reader.resolve_object((id, 0)).unwrap();
            let scalar = scalar.as_stream().unwrap();
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
            assert_eq!(
                descriptor.dictionary().get(b"Kind").unwrap(),
                scalar.dict.get(b"Kind").unwrap()
            );
            let expected = u64::try_from(scalar.content.len()).unwrap();
            assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Known(expected));
            assert_eq!(descriptor.encoded_len(), Some(expected));
            assert_eq!(read_all_encoded(&descriptor, 3), scalar.content);
            assert_eq!(
                reader.resolve_object((id, 0)).unwrap(),
                eager.get_object((id, 0)).unwrap().clone()
            );
        }
        for id in [5, 6, 7, 10] {
            let scalar = reader.resolve_object((id, 0)).unwrap();
            assert!(scalar.as_stream().unwrap().content.is_empty());
            assert_eq!(scalar, eager.get_object((id, 0)).unwrap().clone());
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
            let reason = EncodedStreamLengthUnavailableReason::MissingOrInvalid;
            assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Unavailable(reason));
            assert_eq!(descriptor.encoded_len(), None);
            source.requests.lock().unwrap().clear();
            source.len_calls.store(0, Ordering::SeqCst);
            assert!(matches!(
                descriptor.open_plain_encoded(),
                Err(IndexedStreamReadError::LengthUnavailable {
                    id: error_id,
                    reason: EncodedStreamLengthUnavailableReason::MissingOrInvalid,
                }) if error_id == (id, 0)
            ));
            assert_eq!(source.len_calls.load(Ordering::SeqCst), 0);
            assert!(source.requests.lock().unwrap().is_empty());
        }
        assert_eq!(
            reader
                .resolve_stream_descriptor((2, 0))
                .unwrap()
                .dictionary()
                .get(b"Length")
                .unwrap(),
            &Object::Reference((3, 0))
        );
        assert!(matches!(reader.resolve_object((9, 0)).unwrap(), Object::Dictionary(_)));
        assert!(matches!(
            reader.resolve_stream_descriptor((9, 0)),
            Err(IndexedStreamReadError::NotStream { id: (9, 0) })
        ));
        assert!(matches!(reader.resolve_object((14, 0)).unwrap(), Object::Dictionary(_)));
        assert!(matches!(
            reader.resolve_stream_descriptor((14, 0)),
            Err(IndexedStreamReadError::NotStream { id: (14, 0) })
        ));
        assert!(matches!(
            reader.resolve_stream_descriptor((3, 0)),
            Err(IndexedStreamReadError::NotStream { id: (3, 0) })
        ));
        assert!(matches!(
            reader.resolve_stream_descriptor((99, 0)),
            Err(IndexedStreamReadError::Resolve(
                IndexedReaderError::MissingNormalObject { id: (99, 0) }
            ))
        ));

        let limited_source = Arc::new(LengthTracingBytesSource {
            bytes: pdf,
            len_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let limited = IndexedReader::open_with_limits(
            limited_source.clone(),
            ResolverLimits {
                max_length_depth: 1,
                ..ResolverLimits::default()
            },
        )
        .unwrap();
        let scalar = limited.resolve_object((11, 0)).unwrap();
        assert!(scalar.as_stream().unwrap().content.is_empty());
        let descriptor = limited.resolve_stream_descriptor((11, 0)).unwrap();
        let reason = EncodedStreamLengthUnavailableReason::MissingOrInvalid;
        assert_eq!(descriptor.encoded_length(), EncodedStreamLength::Unavailable(reason));
        limited_source.requests.lock().unwrap().clear();
        limited_source.len_calls.store(0, Ordering::SeqCst);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::LengthUnavailable {
                id: (11, 0),
                reason: EncodedStreamLengthUnavailableReason::MissingOrInvalid,
            })
        ));
        assert_eq!(limited_source.len_calls.load(Ordering::SeqCst), 0);
        assert!(limited_source.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn stream_descriptors_preserve_scalar_resource_and_endstream_errors() {
        let pdf = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length -1 >>\nstream\n\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 20 >>\nstream\n01234567890123456789\nendstream",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 >>\nstream\nhello\nendst",
            },
        ]);
        let reader = open_reader(
            &pdf,
            ResolverLimits {
                max_stream_bytes: 10,
                max_endstream_tail_bytes: 6,
                ..ResolverLimits::default()
            },
        );
        for id in [1, 2, 3] {
            let scalar = reader.resolve_object((id, 0)).unwrap_err().to_string();
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap_err().to_string();
            assert!(
                descriptor.contains(&scalar),
                "scalar={scalar:?}, descriptor={descriptor:?}"
            );
        }
    }

    #[test]
    fn stream_descriptor_rejects_objstm_and_protected_payloads_before_open_read() {
        let (first, content) = object_stream_content(&[(10, b"(member)")]);
        let fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
        let reader = open_reader(&fixture.pdf, ResolverLimits::default());
        assert!(matches!(
            reader.resolve_stream_descriptor((10, 0)),
            Err(IndexedStreamReadError::NotNormalObject { id: (10, 0) })
        ));

        let filters = object_pdf(&[
            ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 2 0 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 2,
                object_generation: 0,
                xref_generation: 0,
                body: b"/Crypt",
            },
            ObjectDef {
                id: 3,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 4 0 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 4,
                object_generation: 0,
                xref_generation: 0,
                body: b"[/FlateDecode 5 0 R]",
            },
            ObjectDef {
                id: 5,
                object_generation: 0,
                xref_generation: 0,
                body: b"/Crypt",
            },
            ObjectDef {
                id: 6,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter [7 0 R /FlateDecode] >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 7,
                object_generation: 0,
                xref_generation: 0,
                body: b"/ASCIIHexDecode",
            },
            ObjectDef {
                id: 8,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 99 0 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 9,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 10 0 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 10,
                object_generation: 0,
                xref_generation: 0,
                body: b"9 0 R",
            },
            ObjectDef {
                id: 11,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 12 1 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 12,
                object_generation: 0,
                xref_generation: 0,
                body: b"/Crypt",
            },
            ObjectDef {
                id: 13,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 42 >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 14,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter /Crypt /DecodeParms << /Name /Identity >> >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 15,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter [16 0 R /FlateDecode] >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 16,
                object_generation: 0,
                xref_generation: 0,
                body: b"/Crypt",
            },
            ObjectDef {
                id: 17,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter [/FlateDecode 99 0 R] >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 18,
                object_generation: 0,
                xref_generation: 0,
                body: b"<< /Length 5 /Filter 19 0 R >>\nstream\nhello\nendstream",
            },
            ObjectDef {
                id: 19,
                object_generation: 0,
                xref_generation: 0,
                body: b"[/FlateDecode /ASCIIHexDecode]",
            },
        ]);
        let reader = open_reader(&filters, ResolverLimits::default());
        for id in [1, 3, 14, 15] {
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
            assert_eq!(descriptor.protection(), EncodedStreamProtection::CryptFilter);
            assert!(matches!(
                descriptor.open_plain_encoded(),
                Err(IndexedStreamReadError::Protected {
                    protection: EncodedStreamProtection::CryptFilter,
                    ..
                })
            ));
        }
        for id in [8, 9, 11, 13, 17] {
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
            assert_eq!(descriptor.protection(), EncodedStreamProtection::UnresolvedFilter);
            assert!(matches!(
                descriptor.open_plain_encoded(),
                Err(IndexedStreamReadError::Protected {
                    protection: EncodedStreamProtection::UnresolvedFilter,
                    ..
                })
            ));
        }
        for id in [6, 18] {
            let descriptor = reader.resolve_stream_descriptor((id, 0)).unwrap();
            assert_eq!(descriptor.protection(), EncodedStreamProtection::Plain);
            assert_eq!(read_all_encoded(&descriptor, 64 * 1_024), b"hello");
        }

        for revision in 2..=6 {
            let pdf = encrypted_pdf(revision, "owner", "user");
            let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
            let descriptor = reader.resolve_stream_descriptor((2, 0)).unwrap();
            assert!(descriptor.dictionary().has_type(b"Metadata"));
            assert_eq!(descriptor.protection(), EncodedStreamProtection::DocumentEncrypted);
            assert!(matches!(
                descriptor.open_plain_encoded(),
                Err(IndexedStreamReadError::Protected {
                    protection: EncodedStreamProtection::DocumentEncrypted,
                    ..
                })
            ));
        }
    }

    #[test]
    fn stream_descriptor_never_reads_compressed_filter_metadata_before_lease() {
        const OBJECT_ID: u32 = 1;
        const CONTAINER_ID: u32 = 5;
        const XREF_ID: u32 = 6;
        const FILTER_ID: u32 = 10;

        let object_offset = 1_024_u64;
        let object =
            format!("{OBJECT_ID} 0 obj\n<< /Length 5 /Filter {FILTER_ID} 0 R >>\nstream\nhello\nendstream\nendobj\n")
                .into_bytes();

        let container_offset = 1_024_u64 * 1_024;
        let container_stream_length = 64_u64 * 1_024 * 1_024 - 1;
        let container_prefix = format!(
            "{CONTAINER_ID} 0 obj\n<< /Type /ObjStm /N 1 /First 5 /Length {container_stream_length} >>\nstream\n"
        )
        .into_bytes();
        let container_stream_start = container_offset + u64::try_from(container_prefix.len()).unwrap();
        let container_stream_end = container_stream_start + container_stream_length;
        let container_suffix = b"\nendstream\nendobj\n".to_vec();

        let xref_offset = container_stream_end + 1_024_u64 * 1_024;
        let mut xref_content = Vec::new();
        for id in 0..=FILTER_ID {
            match id {
                OBJECT_ID => {
                    encode_field(1, 1, &mut xref_content);
                    encode_field(object_offset, 8, &mut xref_content);
                    encode_field(0, 4, &mut xref_content);
                }
                CONTAINER_ID => {
                    encode_field(1, 1, &mut xref_content);
                    encode_field(container_offset, 8, &mut xref_content);
                    encode_field(0, 4, &mut xref_content);
                }
                XREF_ID => {
                    encode_field(1, 1, &mut xref_content);
                    encode_field(xref_offset, 8, &mut xref_content);
                    encode_field(0, 4, &mut xref_content);
                }
                FILTER_ID => {
                    encode_field(2, 1, &mut xref_content);
                    encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
                    encode_field(0, 4, &mut xref_content);
                }
                _ => {
                    encode_field(0, 1, &mut xref_content);
                    encode_field(0, 8, &mut xref_content);
                    encode_field(if id == 0 { 65_535 } else { 0 }, 4, &mut xref_content);
                }
            }
        }
        let mut xref = format!(
            "{XREF_ID} 0 obj\n<< /Type /XRef /Size {} /Root {OBJECT_ID} 0 R /W [1 8 4] /Length {} >>\nstream\n",
            FILTER_ID + 1,
            xref_content.len()
        )
        .into_bytes();
        xref.extend_from_slice(&xref_content);
        xref.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
        let source_len = xref_offset + u64::try_from(xref.len()).unwrap();
        let container_region_end = container_stream_end + u64::try_from(container_suffix.len()).unwrap();

        let source = Arc::new(OverlaySource {
            len: source_len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object),
                (container_offset, container_prefix),
                (container_stream_start, b"10 0 /Crypt".to_vec()),
                (container_stream_end, container_suffix),
                (xref_offset, xref),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let descriptor = reader.resolve_stream_descriptor((OBJECT_ID, 0)).unwrap();
        assert_eq!(descriptor.protection(), EncodedStreamProtection::UnresolvedFilter);
        assert!(matches!(
            descriptor.open_plain_encoded(),
            Err(IndexedStreamReadError::Protected {
                protection: EncodedStreamProtection::UnresolvedFilter,
                ..
            })
        ));

        let requests = source.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|(offset, length)| {
            let end = offset.saturating_add(u64::try_from(*length).unwrap_or(u64::MAX));
            end <= container_offset || *offset >= container_region_end
        }));
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

        let stream = open_reader(&pdf, ResolverLimits::default())
            .resolve_object((1, 0))
            .unwrap();
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
            let resolved = reader.resolve_object(id).unwrap();
            assert_eq!(&resolved, eager.get_object(id).unwrap());
            assert!(resolved.as_stream().unwrap().content.is_empty());
        }
        assert!(
            reader
                .resolve_object((7, 0))
                .unwrap()
                .as_stream()
                .unwrap()
                .content
                .is_empty()
        );
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
                default_reader.resolve_object(id),
                Err(IndexedReaderError::NegativeStreamLength { id: actual, length: -1 }) if actual == id
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
            limited_reader.resolve_object((4, 0)),
            Err(IndexedReaderError::StreamLimitExceeded {
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
            tail_limited_reader.resolve_object((4, 0)),
            Err(IndexedReaderError::MissingEndstream { id: (4, 0) })
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
            let resolved = reader.resolve_object(id).unwrap();
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
            reader.resolve_object((1, 0)),
            Err(IndexedReaderError::ObjectLimitExceeded { id: (1, 0), limit: 32 })
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

    struct LengthTracingBytesSource {
        bytes: Vec<u8>,
        len_calls: AtomicUsize,
        requests: Mutex<Vec<(u64, usize)>>,
    }

    struct SwitchableFailureSource {
        bytes: Vec<u8>,
        mode: AtomicU8,
    }

    struct FailOnceSource {
        bytes: Vec<u8>,
        armed: AtomicBool,
        armed_reads: AtomicUsize,
    }

    impl RandomAccessSource for FailOnceSource {
        fn len(&self) -> Result<u64, SourceError> {
            Ok(u64::try_from(self.bytes.len()).unwrap())
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
            if self.armed.load(Ordering::SeqCst) {
                self.armed_reads.fetch_add(1, Ordering::SeqCst);
                if self.armed.swap(false, Ordering::SeqCst) {
                    return Err(SourceError::Io(std::io::Error::other("one-shot positional failure")));
                }
            }
            let offset = usize::try_from(offset).unwrap();
            let length = output.len().min(self.bytes.len().saturating_sub(offset));
            output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
            Ok(length)
        }
    }

    impl RandomAccessSource for SwitchableFailureSource {
        fn len(&self) -> Result<u64, SourceError> {
            Ok(u64::try_from(self.bytes.len()).unwrap())
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
            match self.mode.load(Ordering::SeqCst) {
                1 => {
                    return Err(SourceError::Io(std::io::Error::other(
                        "injected positional read failure",
                    )));
                }
                2 => return Ok(0),
                _ => {}
            }
            let offset = usize::try_from(offset).unwrap();
            let read = output.len().min(self.bytes.len().saturating_sub(offset));
            output[..read].copy_from_slice(&self.bytes[offset..offset + read]);
            Ok(read)
        }
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

    impl RandomAccessSource for LengthTracingBytesSource {
        fn len(&self) -> Result<u64, SourceError> {
            self.len_calls.fetch_add(1, Ordering::SeqCst);
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
    fn page_map_propagates_source_and_resource_failures() {
        for mode in [1, 2] {
            let source = Arc::new(SwitchableFailureSource {
                bytes: classic_pdf(),
                mode: AtomicU8::new(0),
            });
            let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
            source.mode.store(mode, Ordering::SeqCst);
            assert!(matches!(
                PageMap::from_reader(&reader),
                Err(IndexedReaderError::Source(_))
            ));
        }

        let reader = IndexedReader::open_with_limits(
            Arc::new(BytesSource::from(classic_pdf())),
            ResolverLimits {
                max_object_bytes: 8,
                ..ResolverLimits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            PageMap::from_reader(&reader),
            Err(IndexedReaderError::ObjectLimitExceeded { limit: 8, .. })
        ));
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
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let first_value = reader.resolve_object((10, 0)).unwrap();
        let second_value = reader.resolve_object((10, 0)).unwrap();
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
    fn page_map_walk_is_bounded_on_a_sparse_hundred_megabyte_source() {
        let pdf = generated_page_tree_pdf(3, 999_999);
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

        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        let page_map = PageMap::from_reader(&reader).unwrap();
        assert_eq!(
            page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
            vec![(3, 0), (4, 0), (5, 0)]
        );

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
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

        let stream = reader.resolve_object((1, 0)).unwrap();
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
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let image = reader.resolve_object((1, 0)).unwrap();
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
    fn hundred_megabyte_stream_descriptor_has_bounded_lookahead_and_chunk_reads() {
        let source_len = 200_u64 * 1_024 * 1_024;
        let object_offset = 1_024_u64 * 1_024;
        let stream_length = 100_u64 * 1_024 * 1_024;
        let object_prefix =
            format!("1 0 obj\n<< /Type /XObject /Subtype /Image /Length {stream_length} >>\nstream\n").into_bytes();
        let stream_start = object_offset + u64::try_from(object_prefix.len()).unwrap();
        let stream_end = stream_start + stream_length;
        let xref = source_len - 512;
        let xref_bytes = format!(
            "xref\n0 2\n0000000000 65535 f \n{object_offset:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes();
        let source = Arc::new(OverlaySource {
            len: source_len,
            regions: vec![
                (0, b"%PDF-1.7\n".to_vec()),
                (object_offset, object_prefix),
                (stream_end, b"\nendstream\nendobj\n".to_vec()),
                (xref, xref_bytes),
            ],
            requests: Mutex::new(Vec::new()),
        });
        let reader = IndexedReader::open_with_limits(
            source.clone(),
            ResolverLimits {
                max_stream_bytes: 128 * 1_024 * 1_024,
                ..ResolverLimits::default()
            },
        )
        .unwrap();
        source.requests.lock().unwrap().clear();

        let descriptor = reader.resolve_stream_descriptor((1, 0)).unwrap();
        assert_eq!(descriptor.encoded_len(), Some(stream_length));
        let metadata_requests = source.requests.lock().unwrap().clone();
        assert!(metadata_requests.iter().all(|(_, length)| *length <= 64 * 1_024));
        assert!(
            !metadata_requests
                .iter()
                .any(|(offset, length)| *offset == stream_start && u64::try_from(*length).unwrap() == stream_length)
        );
        let lookahead: u64 = metadata_requests
            .iter()
            .filter_map(|(offset, length)| {
                let end = offset.checked_add(u64::try_from(*length).ok()?)?;
                (*offset < stream_start && end > stream_start).then_some(end - stream_start)
            })
            .sum();
        assert!(lookahead <= 64 * 1_024);

        source.requests.lock().unwrap().clear();
        let mut encoded = descriptor.open_plain_encoded().unwrap();
        let mut output = vec![0xff; 128 * 1_024];
        assert_eq!(encoded.read_chunk(&mut output).unwrap(), 64 * 1_024);
        assert!(output[..64 * 1_024].iter().all(|byte| *byte == 0));
        assert!(
            source
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, length)| *length <= 64 * 1_024)
        );
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
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();

        let error = reader.resolve_object((1, 0)).unwrap_err();
        assert!(
            matches!(error, IndexedReaderError::InvalidIndirectObject { id: (1, 0), .. }),
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
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));

        let stream = reader.resolve_object((1, 0)).unwrap();
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
            Err(IndexedReaderError::RevisionLimitExceeded {
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
            Err(IndexedReaderError::InvalidXref { .. })
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
            Err(IndexedReaderError::InvalidIndirectObject { .. })
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
            b"[/Indexed 34471 0 R 255 34473 0 R]\rendobj".as_slice(),
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
            let indexed = open_reader(&pdf, ResolverLimits::default())
                .resolve_object((1, 0))
                .unwrap();
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
                .resolve_object((1, 0))
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
            open_reader(&pdf, ResolverLimits::default()).resolve_object((1, 0)),
            Err(IndexedReaderError::InvalidIndirectObject { .. })
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
    fn zero_padded_generation_boundary_continues_numbers_without_replay() {
        let cases = [
            (b"65535".as_slice(), Object::Integer(65_535)),
            (b"65535.3".as_slice(), Object::Real(65_535.3)),
            (b"65536".as_slice(), Object::Integer(65_536)),
            (b"65536.3".as_slice(), Object::Real(65_536.3)),
            (b"-65536.3".as_slice(), Object::Real(-65_536.3)),
            (b"+65536.3".as_slice(), Object::Real(65_536.3)),
        ];

        for (suffix, value) in cases {
            let mut body = b"[1 ".to_vec();
            let sign = suffix.first().copied().filter(|byte| matches!(byte, b'+' | b'-'));
            if let Some(sign) = sign {
                body.push(sign);
            }
            body.extend(std::iter::repeat_n(b'0', 256 * 1_024));
            body.extend_from_slice(&suffix[usize::from(sign.is_some())..]);
            body.extend_from_slice(b"] ");
            let expected = Object::Array(vec![Object::Integer(1), value]);
            assert_eq!(crate::parser::direct_object_with_consumed(&body).unwrap().1, expected);

            let number_end = body.len() - 2;
            let mut splits: Vec<_> = (OBJECT_GROWTH_CHUNK as usize..body.len())
                .step_by(OBJECT_GROWTH_CHUNK as usize)
                .collect();
            splits.extend(number_end.saturating_sub(8)..=number_end);
            splits.sort_unstable();
            splits.dedup();
            for split in splits {
                let mut framer = DirectObjectFramer::new();
                assert_ne!(framer.advance(&body[..split]), FrameStatus::Invalid, "split {split}");
                assert_eq!(framer.advance(&body), FrameStatus::Ready, "split {split}");
                assert!(
                    framer.scanned_work <= body.len() + 6,
                    "suffix={suffix:?} split={split} work={} input={}",
                    framer.scanned_work,
                    body.len()
                );
            }
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
        let reader = IndexedReader::open_with_limits(
            source.clone(),
            ResolverLimits {
                max_object_bytes: 3 * 1_024 * 1_024,
                ..ResolverLimits::default()
            },
        )
        .unwrap();
        source.requests.lock().unwrap().clear();
        OBJECT_BODY_PARSE_CALLS.with(|calls| calls.set(0));
        assert_eq!(
            reader.resolve_object((1, 0)).unwrap().as_str().unwrap().len(),
            literal_length
        );
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
            Err(IndexedReaderError::InvalidXref { .. })
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
                Err(IndexedReaderError::InvalidXref { .. })
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
                Err(IndexedReaderError::InvalidXref { .. })
            ));
        }
    }

    #[test]
    fn bounded_scalar_holds_one_allowance_through_parse_and_measurement() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Value [1 /Name (text)] >>",
        }]);
        let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap();
        assert!(matches!(scalar.as_object(), Object::Dictionary(_)));
        assert!(scalar.retained_bytes() > 0);
        assert!(scalar.peak_bytes() <= permit.limit_bytes());
        assert_eq!(permit.stats().current_bytes, scalar.retained_bytes());
        drop(scalar);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }

    #[test]
    fn bounded_scalar_one_byte_below_observed_peak_refuses_before_ast_parse() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Value [1 /Name (text)] >>",
        }]);
        let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
        let generous = crate::ScalarResolutionPermit::new(1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((1, 0), &generous).unwrap();
        let peak = scalar.peak_bytes();
        drop(scalar);
        generous.close().unwrap();

        let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
        let limited = crate::ScalarResolutionPermit::new(peak - 1);
        assert!(matches!(
            reader.resolve_scalar_with_permit((1, 0), &limited),
            Err(IndexedReaderError::ScalarResourceLimit {
                phase: "scalar-ast-envelope",
                ..
            })
        ));
        assert_eq!(limited.stats().current_bytes, 0);
        limited.close().unwrap();
    }

    #[test]
    fn bounded_scalar_refuses_a_scalar_larger_than_four_mib_and_releases_every_charge() {
        let mut body = Vec::with_capacity(4 * 1024 * 1024 + 3);
        body.push(b'(');
        body.resize(4 * 1024 * 1024 + 2, b'x');
        body.push(b')');
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: &body,
        }]);
        let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(matches!(
            reader.resolve_scalar_with_permit((1, 0), &permit),
            Err(IndexedReaderError::ScalarResourceLimit { .. }) | Err(IndexedReaderError::ObjectLimitExceeded { .. })
        ));
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
    }

    #[test]
    fn bounded_scalar_cancelled_permit_never_allocates_and_closes_at_zero() {
        let pdf = object_pdf(&[ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog >>",
        }]);
        let reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        permit.cancel();
        assert!(matches!(
            reader.resolve_scalar_with_permit((1, 0), &permit),
            Err(IndexedReaderError::ScalarResolutionCancelled { .. })
        ));
        assert_eq!(permit.stats().peak_bytes, 0);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }

    #[test]
    fn bounded_compressed_scalar_accounts_encoded_decoded_and_ast_overlap() {
        let body = b"<< /Type /Catalog /Value [1 /Name (compressed)] >>";
        let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&plain).unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
            &encoder.finish().unwrap(),
            &[(10, 0)],
        );
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap();
        assert!(matches!(scalar.as_object(), Object::Dictionary(_)));
        assert!(scalar.peak_bytes() <= permit.limit_bytes());
        drop(scalar);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }

    #[test]
    fn bounded_compressed_scalar_rejects_large_inflation_and_releases_to_zero() {
        let mut body = Vec::with_capacity(2 * 1024 * 1024 + 2);
        body.push(b'(');
        body.resize(2 * 1024 * 1024 + 1, b'x');
        body.push(b')');
        let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&plain).unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode"),
            &encoder.finish().unwrap(),
            &[(10, 0)],
        );
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(reader.resolve_scalar_with_permit((10, 0), &permit).is_err());
        assert!(permit.stats().peak_bytes <= permit.limit_bytes());
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
    }

    #[test]
    fn bounded_compressed_scalar_refuses_predictor_before_encoded_allocation() {
        let body = b"<< /Type /Catalog >>";
        let (first, plain) = object_stream_content(&[(10, body.as_slice())]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&plain).unwrap();
        let fixture = object_stream_fixture(
            &format!("/Type /ObjStm /N 1 /First {first} /Filter /FlateDecode /DecodeParms << /Predictor 12 >>"),
            &encoder.finish().unwrap(),
            &[(10, 0)],
        );
        let reader = IndexedReader::open(BytesSource::from(fixture.pdf)).unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(matches!(
            reader.resolve_scalar_with_permit((10, 0), &permit),
            Err(IndexedReaderError::UnsupportedBoundedScalar { .. })
        ));
        assert_eq!(permit.stats().peak_bytes, 0);
        permit.close().unwrap();
    }

    #[test]
    fn bounded_scalar_never_issues_a_physical_read_over_sixty_four_kib() {
        let mut body = Vec::with_capacity(128 * 1024 + 2);
        body.push(b'(');
        body.resize(128 * 1024 + 1, b'x');
        body.push(b')');
        let source = Arc::new(TracingBytesSource {
            bytes: object_pdf(&[ObjectDef {
                id: 1,
                object_generation: 0,
                xref_generation: 0,
                body: &body,
            }]),
            requests: Mutex::new(Vec::new()),
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();
        source.requests.lock().unwrap().clear();
        let permit = crate::ScalarResolutionPermit::new(64 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((1, 0), &permit).unwrap();
        assert_eq!(scalar.as_object().as_str().unwrap().len(), 128 * 1024);
        assert!(
            source
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, bytes)| *bytes <= 64 * 1024)
        );
        drop(scalar);
        permit.close().unwrap();
    }

    #[test]
    fn bounded_scalar_refuses_encryption_before_any_call_local_allocation() {
        let pdf = encrypted_pdf(2, "owner", "user");
        let reader = IndexedReader::open_with_options(
            BytesSource::from(pdf),
            IndexedReaderOptions {
                password: Some(b"user".to_vec()),
                ..IndexedReaderOptions::default()
            },
        )
        .unwrap();
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        assert!(matches!(
            reader.resolve_scalar_with_permit((1, 0), &permit),
            Err(IndexedReaderError::UnsupportedBoundedScalar {
                reason: "encrypted scalar objects",
                ..
            })
        ));
        assert_eq!(permit.stats().peak_bytes, 0);
        permit.close().unwrap();
    }
}
