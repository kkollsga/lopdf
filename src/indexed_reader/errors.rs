//! Error types and error-classification helpers for the indexed reader.

use super::*;

/// Result returned by the indexed random-access reader.
pub type IndexedReaderResult<T> = std::result::Result<T, IndexedReaderError>;

/// Shareable result returned by batched and shared object resolution.
pub type SharedIndexedReaderResult<T> = std::result::Result<T, Arc<IndexedReaderError>>;

/// Result returned while opening or reading a bounded encoded stream.
pub type IndexedStreamReadResult<T> = std::result::Result<T, IndexedStreamReadError>;

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
    /// The declared encoded span exceeds the caller-selected streaming
    /// workload cap. This cap is distinct from the retained/materialized
    /// stream limit used by ordinary object resolution.
    #[error("encoded stream in object {id:?} declares {length} bytes, exceeding the {limit}-byte streaming limit")]
    EncodedStreamLimitExceeded {
        id: crate::ObjectId,
        length: u64,
        limit: u64,
    },
    /// The source's reported length changed after the reader captured it.
    #[error("indexed source length changed: expected {expected}, found {actual}")]
    SourceLengthChanged { expected: u64, actual: u64 },
    /// A checked positional source read failed.
    #[error("indexed stream source error")]
    Source(#[from] SourceError),
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
    #[error("failed parsing cross reference table")]
    InvalidStartXref { limit: u64 },
    #[error("failed parsing cross reference table")]
    StartXrefOutOfBounds { offset: u64, logical_len: u64 },
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
    ObjectLimitExceeded {
        id: crate::ObjectId,
        limit: u64,
        provenance: ObjectLimitProvenance,
    },
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
    #[error("object {id:?} is not a stream object")]
    NotStreamObject { id: crate::ObjectId },
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
    #[error("decoded object-stream container {container:?} must use call-local bounded storage")]
    ObjectStreamCacheBypass { container: crate::ObjectId },
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
    #[error("page tree nests deeper than the {limit}-level limit")]
    PageTreeDepthLimitExceeded { limit: usize },
}

/// Typed proof for an [`IndexedReaderError::ObjectLimitExceeded`] result.
///
/// Downstream bounded caches use this field to distinguish immutable demand
/// observed by the object framer from defensive arithmetic failures without
/// parsing the stable public display string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ObjectLimitProvenance {
    /// The direct-object framer still required input at its configured maximum.
    FrameNeedMoreAtMaximum,
    /// The bounded scalar framer exhausted the available source while preserving
    /// its historical object-limit classification.
    SourceExhaustedAtMaximum,
    /// A platform conversion or checked arithmetic operation could not
    /// represent the otherwise bounded window.
    ArithmeticInvariant,
}

pub(super) fn object_limit_arithmetic(id: crate::ObjectId, limit: u64) -> IndexedReaderError {
    IndexedReaderError::ObjectLimitExceeded {
        id,
        limit,
        provenance: ObjectLimitProvenance::ArithmeticInvariant,
    }
}

pub(super) fn object_frame_at_maximum(id: crate::ObjectId, maximum: u64, remaining: u64) -> IndexedReaderError {
    IndexedReaderError::ObjectLimitExceeded {
        id,
        limit: maximum,
        provenance: if remaining > maximum {
            ObjectLimitProvenance::FrameNeedMoreAtMaximum
        } else {
            ObjectLimitProvenance::SourceExhaustedAtMaximum
        },
    }
}

pub(super) fn object_frame_need_more(
    id: crate::ObjectId, offset: u64, maximum: u64, remaining: u64,
) -> IndexedReaderError {
    if remaining > maximum {
        IndexedReaderError::ObjectLimitExceeded {
            id,
            limit: maximum,
            provenance: ObjectLimitProvenance::FrameNeedMoreAtMaximum,
        }
    } else {
        IndexedReaderError::IncompleteObject { id, offset }
    }
}

pub(super) fn is_transient_error(error: &IndexedReaderError) -> bool {
    match error {
        IndexedReaderError::Source(SourceError::SourceChanged) => false,
        IndexedReaderError::Source(_) => true,
        IndexedReaderError::ObjectStreamMember { source, .. }
        | IndexedReaderError::ObjectStreamBatchSetup { source, .. } => matches!(source, crate::Error::IO(_)),
        _ => false,
    }
}

