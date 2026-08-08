//! Bounded structural bootstrap for the staged indexed reader.

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
