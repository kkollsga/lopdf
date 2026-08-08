//! Random-access reading of a PDF without materializing the file.
//!
//! [`IndexedReader`] opens a document over a [`RandomAccessSource`] and answers
//! queries — a trailer entry, one object, a page map, a stream's bytes — by
//! reading only the ranges those queries need. It is an addition beside
//! [`Document`](crate::Document), not a replacement: `Document` still owns
//! whole-file loading, mutation and writing. This module exists for the case
//! where a caller wants a few objects out of a large file and cannot afford to
//! pay for the rest of it.
//!
//! # The model
//!
//! ## Bounded open
//!
//! Opening reads a fixed set of small windows and never scans the body:
//!
//! 1. **Header.** The first `HEADER_SCAN_LIMIT` bytes are searched for
//!    `%PDF-`. The match offset becomes the index's *source origin*, so a file
//!    with junk prepended is addressed by its own logical offsets, exactly as
//!    the eager reader addresses it.
//! 2. **`startxref`.** The last `TAIL_SCAN_LIMIT` bytes are read once and
//!    searched backwards for `%%EOF`, then for `startxref` before it.
//! 3. **Cross-reference walk.** Each section is parsed from a window that
//!    starts at `XREF_INITIAL_WINDOW` and grows to at most
//!    `XREF_WINDOW_LIMIT`; xref streams decompress under
//!    `XREF_DECOMPRESSED_LIMIT`. The walk follows `/Prev` until a revision
//!    repeats, bounded by `MAX_XREF_REVISIONS` revisions and
//!    `MAX_XREF_ENTRIES` entries. Newer revisions win. Within one revision a
//!    `/XRefStm` supplement takes precedence over the classic section it
//!    supplements, because a hybrid-reference file is required to mask its
//!    compressed objects as free in the classic table (ISO 32000-1, 7.5.8.4).
//!
//! What open does *not* do is as important: no object body is parsed, no
//! object stream is decoded, no page tree is walked. The result is a
//! `BTreeMap` of live xref entries plus the newest trailer. Free entries are
//! retained rather than dropped, so a later writer or recovery layer does not
//! have to reconstruct the free list.
//!
//! ## On-demand resolution
//!
//! Three shapes, all reading only the object's own bytes:
//!
//! - [`resolve_object`](IndexedReader::resolve_object) — one owned [`Object`].
//!   For a normal entry the framer probes at most `INDIRECT_HEADER_LIMIT`
//!   bytes for the `N G obj` header, then grows a window from
//!   `INITIAL_OBJECT_WINDOW` in `OBJECT_GROWTH_CHUNK` steps until the
//!   object parses or the per-object cap is reached.
//! - [`resolve_object_shared`](IndexedReader::resolve_object_shared) —
//!   `Arc<Object>` through the per-reader object cache. Concurrent calls for
//!   one id are single-flighted: one thread does the work, the others wait on
//!   the same cell.
//! - [`resolve_many_shared`](IndexedReader::resolve_many_shared) — a batch.
//!   Compressed ids are grouped by their `/ObjStm` container so each container
//!   is fetched, decoded and header-indexed once per call rather than once per
//!   member. Ordinary ids fan out across rayon when the feature is on.
//!
//! A `/Length` given as an indirect reference is itself resolved through this
//! path, under a depth cap ([`IndexedReaderOptions::reference_depth`]) and a
//! per-call cycle set.
//!
//! ## Chunked stream reading
//!
//! [`IndexedStreamDescriptor`] locates a stream's encoded payload without
//! reading it. For an unencrypted payload,
//! [`open_plain_encoded`](IndexedStreamDescriptor::open_plain_encoded) returns
//! an [`EncodedStreamReader`] whose
//! [`read_chunk`](EncodedStreamReader::read_chunk) yields the encoded bytes in
//! pieces of at most `ENCODED_STREAM_CHUNK_LIMIT`. A caller that wants to
//! feed a decoder incrementally never holds the whole payload, and the
//! streaming span cap ([`IndexedReaderOptions::encoded_stream_bytes`]) is
//! deliberately separate from the retained-bytes cap
//! ([`IndexedReaderOptions::stream_bytes`]) so admitting a large *span* does
//! not weaken the limit on materialized data.
//!
//! # Memory bounds
//!
//! - **No whole-file buffer.** [`FileSource`](crate::source::FileSource)
//!   satisfies every read positionally. Nothing in this module ever asks a
//!   source for its entire contents. [`BytesSource`](crate::source::BytesSource)
//!   exists for callers who already hold the bytes.
//! - **Physical reads are split.** Through `CachedSource` every request is
//!   decomposed into aligned chunks of
//!   `SOURCE_CHUNK_BYTES` (64 KiB),
//!   so one logical read of a large window is many small reads against a
//!   bounded pool rather than one large allocation.
//! - **Caches are per reader and budgeted.** [`IndexedReaderCacheOptions`]
//!   states a total byte and entry budget, split one quarter to source chunks,
//!   one half to resolved objects and the remainder to decoded object streams.
//!   The object cache shards (up to `SHARED_OBJECT_MAX_SHARDS`, never below
//!   `SHARED_OBJECT_MIN_ENTRIES_PER_SHARD` entries per shard) so admission
//!   does not serialize on one lock. Eviction is best effort by construction:
//!   entries a caller still holds are skipped, and a pass gives up after
//!   `MAX_PINNED_EVICTION_SCAN` non-evictable candidates rather than block.
//!   Cache pressure decides *retention*, never whether a value resolves — a
//!   reader opened without a cache budget resolves the same values, just
//!   without reuse.
//! - **Allocation is charged before it happens.** The preflight path reserves
//!   against a [`ScalarResolutionPermit`] before reading, so a declared length
//!   that would exceed the permit fails as
//!   [`ScalarResourceLimit`](IndexedReaderError::ScalarResourceLimit) instead
//!   of being allocated and then rejected.
//! - **Reported, not guessed.** [`IndexedReader::index_stats`] and
//!   [`IndexedReader::cache_stats`] report retained bytes, peaks and hit rates,
//!   which is how the bounds above are tested rather than asserted.
//!
//! # The refusal envelope
//!
//! Every bound fails closed with a typed variant of [`IndexedReaderError`] or
//! [`IndexedStreamReadError`] — never a truncated value, never a silent empty
//! result. The point of the typing is that a caller can tell a limit from
//! corruption without matching on message text, and can decide per case whether
//! to raise the limit, fall back to [`Document`](crate::Document), or give up.
//! The cases worth naming:
//!
//! - **Per-object decode cap.**
//!   [`ObjectLimitExceeded`](IndexedReaderError::ObjectLimitExceeded) carries
//!   an [`ObjectLimitProvenance`] distinguishing "the framer still wanted more
//!   input at the configured maximum" from "the source ran out" from "checked
//!   arithmetic could not represent the window". Bounded caches use that field;
//!   without it they would have to parse the display string.
//! - **Page-tree depth and count.** The page-tree walk is iterative and
//!   bounded by [`IndexedReaderOptions::page_tree_depth`] and
//!   [`IndexedReaderOptions::max_pages`], with a per-node dereference budget of
//!   `PAGE_TREE_DEREFERENCE_LIMIT`. A `/Kids` cycle terminates as
//!   [`PageTreeDepthLimitExceeded`](IndexedReaderError::PageTreeDepthLimitExceeded)
//!   rather than recursing.
//! - **Unsupported object-stream and encoded-stream shapes.** A compressed
//!   entry whose container is not a stream, or whose member index does not
//!   decode, is reported against both ids
//!   ([`ObjectStreamMember`](IndexedReaderError::ObjectStreamMember)). An
//!   encoded payload that is encrypted, or otherwise not exposable as
//!   plaintext, refuses at descriptor-open time with
//!   [`Protected`](IndexedStreamReadError::Protected) instead of handing back
//!   ciphertext; such objects remain available through ordinary resolution,
//!   which decrypts. A payload with no provable span refuses with
//!   [`LengthUnavailable`](IndexedStreamReadError::LengthUnavailable).
//! - **Encryption.** A document needing a password fails as
//!   [`PasswordRequired`](IndexedReaderError::PasswordRequired) at open; a
//!   wrong one as [`InvalidPassword`](IndexedReaderError::InvalidPassword).
//!   [`IndexedReaderOptions`]'s `Debug` redacts the password value.
//!
//! # Cross-reference recovery
//!
//! If — and only if — the cross-reference machinery is unusable
//! ([`InvalidStartXref`](IndexedReaderError::InvalidStartXref),
//! [`StartXrefOutOfBounds`](IndexedReaderError::StartXrefOutOfBounds),
//! [`InvalidXref`](IndexedReaderError::InvalidXref)) the index is rebuilt by
//! scanning the body for `N G obj` headers. A source error, a resource limit or
//! a bad header does not trigger it, because rescanning would not answer any of
//! those, and a healthy document never reaches it at all.
//!
//! The scan is a single forward pass over the same chunked read path the
//! resolver uses — 64 KiB at a time into a reused window, with a small
//! lookbehind/lookahead so a header straddling two reads is still matched
//! whole. Retention is the offsets map, which is O(live objects) and capped at
//! `MAX_RECOVERED_OBJECTS`. A later header
//! for an object number wins, which is how an incremental update supersedes the
//! revision it was appended to.
//!
//! Recovery refuses rather than guesses:
//!
//! - The catalog must be **provable**. If the recovered `/Root` is not itself
//!   an object the scan found as a normal header, the original
//!   cross-reference error is returned unchanged, so a caller's own fallback
//!   sees exactly what it saw before. Opening a document that resolves to
//!   nothing would be strictly worse than reporting the failure.
//! - A trailer is never **synthesized over encryption**. If the body mentions
//!   `/Encrypt` anywhere, synthesis is forbidden, because a synthesized trailer
//!   cannot carry the encryption dictionary and the result would silently be
//!   read as plaintext.
//! - The outcome is **provenance-marked**:
//!   [`IndexedReaderIndexStats::recovered`] is `true` only for an index built
//!   this way, so a caller can count recoveries and treat them differently.
//!
//! # Determinism
//!
//! For a fixed source and fixed [`IndexedReaderOptions`], results do not depend
//! on cache state, thread count or the order in which a caller asks:
//!
//! - Resolving an id yields the same value whether it was cached, resolved
//!   concurrently by another thread, or resolved by a reader with no cache.
//! - [`resolve_many_shared`](IndexedReader::resolve_many_shared) returns
//!   first-occurrence order of the requested ids regardless of internal
//!   completion order, and deduplicates.
//! - Where two structures disagree — duplicate object-stream members, a
//!   `/XRefStm` supplement against its classic section, a `/Prev` chain — the
//!   precedence rule is fixed and documented at the site, not left to
//!   whichever parse finished first.
//! - Only source I/O failures are treated as retryable: they are shared with
//!   callers already waiting on the cell and then discarded, so a later call
//!   can try again. Deterministic failures stay cached, because re-deriving
//!   them would cost the same work and give the same answer.
//!
//! Sources must hold up their end: a [`RandomAccessSource`] is required to be
//! thread-safe and to keep its length and bytes stable for the reader's
//! lifetime. The reader captures the length at open and reports
//! [`SourceLengthChanged`](IndexedStreamReadError::SourceLengthChanged) if it
//! later disagrees.
//!
//! # Eager behaviour is the oracle
//!
//! This module adds a second way to read a file that
//! [`Document`](crate::Document) already reads, so wherever the two can be
//! asked the same question, the eager answer defines the correct one. The tests
//! are written that way: fixtures are loaded both ways and compared —
//! version, `xref_start`, declared size, per-entry offsets and generations,
//! `/Root`, and resolved object values — including for the deliberately
//! malformed inputs where "correct" means "the same bytes eager produces", not
//! "what the spec would have preferred". Where the indexed reader intentionally
//! differs (retaining free entries; refusing instead of truncating) the
//! difference is pinned by its own test rather than left implicit.
//!
//! [`Object`]: crate::Object
//! [`RandomAccessSource`]: crate::source::RandomAccessSource

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[cfg(feature = "rayon")]
use rayon::prelude::*;
use thiserror::Error;

