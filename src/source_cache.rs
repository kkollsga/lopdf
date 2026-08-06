use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::source::{RandomAccessSource, SourceError, SourceResult};

pub(crate) const SOURCE_CHUNK_BYTES: u64 = 64 * 1024;

/// Total cache budget for an indexed reader.
///
/// The reader assigns one quarter of each limit to aligned source chunks, one
/// half to resolved objects, and the remainder to decoded object streams.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct IndexedReaderCacheOptions {
    max_bytes: u64,
    max_entries: usize,
}

impl IndexedReaderCacheOptions {
    /// Create a total cache budget with fixed byte and entry limits.
    pub const fn new(max_bytes: u64, max_entries: usize) -> Self {
        Self { max_bytes, max_entries }
    }

    /// Maximum bytes retained across all indexed-reader caches.
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Maximum entries retained across all indexed-reader caches.
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub(crate) const fn source_max_bytes(&self) -> u64 {
        self.max_bytes / 4
    }

    pub(crate) const fn source_max_entries(&self) -> usize {
        self.max_entries / 4
    }
}

impl Default for IndexedReaderCacheOptions {
    fn default() -> Self {
        Self::new(32 * 1024 * 1024, 16 * 1024)
    }
}

impl std::fmt::Debug for IndexedReaderCacheOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexedReaderCacheOptions")
            .field("max_bytes", &self.max_bytes)
            .field("max_entries", &self.max_entries)
            .finish()
    }
}

/// Read-only counters for the indexed reader's bounded caches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedReaderCacheStats {
    source: IndexedReaderSourceCacheStats,
}

impl IndexedReaderCacheStats {
    /// Counters for the aligned positional source-chunk cache.
    pub const fn source(&self) -> &IndexedReaderSourceCacheStats {
        &self.source
    }
}

/// Read-only counters for aligned positional source chunks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedReaderSourceCacheStats {
    hits: u64,
    misses: u64,
    loads: u64,
    duplicate_loads_avoided: u64,
    waits: u64,
    evictions: u64,
    bypass_reads: u64,
    logical_requested_bytes: u64,
    requested_bytes: u64,
    bypass_bytes: u64,
    retained_bytes: u64,
    in_flight_bytes: u64,
    peak_retained_bytes: u64,
    peak_in_flight_bytes: u64,
    retained_entries: usize,
    in_flight_entries: usize,
    peak_entries: usize,
}

macro_rules! stat_getters {
    ($($name:ident : $ty:ty),+ $(,)?) => {$(
        #[doc = concat!("Return the current ", stringify!($name), " counter.")]
        pub const fn $name(&self) -> $ty { self.$name }
    )+};
}

impl IndexedReaderSourceCacheStats {
    stat_getters! {
        hits: u64,
        misses: u64,
        loads: u64,
        duplicate_loads_avoided: u64,
        waits: u64,
        evictions: u64,
        bypass_reads: u64,
        logical_requested_bytes: u64,
        requested_bytes: u64,
        bypass_bytes: u64,
        retained_bytes: u64,
        in_flight_bytes: u64,
        peak_retained_bytes: u64,
        peak_in_flight_bytes: u64,
        retained_entries: usize,
        in_flight_entries: usize,
        peak_entries: usize,
    }
}

pub(crate) struct CachedSource {
    source: Arc<dyn RandomAccessSource>,
    source_len: u64,
    max_bytes: u64,
    max_entries: usize,
    cache: Mutex<CacheState>,
    stats: SourceCacheCounters,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<u64, CacheRecord>,
    ready_lru: VecDeque<(u64, u64)>,
    clock: u64,
    retained_bytes: u64,
    in_flight_bytes: u64,
    retained_entries: usize,
    in_flight_entries: usize,
}

struct CacheRecord {
    entry: Arc<CacheEntry>,
    ready_len: Option<u64>,
    last_used: u64,
}

struct CacheEntry {
    state: Mutex<EntryState>,
    ready: Condvar,
}

enum EntryState {
    Loading,
    Ready(Arc<Vec<u8>>),
    Failed(SharedSourceError),
}