fn clone_stable_object_stream_error(error: &crate::Error) -> Option<crate::Error> {
    match error {
        crate::Error::InvalidObjectStream(message) => Some(crate::Error::InvalidObjectStream(message.clone())),
        crate::Error::InvalidStream(message) => Some(crate::Error::InvalidStream(message.clone())),
        crate::Error::Decompress(crate::DecompressError::Ascii85(message)) => {
            Some(crate::Error::Decompress(crate::DecompressError::Ascii85(message)))
        }
        crate::Error::Decompress(crate::DecompressError::AsciiHex(message)) => {
            Some(crate::Error::Decompress(crate::DecompressError::AsciiHex(message)))
        }
        crate::Error::Decompress(crate::DecompressError::Predictor(message)) => {
            Some(crate::Error::Decompress(crate::DecompressError::Predictor(message)))
        }
        // MemoryLimitExceeded is permit-dependent and must never poison a
        // container-keyed negative cache.
        _ => None,
    }
}

/// Convert member-attributed failures to a container-neutral fingerprint before
/// publishing them under the container cache key.  A racing leader must not
/// decide which requested member future callers see in their error. This is an
/// explicit safe whitelist: permit-dependent limits, cancellation, resource
/// admission, I/O, and generic fallbacks are never retained.
pub(super) fn neutralize_cacheable_bounded_error(error: &IndexedReaderError) -> Option<IndexedReaderError> {
    match error {
        IndexedReaderError::ObjectStreamMember { container, source, .. } => {
            Some(IndexedReaderError::ObjectStreamBatchSetup {
                container: *container,
                source: clone_stable_object_stream_error(source)?,
            })
        }
        IndexedReaderError::ObjectStreamBatchSetup { container, source } => {
            Some(IndexedReaderError::ObjectStreamBatchSetup {
                container: *container,
                source: clone_stable_object_stream_error(source)?,
            })
        }
        IndexedReaderError::ObjectStreamContainerNotStream { container, .. } => {
            Some(IndexedReaderError::ObjectStreamContainerNotStream {
                id: *container,
                container: *container,
            })
        }
        IndexedReaderError::UnsupportedBoundedScalar { reason, .. }
            if matches!(
                *reason,
                "object-stream filter chains or decode parameters outside the bounded decode envelope"
                    | "object streams without a bounded nonnegative /Length"
            ) =>
        {
            Some(IndexedReaderError::UnsupportedBoundedScalar { id: (0, 0), reason })
        }
        IndexedReaderError::Source(SourceError::SourceChanged) => {
            Some(IndexedReaderError::Source(SourceError::SourceChanged))
        }
        _ => None,
    }
}

/// Reapply the current request's member identity to a container-neutral
/// negative-cache fingerprint.
pub(super) fn rewrap_cacheable_bounded_error(
    error: &IndexedReaderError, member_id: crate::ObjectId, member_index: u32,
) -> Option<IndexedReaderError> {
    match error {
        IndexedReaderError::ObjectStreamBatchSetup { container, source } => {
            Some(IndexedReaderError::ObjectStreamMember {
                id: member_id,
                container: *container,
                index: member_index,
                source: clone_stable_object_stream_error(source)?,
            })
        }
        IndexedReaderError::ObjectStreamContainerNotStream { container, .. } => {
            Some(IndexedReaderError::ObjectStreamContainerNotStream {
                id: member_id,
                container: *container,
            })
        }
        IndexedReaderError::UnsupportedBoundedScalar { reason, .. } => {
            Some(IndexedReaderError::UnsupportedBoundedScalar { id: member_id, reason })
        }
        IndexedReaderError::Source(SourceError::SourceChanged) => {
            Some(IndexedReaderError::Source(SourceError::SourceChanged))
        }
        _ => None,
    }
}

pub(super) enum StreamMetadataError {
    Reader(IndexedReaderError),
    EncodedLimit {
        id: crate::ObjectId,
        length: u64,
        limit: u64,
    },
}

impl StreamMetadataError {
    pub(super) fn into_reader_error(self) -> IndexedReaderError {
        match self {
            Self::Reader(error) => error,
            Self::EncodedLimit { id, length, limit } => IndexedReaderError::StreamLimitExceeded { id, length, limit },
        }
    }

    pub(super) fn into_stream_error(self) -> IndexedStreamReadError {
        match self {
            Self::Reader(error) => error.into(),
            Self::EncodedLimit { id, length, limit } => {
                IndexedStreamReadError::EncodedStreamLimitExceeded { id, length, limit }
            }
        }
    }
}

impl From<IndexedReaderError> for StreamMetadataError {
    fn from(error: IndexedReaderError) -> Self {
        Self::Reader(error)
    }
}
