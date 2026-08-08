//! Cache and index statistics reported by the indexed reader.

use super::*;

/// A point-in-time snapshot of all bounded indexed-reader caches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedReaderCacheStats {
    source: IndexedReaderSourceCacheStats,
    pub(super) object: IndexedObjectCacheStats,
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

/// Conservative structural residency and cardinality for an opened index.
///
/// The byte estimate includes the reader/index bookkeeping, live xref map,
/// trailer, encryption metadata, and one owned page map. Source and resolver
/// caches are reported separately by [`IndexedReader::cache_stats`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedReaderIndexStats {
    object_count: usize,
    page_count: usize,
    index_retained_bytes: u64,
    page_map_retained_bytes: u64,
    estimated_retained_bytes: u64,
    recovered: bool,
}

impl IndexedReaderIndexStats {
    /// Number of live normal or compressed xref entries.
    pub const fn object_count(&self) -> usize {
        self.object_count
    }

    /// Number of actual leaf pages found by the bounded page-tree walk.
    pub const fn page_count(&self) -> usize {
        self.page_count
    }

    /// Conservative bytes retained by reader and index structures, excluding caches.
    pub const fn index_retained_bytes(&self) -> u64 {
        self.index_retained_bytes
    }

    /// Conservative bytes needed to retain the derived owned page map.
    pub const fn page_map_retained_bytes(&self) -> u64 {
        self.page_map_retained_bytes
    }

    /// Sum of index and page-map retained-byte estimates.
    pub const fn estimated_retained_bytes(&self) -> u64 {
        self.estimated_retained_bytes
    }

    /// Whether the index was rebuilt by scanning the body for indirect-object
    /// headers because the cross-reference sections were unusable.
    ///
    /// `false` for every healthy document: recovery runs only after a
    /// `startxref`/xref failure, never speculatively.
    pub const fn recovered(&self) -> bool {
        self.recovered
    }
}

impl IndexedReader {
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
}

impl IndexedReader {
    /// Compute conservative index/page-map residency and live cardinalities.
    pub fn index_stats(&self) -> IndexedReaderResult<IndexedReaderIndexStats> {
        let page_map = self.page_map()?;
        Ok(self.index_stats_for_page_map(&page_map))
    }

    pub(super) fn index_stats_for_page_map(&self, page_map: &PageMap) -> IndexedReaderIndexStats {
        let index_retained_bytes = u64_from_usize_saturating(index_retained_bytes(self));
        let page_map_retained_bytes = u64_from_usize_saturating(
            std::mem::size_of::<PageMap>().saturating_add(
                page_map
                    .pages
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PageMapEntry>()),
            ),
        );
        IndexedReaderIndexStats {
            object_count: self
                .index
                .locations
                .values()
                .filter(|location| !matches!(location, ObjectLocation64::Free { .. }))
                .count(),
            page_count: page_map.len(),
            index_retained_bytes,
            page_map_retained_bytes,
            estimated_retained_bytes: index_retained_bytes.saturating_add(page_map_retained_bytes),
            recovered: self.index.recovered,
        }
    }
}

impl IndexedReader {
    /// Snapshot counters and residency for the opt-in shared object cache.
    pub fn object_cache_stats(&self) -> IndexedObjectCacheStats {
        let Some(counters) = self.cache_counters.as_ref() else {
            return IndexedObjectCacheStats::default();
        };
        let (probation_entries, probation_bytes, protected_entries, protected_bytes) =
            self.object_cache.as_ref().map_or((0, 0, 0, 0), ShardedCache::residency);
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
}