#[derive(Clone)]
enum SharedSourceError {
    RangeOverflow {
        offset: u64,
        length: u64,
    },
    OutOfBounds {
        offset: u64,
        length: u64,
        source_len: u64,
    },
    ReadLimitExceeded {
        requested: u64,
        limit: u64,
    },
    PlatformLimitExceeded {
        requested: u64,
        limit: u64,
    },
    AllocationFailed {
        requested: u64,
    },
    UnexpectedEof {
        offset: u64,
        expected: u64,
        actual: u64,
    },
    InvalidReadCount {
        returned: usize,
        buffer_len: usize,
    },
    Io {
        kind: std::io::ErrorKind,
        message: Arc<str>,
    },
}

impl From<&SourceError> for SharedSourceError {
    fn from(error: &SourceError) -> Self {
        match error {
            SourceError::RangeOverflow { offset, length } => Self::RangeOverflow {
                offset: *offset,
                length: *length,
            },
            SourceError::OutOfBounds {
                offset,
                length,
                source_len,
            } => Self::OutOfBounds {
                offset: *offset,
                length: *length,
                source_len: *source_len,
            },
            SourceError::ReadLimitExceeded { requested, limit } => Self::ReadLimitExceeded {
                requested: *requested,
                limit: *limit,
            },
            SourceError::PlatformLimitExceeded { requested, limit } => Self::PlatformLimitExceeded {
                requested: *requested,
                limit: *limit,
            },
            SourceError::AllocationFailed { requested } => Self::AllocationFailed { requested: *requested },
            SourceError::UnexpectedEof {
                offset,
                expected,
                actual,
            } => Self::UnexpectedEof {
                offset: *offset,
                expected: *expected,
                actual: *actual,
            },
            SourceError::InvalidReadCount { returned, buffer_len } => Self::InvalidReadCount {
                returned: *returned,
                buffer_len: *buffer_len,
            },
            SourceError::Io(error) => Self::Io {
                kind: error.kind(),
                message: Arc::from(error.to_string()),
            },
        }
    }
}

impl From<SharedSourceError> for SourceError {
    fn from(error: SharedSourceError) -> Self {
        match error {
            SharedSourceError::RangeOverflow { offset, length } => Self::RangeOverflow { offset, length },
            SharedSourceError::OutOfBounds {
                offset,
                length,
                source_len,
            } => Self::OutOfBounds {
                offset,
                length,
                source_len,
            },
            SharedSourceError::ReadLimitExceeded { requested, limit } => Self::ReadLimitExceeded { requested, limit },
            SharedSourceError::PlatformLimitExceeded { requested, limit } => {
                Self::PlatformLimitExceeded { requested, limit }
            }
            SharedSourceError::AllocationFailed { requested } => Self::AllocationFailed { requested },
            SharedSourceError::UnexpectedEof {
                offset,
                expected,
                actual,
            } => Self::UnexpectedEof {
                offset,
                expected,
                actual,
            },
            SharedSourceError::InvalidReadCount { returned, buffer_len } => {
                Self::InvalidReadCount { returned, buffer_len }
            }
            SharedSourceError::Io { kind, message } => Self::Io(std::io::Error::new(kind, message.to_string())),
        }
    }
}

#[derive(Default)]
struct SourceCacheCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    loads: AtomicU64,
    duplicate_loads_avoided: AtomicU64,
    waits: AtomicU64,
    evictions: AtomicU64,
    bypass_reads: AtomicU64,
    logical_requested_bytes: AtomicU64,
    requested_bytes: AtomicU64,
    bypass_bytes: AtomicU64,
    peak_retained_bytes: AtomicU64,
    peak_in_flight_bytes: AtomicU64,
    peak_entries: AtomicUsize,
}

impl CachedSource {
    pub(crate) fn new(
        source: Arc<dyn RandomAccessSource>, cache_options: IndexedReaderCacheOptions,
    ) -> SourceResult<Self> {
        let source_len = source.len()?;
        Ok(Self {
            source,
            source_len,
            max_bytes: cache_options.source_max_bytes(),
            max_entries: cache_options.source_max_entries(),
            cache: Mutex::new(CacheState::default()),
            stats: SourceCacheCounters::default(),
        })
    }