use crate::encryption::{self, EncryptionState, PasswordAlgorithm};
use crate::scalar_budget::{BoundedScalar, ScalarCharge, ScalarResolutionPermit};
use crate::source::{RandomAccessSource, SourceError};
use crate::source_cache::CachedSource;
use crate::{Dictionary, Object, ObjectStream, Stream};

pub use crate::source_cache::{IndexedReaderCacheOptions, IndexedReaderSourceCacheStats};

mod cache;
mod errors;
mod framing;
mod lex;
mod open;
mod options;
mod page_map;
mod preflight;
mod resolve;
mod stats;
mod streams;

#[cfg(test)]
mod tests;

use self::cache::*;
pub use self::errors::*;
use self::framing::*;
use self::lex::*;
use self::open::*;
pub use self::options::*;
pub use self::page_map::*;
use self::preflight::*;
use self::resolve::*;
pub use self::stats::*;
pub use self::streams::*;

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
/// Maximum consecutive non-evictable (loading, or still held by a caller) candidates one
/// eviction pass rotates past before giving up.
///
/// Eviction is best effort by construction: the pass already stops once it has rotated every
/// probation candidate, because a cache in which nothing is currently releasable must stay
/// briefly over its byte target rather than block a caller. Without a constant bound, though,
/// a caller that legitimately holds a wide fan-out of resolved objects — a structure-tree walk
/// holding one handle per sibling, say — pins a long prefix of the queue, and every insertion
/// re-locks each pinned cell in turn. That is quadratic in the retained set: one 144-page
/// corpus render spent 4.25M rotations across 6.5k passes and ran 1.64x the eager wall.
/// Giving up after a constant prefix only forgoes reclaiming an entry sitting behind it — the
/// next insertion tries again — and changes nothing a caller can observe, since cache pressure
/// decides retention, never whether a value resolves.
const MAX_PINNED_EVICTION_SCAN: usize = 16;
/// Entries a shard must be able to hold before the object cache is split across more of them.
///
/// Sharding trades a smaller per-shard budget for independent locks. Below this many entries a
/// shard's own budget starts to distort admission, so small caches (every test fixture, and any
/// document whose index is tiny) stay single-shard and behave exactly as before.
const SHARED_OBJECT_MIN_ENTRIES_PER_SHARD: usize = 128;
/// Upper bound on object-cache shards.
const SHARED_OBJECT_MAX_SHARDS: usize = 16;
// Conservative envelope for the map node, queue key, Arc allocation/header,
// mutex/condvar cell, and allocator slack of one retained ObjStm entry.
const OBJECT_STREAM_CACHE_ENTRY_BYTES: usize = 512;

// Test-only instrumentation counters shared by the framing and page-tree walks.
#[cfg(test)]
thread_local! {
    pub(super) static OBJECT_BODY_PARSE_CALLS: Cell<usize> = const { Cell::new(0) };
    pub(super) static PAGE_TREE_WALK_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Structural location of one live indexed indirect object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexedObjectLocation {
    /// Ordinary indirect object at the requested full generation.
    Normal,
    /// Generation-zero member declared by an xref stream.
    Compressed { container: crate::ObjectId, index: u32 },
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
    object_cache: Option<ShardedCache<Object>>,
    object_stream_cache: Option<SharedCache<PreparedObjectStream>>,
    cache_counters: Option<Arc<CacheCounters>>,
}