    pub(crate) fn stats(&self) -> IndexedReaderCacheStats {
        let state = lock_unpoisoned(&self.cache);
        IndexedReaderCacheStats {
            source: IndexedReaderSourceCacheStats {
                hits: self.stats.hits.load(Ordering::Relaxed),
                misses: self.stats.misses.load(Ordering::Relaxed),
                loads: self.stats.loads.load(Ordering::Relaxed),
                duplicate_loads_avoided: self.stats.duplicate_loads_avoided.load(Ordering::Relaxed),
                waits: self.stats.waits.load(Ordering::Relaxed),
                evictions: self.stats.evictions.load(Ordering::Relaxed),
                bypass_reads: self.stats.bypass_reads.load(Ordering::Relaxed),
                logical_requested_bytes: self.stats.logical_requested_bytes.load(Ordering::Relaxed),
                requested_bytes: self.stats.requested_bytes.load(Ordering::Relaxed),
                bypass_bytes: self.stats.bypass_bytes.load(Ordering::Relaxed),
                retained_bytes: state.retained_bytes,
                in_flight_bytes: state.in_flight_bytes,
                peak_retained_bytes: self.stats.peak_retained_bytes.load(Ordering::Relaxed),
                peak_in_flight_bytes: self.stats.peak_in_flight_bytes.load(Ordering::Relaxed),
                retained_entries: state.retained_entries,
                in_flight_entries: state.in_flight_entries,
                peak_entries: self.stats.peak_entries.load(Ordering::Relaxed),
            },
        }
    }

    fn get_chunk(&self, chunk_offset: u64, chunk_len: u64) -> SourceResult<Arc<Vec<u8>>> {
        let entry = {
            let mut cache = lock_unpoisoned(&self.cache);
            if cache.entries.contains_key(&chunk_offset) {
                cache.clock = cache.clock.wrapping_add(1);
                let stamp = cache.clock;
                let (entry, ready) = {
                    let record = cache.entries.get_mut(&chunk_offset).expect("entry just found");
                    let ready = record.ready_len.is_some();
                    if ready {
                        record.last_used = stamp;
                    }
                    (Arc::clone(&record.entry), ready)
                };
                if ready {
                    self.stats.hits.fetch_add(1, Ordering::Relaxed);
                    cache.ready_lru.push_back((chunk_offset, stamp));
                    compact_lru(&mut cache, self.max_entries);
                } else {
                    self.stats.duplicate_loads_avoided.fetch_add(1, Ordering::Relaxed);
                    self.stats.waits.fetch_add(1, Ordering::Relaxed);
                }
                Some(entry)
            } else {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                self.evict_for(&mut cache, chunk_len);
                if self.max_entries == 0
                    || chunk_len > self.max_bytes
                    || cache.entries.len() >= self.max_entries
                    || cache
                        .retained_bytes
                        .saturating_add(cache.in_flight_bytes)
                        .saturating_add(chunk_len)
                        > self.max_bytes
                {
                    None
                } else {
                    let entry = Arc::new(CacheEntry {
                        state: Mutex::new(EntryState::Loading),
                        ready: Condvar::new(),
                    });
                    cache.entries.insert(
                        chunk_offset,
                        CacheRecord {
                            entry: Arc::clone(&entry),
                            ready_len: None,
                            last_used: 0,
                        },
                    );
                    cache.in_flight_bytes += chunk_len;
                    cache.in_flight_entries += 1;
                    self.stats.loads.fetch_add(1, Ordering::Relaxed);
                    self.record_peaks(&cache);
                    drop(cache);

                    let result = self.load_chunk(chunk_offset, chunk_len);
                    let mut cache = lock_unpoisoned(&self.cache);
                    cache.in_flight_bytes -= chunk_len;
                    cache.in_flight_entries -= 1;
                    match result {
                        Ok(bytes) => {
                            cache.clock = cache.clock.wrapping_add(1);
                            let stamp = cache.clock;
                            let record = cache.entries.get_mut(&chunk_offset).expect("loading entry retained");
                            record.ready_len = Some(chunk_len);
                            record.last_used = stamp;
                            cache.retained_bytes += chunk_len;
                            cache.retained_entries += 1;
                            cache.ready_lru.push_back((chunk_offset, stamp));
                            self.record_peaks(&cache);
                            drop(cache);
                            *lock_unpoisoned(&entry.state) = EntryState::Ready(Arc::clone(&bytes));
                            entry.ready.notify_all();
                            return Ok(bytes);
                        }
                        Err(error) => {
                            cache.entries.remove(&chunk_offset);
                            drop(cache);
                            let shared = SharedSourceError::from(&error);
                            *lock_unpoisoned(&entry.state) = EntryState::Failed(shared);
                            entry.ready.notify_all();
                            return Err(error);
                        }
                    }
                }
            }
        };

        let Some(entry) = entry else {
            return self.load_bypass_chunk(chunk_offset, chunk_len);
        };
        let mut state = lock_unpoisoned(&entry.state);
        loop {
            match &*state {
                EntryState::Loading => state = wait_unpoisoned(&entry.ready, state),
                EntryState::Ready(bytes) => return Ok(Arc::clone(bytes)),
                EntryState::Failed(error) => return Err(error.clone().into()),
            }
        }
    }

    fn load_chunk(&self, offset: u64, length: u64) -> SourceResult<Arc<Vec<u8>>> {
        let output_len = usize::try_from(length).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: length,
            limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(output_len)
            .map_err(|_| SourceError::AllocationFailed { requested: length })?;
        bytes.resize(output_len, 0);
        self.read_source_exact(offset, &mut bytes)?;
        Ok(Arc::new(bytes))
    }

    fn read_source_exact(&self, offset: u64, output: &mut [u8]) -> SourceResult<()> {
        let expected = output.len() as u64;
        let mut completed = 0_usize;
        while completed < output.len() {
            let remaining = output.len() - completed;
            self.stats
                .requested_bytes
                .fetch_add(remaining as u64, Ordering::Relaxed);
            let read_offset = offset.checked_add(completed as u64).ok_or(SourceError::RangeOverflow {
                offset,
                length: expected,
            })?;
            match self.source.read_at(read_offset, &mut output[completed..]) {
                Ok(0) => {
                    return Err(SourceError::UnexpectedEof {
                        offset,
                        expected,
                        actual: completed as u64,
                    });
                }
                Ok(read) if read > remaining => {
                    return Err(SourceError::InvalidReadCount {
                        returned: read,
                        buffer_len: remaining,
                    });
                }
                Ok(read) => completed += read,
                Err(SourceError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn load_bypass_chunk(&self, offset: u64, length: u64) -> SourceResult<Arc<Vec<u8>>> {
        self.stats.bypass_reads.fetch_add(1, Ordering::Relaxed);
        self.stats.bypass_bytes.fetch_add(length, Ordering::Relaxed);
        self.load_chunk(offset, length)
    }

    fn evict_for(&self, cache: &mut CacheState, incoming: u64) {
        while (cache.entries.len() >= self.max_entries
            || cache
                .retained_bytes
                .saturating_add(cache.in_flight_bytes)
                .saturating_add(incoming)
                > self.max_bytes)
            && !cache.ready_lru.is_empty()
        {
            let (offset, stamp) = cache.ready_lru.pop_front().expect("LRU checked non-empty");
            let Some(record) = cache.entries.get(&offset) else {
                continue;
            };
            if record.last_used != stamp {
                continue;
            }
            let Some(length) = record.ready_len else {
                continue;
            };
            cache.entries.remove(&offset);
            cache.retained_bytes = cache.retained_bytes.saturating_sub(length);
            cache.retained_entries = cache.retained_entries.saturating_sub(1);
            self.stats.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_peaks(&self, cache: &CacheState) {
        self.stats
            .peak_retained_bytes
            .fetch_max(cache.retained_bytes, Ordering::Relaxed);
        self.stats
            .peak_in_flight_bytes
            .fetch_max(cache.in_flight_bytes, Ordering::Relaxed);
        self.stats
            .peak_entries
            .fetch_max(cache.entries.len(), Ordering::Relaxed);
    }
}

impl RandomAccessSource for CachedSource {
    fn len(&self) -> SourceResult<u64> {
        self.source.len()
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
        let requested = u64::try_from(output.len()).map_err(|_| SourceError::RangeOverflow {
            offset,
            length: u64::MAX,
        })?;
        self.stats
            .logical_requested_bytes
            .fetch_add(requested, Ordering::Relaxed);
        if offset > self.source_len {
            return Err(SourceError::OutOfBounds {
                offset,
                length: 0,
                source_len: self.source_len,
            });
        }
        if output.is_empty() || offset == self.source_len {
            return Ok(0);
        }
        if requested > SOURCE_CHUNK_BYTES {
            self.stats.bypass_reads.fetch_add(1, Ordering::Relaxed);
            self.stats.bypass_bytes.fetch_add(requested, Ordering::Relaxed);
            self.stats.requested_bytes.fetch_add(requested, Ordering::Relaxed);
            return self.source.read_at(offset, output);
        }

        let available = self.source_len - offset;
        let wanted = requested.min(available);
        let mut copied = 0_u64;
        while copied < wanted {
            let position = offset.checked_add(copied).ok_or(SourceError::RangeOverflow {
                offset,
                length: requested,
            })?;
            let chunk_offset = (position / SOURCE_CHUNK_BYTES) * SOURCE_CHUNK_BYTES;
            let chunk_len = SOURCE_CHUNK_BYTES.min(self.source_len - chunk_offset);
            let chunk = self.get_chunk(chunk_offset, chunk_len)?;
            let within = position - chunk_offset;
            let take = (chunk_len - within).min(wanted - copied);
            let output_start = usize::try_from(copied).map_err(|_| SourceError::PlatformLimitExceeded {
                requested,
                limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            let output_end = usize::try_from(copied + take).map_err(|_| SourceError::PlatformLimitExceeded {
                requested,
                limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            let chunk_start = usize::try_from(within).map_err(|_| SourceError::PlatformLimitExceeded {
                requested: within,
                limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            let chunk_end = usize::try_from(within + take).map_err(|_| SourceError::PlatformLimitExceeded {
                requested: within + take,
                limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            })?;
            output[output_start..output_end].copy_from_slice(&chunk[chunk_start..chunk_end]);
            copied += take;
        }
        usize::try_from(copied).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: copied,
            limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
        })
    }
}

fn compact_lru(cache: &mut CacheState, max_entries: usize) {
    let limit = max_entries.saturating_mul(2).max(64);
    if cache.ready_lru.len() <= limit {
        return;
    }
    let mut current: Vec<_> = cache
        .entries
        .iter()
        .filter_map(|(offset, record)| record.ready_len.map(|_| (*offset, record.last_used)))
        .collect();
    current.sort_unstable_by_key(|(_, stamp)| *stamp);
    cache.ready_lru = current.into();
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_unpoisoned<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::BytesSource;

    fn bytes(chunks: usize) -> Arc<[u8]> {
        (0..chunks * SOURCE_CHUNK_BYTES as usize)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>()
            .into()
    }

    struct CountingSource {
        bytes: Arc<[u8]>,
        reads: AtomicUsize,
    }

    impl RandomAccessSource for CountingSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let start = offset as usize;
            let read = output.len().min(self.bytes.len().saturating_sub(start));
            output[..read].copy_from_slice(&self.bytes[start..start + read]);
            Ok(read)
        }
    }

    struct GatedSource {
        bytes: Arc<[u8]>,
        reads: AtomicUsize,
        gate: Barrier,
        gate_reads: usize,
    }

    impl RandomAccessSource for GatedSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            let call = self.reads.fetch_add(1, Ordering::SeqCst);
            if call < self.gate_reads {
                self.gate.wait();
            }
            let start = offset as usize;
            output.copy_from_slice(&self.bytes[start..start + output.len()]);
            Ok(output.len())
        }
    }

    #[test]
    fn repeated_reads_hit_one_aligned_chunk() {
        let source = Arc::new(CountingSource {
            bytes: bytes(2),
            reads: AtomicUsize::new(0),
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let cached = CachedSource::new(erased, IndexedReaderCacheOptions::new(4 * SOURCE_CHUNK_BYTES, 8)).unwrap();
        let expected = source.bytes[123..635].to_vec();
        for _ in 0..3 {
            assert_eq!(cached.read_range(123, 512, 512).unwrap(), expected);
        }
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        let stats = cached.stats();
        assert_eq!(stats.source().loads(), 1);
        assert_eq!(stats.source().hits(), 2);
        assert_eq!(stats.source().retained_bytes(), SOURCE_CHUNK_BYTES);
        assert!(stats.source().logical_requested_bytes() >= 3 * 512);
        assert_eq!(stats.source().requested_bytes(), SOURCE_CHUNK_BYTES);
    }

    #[test]
    fn same_chunk_is_single_flight_for_four_threads() {
        let source = Arc::new(GatedSource {
            bytes: bytes(2),
            reads: AtomicUsize::new(0),
            gate: Barrier::new(2),
            gate_reads: 1,
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let cached =
            Arc::new(CachedSource::new(erased, IndexedReaderCacheOptions::new(4 * SOURCE_CHUNK_BYTES, 8)).unwrap());
        let mut threads = Vec::new();
        for _ in 0..4 {
            let cached = Arc::clone(&cached);
            threads.push(std::thread::spawn(move || cached.read_range(100, 512, 512).unwrap()));
        }
        for _ in 0..10_000 {
            if cached.stats().source().waits() == 3 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(cached.stats().source().waits(), 3);
        source.gate.wait();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), source.bytes[100..612]);
        }
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        let stats = cached.stats();
        assert_eq!(stats.source().loads(), 1);
        assert_eq!(stats.source().duplicate_loads_avoided(), 3);
        assert_eq!(stats.source().waits(), 3);
    }

    #[test]
    fn four_independent_chunks_overlap_without_a_shared_io_lock() {
        let source = Arc::new(GatedSource {
            bytes: bytes(4),
            reads: AtomicUsize::new(0),
            gate: Barrier::new(4),
            gate_reads: 4,
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let cached =
            Arc::new(CachedSource::new(erased, IndexedReaderCacheOptions::new(16 * SOURCE_CHUNK_BYTES, 16)).unwrap());
        let threads: Vec<_> = (0..4_u64)
            .map(|chunk| {
                let cached = Arc::clone(&cached);
                std::thread::spawn(move || cached.read_range(chunk * SOURCE_CHUNK_BYTES + 17, 64, 64).unwrap())
            })
            .collect();
        for (chunk, thread) in threads.into_iter().enumerate() {
            let start = chunk * SOURCE_CHUNK_BYTES as usize + 17;
            assert_eq!(thread.join().unwrap(), source.bytes[start..start + 64]);
        }
        assert_eq!(source.reads.load(Ordering::SeqCst), 4);
        assert_eq!(cached.stats().source().peak_in_flight_bytes(), 4 * SOURCE_CHUNK_BYTES);
    }

    #[test]
    fn byte_and_entry_caps_evict_while_returned_arc_remains_valid() {
        let erased: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(bytes(3)));
        let cached = CachedSource::new(erased, IndexedReaderCacheOptions::new(8 * SOURCE_CHUNK_BYTES, 8)).unwrap();
        let pinned = cached.get_chunk(0, SOURCE_CHUNK_BYTES).unwrap();
        cached.read_range(SOURCE_CHUNK_BYTES + 9, 32, 32).unwrap();
        cached.read_range(2 * SOURCE_CHUNK_BYTES + 9, 32, 32).unwrap();
        assert_eq!(&pinned[9..41], &bytes(1)[9..41]);
        let stats = cached.stats();
        assert_eq!(stats.source().retained_entries(), 2);
        assert_eq!(stats.source().retained_bytes(), 2 * SOURCE_CHUNK_BYTES);
        assert_eq!(stats.source().evictions(), 1);
        assert!(stats.source().peak_retained_bytes() <= 2 * SOURCE_CHUNK_BYTES);
        assert!(stats.source().peak_entries() <= 2);
    }

    #[test]
    fn zero_cache_and_oversize_requests_bypass_without_retention() {
        let data = bytes(3);
        let erased: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(Arc::clone(&data)));
        let cached = CachedSource::new(erased, IndexedReaderCacheOptions::new(0, 0)).unwrap();
        assert_eq!(cached.read_range(7, 64, 64).unwrap(), data[7..71]);
        let mut output = vec![0; SOURCE_CHUNK_BYTES as usize + 1];
        cached.read_exact_at(0, &mut output).unwrap();
        assert_eq!(output, data[..output.len()]);
        let stats = cached.stats();
        assert_eq!(stats.source().retained_bytes(), 0);
        assert_eq!(stats.source().retained_entries(), 0);
        assert!(stats.source().bypass_reads() >= 2);
        assert!(stats.source().bypass_bytes() > SOURCE_CHUNK_BYTES);
    }

    struct FailOnceSource {
        bytes: Arc<[u8]>,
        failed: AtomicBool,
        reads: AtomicUsize,
    }

    impl RandomAccessSource for FailOnceSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if !self.failed.swap(true, Ordering::SeqCst) {
                return Err(SourceError::Io(std::io::Error::other("changed")));
            }
            let start = offset as usize;
            output.copy_from_slice(&self.bytes[start..start + output.len()]);
            Ok(output.len())
        }
    }

    #[test]
    fn failures_are_neither_retried_internally_nor_cached() {
        let source = Arc::new(FailOnceSource {
            bytes: bytes(1),
            failed: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let cached = CachedSource::new(erased, IndexedReaderCacheOptions::new(4 * SOURCE_CHUNK_BYTES, 8)).unwrap();
        assert!(matches!(cached.read_range(0, 32, 32), Err(SourceError::Io(_))));
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        assert_eq!(cached.read_range(0, 32, 32).unwrap(), source.bytes[..32]);
        assert_eq!(source.reads.load(Ordering::SeqCst), 2);
        assert_eq!(cached.stats().source().loads(), 2);
    }

    struct OverreportingSource;

    impl RandomAccessSource for OverreportingSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(SOURCE_CHUNK_BYTES)
        }

        fn read_at(&self, _offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            Ok(output.len() + 1)
        }
    }

    #[test]
    fn hostile_overreport_is_rejected_for_cached_and_zero_cache_paths() {
        for options in [
            IndexedReaderCacheOptions::new(4 * SOURCE_CHUNK_BYTES, 8),
            IndexedReaderCacheOptions::new(0, 0),
        ] {
            let cached = CachedSource::new(Arc::new(OverreportingSource), options).unwrap();
            assert!(matches!(
                cached.read_range(0, 32, 32),
                Err(SourceError::InvalidReadCount { .. })
            ));
        }
    }

    struct SourceChanged {
        bytes: Arc<[u8]>,
        changed: AtomicBool,
    }

    impl RandomAccessSource for SourceChanged {
        fn len(&self) -> SourceResult<u64> {
            if self.changed.load(Ordering::SeqCst) {
                Err(SourceError::Io(std::io::Error::other("source changed")))
            } else {
                Ok(self.bytes.len() as u64)
            }
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            let start = offset as usize;
            output.copy_from_slice(&self.bytes[start..start + output.len()]);
            Ok(output.len())
        }
    }

    #[test]
    fn source_change_check_precedes_a_cached_hit() {
        let source = Arc::new(SourceChanged {
            bytes: bytes(1),
            changed: AtomicBool::new(false),
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let cached = CachedSource::new(erased, IndexedReaderCacheOptions::new(4 * SOURCE_CHUNK_BYTES, 8)).unwrap();
        cached.read_range(0, 32, 32).unwrap();
        source.changed.store(true, Ordering::SeqCst);
        let error = cached.read_range(0, 32, 32).unwrap_err();
        let SourceError::Io(error) = error else {
            panic!("expected source change I/O error");
        };
        assert_eq!(error.to_string(), "source changed");
        assert_eq!(cached.stats().source().hits(), 0);
    }
}
