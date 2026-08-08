//! Single-flight, segmented, sharded caches for resolved objects and object streams.

use super::*;

type SharedCellResult<T> = std::result::Result<Arc<T>, Arc<IndexedReaderError>>;

enum SharedCellState<T> {
    Loading,
    Ready(SharedCellResult<T>),
}

pub(super) struct SharedCell<T> {
    state: Mutex<SharedCellState<T>>,
    ready: Condvar,
}

impl<T> SharedCell<T> {
    pub(super) fn loading() -> Self {
        Self {
            state: Mutex::new(SharedCellState::Loading),
            ready: Condvar::new(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CacheSegment {
    Probation,
    Protected,
}

pub(super) struct CacheEntry<T> {
    pub(super) cell: Arc<SharedCell<T>>,
    pub(super) segment: CacheSegment,
    pub(super) bytes: usize,
}

pub(super) struct SharedCacheInner<T> {
    pub(super) entries: HashMap<crate::ObjectId, CacheEntry<T>>,
    pub(super) probation: VecDeque<crate::ObjectId>,
    protected: VecDeque<crate::ObjectId>,
    probation_bytes: usize,
    protected_bytes: usize,
    loading_entries: usize,
    /// Residency this shard last published into the cross-shard aggregate.
    ///
    /// Held under the shard's own lock, so the read-modify-write that turns a
    /// new residency into a signed delta is serialised per shard even though
    /// the aggregate itself is only `Relaxed`.
    reported_entries: usize,
    reported_bytes: usize,
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
            reported_entries: 0,
            reported_bytes: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CacheKind {
    Object,
    ObjectStream,
}

pub(super) struct SharedCache<T> {
    pub(super) inner: Mutex<SharedCacheInner<T>>,
    max_bytes: usize,
    max_entries: usize,
    max_entry_bytes: usize,
    protected_percent: usize,
    kind: CacheKind,
    counters: Arc<CacheCounters>,
    #[cfg(test)]
    pub(super) after_publish_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
pub(super) struct CacheCounters {
    pub(super) object_hits: AtomicU64,
    pub(super) object_misses: AtomicU64,
    pub(super) object_waits: AtomicU64,
    pub(super) object_loads: AtomicU64,
    pub(super) object_promotions: AtomicU64,
    pub(super) object_evictions: AtomicU64,
    pub(super) object_bypasses: AtomicU64,
    pub(super) negative_hits: AtomicU64,
    pub(super) object_transient_failures: AtomicU64,
    /// Live entries/bytes summed across every object-cache shard.
    ///
    /// Each shard publishes a signed delta against its own last report while it
    /// holds its lock, so this is the aggregate residency and the peaks below
    /// are `fetch_max`ed against *it* rather than against one shard's slice.
    object_live_entries: AtomicUsize,
    object_live_bytes: AtomicUsize,
    pub(super) object_peak_entries: AtomicUsize,
    pub(super) object_peak_bytes: AtomicUsize,
    pub(super) objstm_hits: AtomicU64,
    pub(super) objstm_misses: AtomicU64,
    pub(super) objstm_waits: AtomicU64,
    pub(super) objstm_loads: AtomicU64,
    pub(super) objstm_evictions: AtomicU64,
    pub(super) objstm_bypasses: AtomicU64,
    pub(super) objstm_transient_failures: AtomicU64,
    objstm_live_entries: AtomicUsize,
    objstm_live_bytes: AtomicUsize,
    pub(super) objstm_peak_entries: AtomicUsize,
    pub(super) objstm_peak_bytes: AtomicUsize,
}

/// The object cache, split into independently locked shards keyed by object number.
///
/// One `Mutex` in front of the whole resolved-object cache serialises every resolution in the
/// process: at full rayon width a page-dense document spends more time waiting for that lock
/// than resolving. Object numbers are dense and sequential, so `number % shards` spreads a
/// document's working set evenly, and each shard carries its own slice of the configured
/// budget — the totals a caller asked for are unchanged, only the lock they contend on.
pub(super) struct ShardedCache<T> {
    pub(super) shards: Box<[SharedCache<T>]>,
    mask: usize,
}

impl<T> ShardedCache<T> {
    pub(super) fn new(
        max_bytes: usize, max_entries: usize, max_entry_bytes: usize, protected_percent: usize, kind: CacheKind,
        counters: Arc<CacheCounters>,
    ) -> Self {
        let shards = (max_entries / SHARED_OBJECT_MIN_ENTRIES_PER_SHARD)
            .clamp(1, SHARED_OBJECT_MAX_SHARDS)
            .next_power_of_two()
            .min(SHARED_OBJECT_MAX_SHARDS);
        let shard_bytes = max_bytes / shards;
        let shard_entries = max_entries / shards;
        let shard_entry_bytes = max_entry_bytes / shards;
        let shards: Vec<_> = (0..shards)
            .map(|_| {
                SharedCache::new(
                    shard_bytes,
                    shard_entries,
                    shard_entry_bytes,
                    protected_percent,
                    kind,
                    Arc::clone(&counters),
                )
            })
            .collect();
        let mask = shards.len() - 1;
        Self {
            shards: shards.into_boxed_slice(),
            mask,
        }
    }

    fn shard(&self, id: crate::ObjectId) -> &SharedCache<T> {
        &self.shards[id.0 as usize & self.mask]
    }

    pub(super) fn resolve<F, W>(&self, id: crate::ObjectId, load: F, weight: W) -> SharedCellResult<T>
    where
        F: FnOnce() -> SharedCellResult<T>,
        W: FnOnce(&T) -> usize,
    {
        self.shard(id).resolve(id, load, weight)
    }

    pub(super) fn residency(&self) -> (usize, usize, usize, usize) {
        self.shards
            .iter()
            .map(SharedCache::residency)
            .fold((0, 0, 0, 0), |total, shard| {
                (
                    total.0.saturating_add(shard.0),
                    total.1.saturating_add(shard.1),
                    total.2.saturating_add(shard.2),
                    total.3.saturating_add(shard.3),
                )
            })
    }
}

impl<T> SharedCache<T> {
    pub(super) fn new(
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

    pub(super) fn resolve<F, W>(&self, id: crate::ObjectId, load: F, weight: W) -> SharedCellResult<T>
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
                self.publish_residency(&mut inner);
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
        let mut bypass = retained_bytes > self.max_entry_bytes || retained_bytes > self.max_bytes;
        let mut inner = self.inner.lock().unwrap();
        inner.loading_entries = inner.loading_entries.saturating_sub(1);
        while !transient
            && !bypass
            && inner
                .probation_bytes
                .saturating_add(inner.protected_bytes)
                .saturating_add(retained_bytes)
                > self.max_bytes
        {
            if !self.evict_one_ready(&mut inner) {
                bypass = true;
            }
        }
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
            self.publish_residency(&mut inner);
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

    pub(super) fn enforce_caps(&self, inner: &mut SharedCacheInner<T>) {
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
            if !Self::entry_is_evictable(entry) {
                match entry.segment {
                    CacheSegment::Probation => inner.probation.push_back(id),
                    CacheSegment::Protected => inner.protected.push_back(id),
                }
                pinned += 1;
                if pinned >= inner.probation.len().clamp(1, MAX_PINNED_EVICTION_SCAN) {
                    break;
                }
                continue;
            }
            self.remove_entry(inner, id, true);
            pinned = 0;
        }
    }

    fn evict_one_ready(&self, inner: &mut SharedCacheInner<T>) -> bool {
        let candidates = inner
            .probation
            .len()
            .saturating_add(inner.protected.len())
            .min(MAX_PINNED_EVICTION_SCAN);
        for _ in 0..candidates {
            let Some(id) = inner.probation.pop_front().or_else(|| inner.protected.pop_front()) else {
                return false;
            };
            let Some(entry) = inner.entries.get(&id) else {
                continue;
            };
            if !Self::entry_is_evictable(entry) {
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

    fn entry_is_evictable(entry: &CacheEntry<T>) -> bool {
        match &*entry.cell.state.lock().unwrap() {
            SharedCellState::Loading => false,
            SharedCellState::Ready(Ok(value)) => Arc::strong_count(value) == 1,
            SharedCellState::Ready(Err(_)) => true,
        }
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
        // Every removal path — eviction, cap enforcement, a bypassed or failed
        // publication — funnels through here, so republishing the shrunken
        // residency here is what keeps the aggregate from drifting upward
        // without a call at each of those sites.
        self.publish_residency(inner);
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

    pub(super) fn residency(&self) -> (usize, usize, usize, usize) {
        let inner = self.inner.lock().unwrap();
        (
            inner.probation.len(),
            inner.probation_bytes,
            inner.protected.len(),
            inner.protected_bytes,
        )
    }

    /// Publish this shard's residency into the cross-shard aggregate and raise
    /// the peaks against the aggregate.
    ///
    /// The cache is sharded, so a peak recorded from one shard's own residency
    /// is the maximum *over* shards, not the maximum of the sum — it can read
    /// below the concurrently reported `current_bytes()`, which is impossible
    /// for a peak by definition. Each shard therefore keeps its last reported
    /// residency in `inner` (so the compare is serialised by the shard lock it
    /// already holds) and contributes only the signed delta to the shared live
    /// counter; the delta's own `fetch_add` returns the pre-image, so the
    /// post-image it implies is what the peak is raised to. Relaxed ordering is
    /// enough: these are statistics, and every mutation of the aggregate is
    /// already serialised per shard.
    fn publish_residency(&self, inner: &mut SharedCacheInner<T>) {
        let entries = inner.entries.len();
        let bytes = inner.probation_bytes.saturating_add(inner.protected_bytes);
        let (live_entries, live_bytes, peak_entries, peak_bytes) = match self.kind {
            CacheKind::Object => (
                &self.counters.object_live_entries,
                &self.counters.object_live_bytes,
                &self.counters.object_peak_entries,
                &self.counters.object_peak_bytes,
            ),
            CacheKind::ObjectStream => (
                &self.counters.objstm_live_entries,
                &self.counters.objstm_live_bytes,
                &self.counters.objstm_peak_entries,
                &self.counters.objstm_peak_bytes,
            ),
        };
        let entry_delta = entries.wrapping_sub(inner.reported_entries);
        if entry_delta != 0 {
            inner.reported_entries = entries;
            let total = live_entries
                .fetch_add(entry_delta, Ordering::Relaxed)
                .wrapping_add(entry_delta);
            peak_entries.fetch_max(total, Ordering::Relaxed);
        }
        let byte_delta = bytes.wrapping_sub(inner.reported_bytes);
        if byte_delta != 0 {
            inner.reported_bytes = bytes;
            let total = live_bytes
                .fetch_add(byte_delta, Ordering::Relaxed)
                .wrapping_add(byte_delta);
            peak_bytes.fetch_max(total, Ordering::Relaxed);
        }
    }
}

impl SharedCache<PreparedObjectStream> {
    pub(super) fn resolve_bounded<F>(
        &self, container_id: crate::ObjectId, member_id: crate::ObjectId, member_index: u32,
        permit: &ScalarResolutionPermit, load: F,
    ) -> IndexedReaderResult<BoundedPreparedObjectStream>
    where
        F: FnOnce() -> IndexedReaderResult<(PreparedObjectStream, Vec<ScalarCharge>)>,
    {
        let mut load = Some(load);
        loop {
            let (cell, leader) = {
                let mut inner = self.inner.lock().unwrap();
                if let Some(entry) = inner.entries.get(&container_id) {
                    let cell = Arc::clone(&entry.cell);
                    self.record_hit();
                    self.touch(&mut inner, container_id);
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
                        let loaded = load.take().expect("bounded loader runs once")()?;
                        if permit.stats().cancelled {
                            drop(loaded);
                            return Err(IndexedReaderError::ScalarResolutionCancelled {
                                id: container_id,
                                phase: "object-stream-bypass-publish",
                            });
                        }
                        return Ok(BoundedPreparedObjectStream::CallLocal {
                            prepared: Arc::new(loaded.0),
                            _charges: loaded.1,
                        });
                    }
                    let cell = Arc::new(SharedCell::loading());
                    inner.probation.push_back(container_id);
                    inner.entries.insert(
                        container_id,
                        CacheEntry {
                            cell: Arc::clone(&cell),
                            segment: CacheSegment::Probation,
                            bytes: 0,
                        },
                    );
                    inner.loading_entries = inner.loading_entries.saturating_add(1);
                    self.publish_residency(&mut inner);
                    (cell, true)
                }
            };

            if !leader {
                let mut state = cell.state.lock().unwrap();
                if matches!(*state, SharedCellState::Loading) {
                    self.record_wait();
                }
                while matches!(*state, SharedCellState::Loading) {
                    if permit.stats().cancelled {
                        return Err(IndexedReaderError::ScalarResolutionCancelled {
                            id: container_id,
                            phase: "object-stream-cache-wait",
                        });
                    }
                    state = cell.ready.wait_timeout(state, Duration::from_millis(10)).unwrap().0;
                }
                let SharedCellState::Ready(result) = &*state else {
                    unreachable!();
                };
                match result {
                    Ok(prepared) => return Ok(BoundedPreparedObjectStream::Cached(Arc::clone(prepared))),
                    Err(error) if !permit.stats().cancelled => {
                        if let Some(error) = rewrap_cacheable_bounded_error(error, member_id, member_index) {
                            atomic_saturating_increment(&self.counters.negative_hits);
                            return Err(error);
                        }
                        drop(state);
                        let mut inner = self.inner.lock().unwrap();
                        self.remove_if_same(&mut inner, container_id, &cell, false);
                        continue;
                    }
                    Err(_) => {
                        return Err(IndexedReaderError::ScalarResolutionCancelled {
                            id: container_id,
                            phase: "object-stream-cache-wait",
                        });
                    }
                }
            }

            self.record_load();
            let loaded = load.take().expect("bounded loader runs once")();
            let cancelled = permit.stats().cancelled;
            let mut inner = self.inner.lock().unwrap();
            inner.loading_entries = inner.loading_entries.saturating_sub(1);

            match loaded {
                Err(error) => {
                    if is_transient_error(&error) {
                        atomic_saturating_increment(&self.counters.objstm_transient_failures);
                    }
                    if let Some(shared) = neutralize_cacheable_bounded_error(&error) {
                        let mut state = cell.state.lock().unwrap();
                        *state = SharedCellState::Ready(Err(Arc::new(shared)));
                        cell.ready.notify_all();
                    } else {
                        self.remove_if_same(&mut inner, container_id, &cell, false);
                        let mut state = cell.state.lock().unwrap();
                        *state = SharedCellState::Ready(Err(Arc::new(IndexedReaderError::ObjectStreamCacheBypass {
                            container: container_id,
                        })));
                        cell.ready.notify_all();
                    }
                    return Err(error);
                }
                Ok((prepared, charges)) if cancelled => {
                    drop(charges);
                    drop(prepared);
                    let error = IndexedReaderError::ScalarResolutionCancelled {
                        id: container_id,
                        phase: "object-stream-cache-publish",
                    };
                    self.remove_if_same(&mut inner, container_id, &cell, false);
                    let mut state = cell.state.lock().unwrap();
                    *state = SharedCellState::Ready(Err(Arc::new(IndexedReaderError::ObjectStreamCacheBypass {
                        container: container_id,
                    })));
                    cell.ready.notify_all();
                    return Err(error);
                }
                Ok((prepared, charges)) => {
                    let prepared = Arc::new(prepared);
                    let retained_bytes = prepared.cache_weight();
                    let mut cacheable = retained_bytes <= self.max_entry_bytes && retained_bytes <= self.max_bytes;
                    while cacheable
                        && inner
                            .probation_bytes
                            .saturating_add(inner.protected_bytes)
                            .saturating_add(retained_bytes)
                            > self.max_bytes
                    {
                        if !self.evict_one_ready(&mut inner) {
                            cacheable = false;
                        }
                    }

                    if cacheable {
                        if let Some(entry) = inner.entries.get_mut(&container_id)
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
                        self.publish_residency(&mut inner);
                        let mut state = cell.state.lock().unwrap();
                        *state = SharedCellState::Ready(Ok(Arc::clone(&prepared)));
                        cell.ready.notify_all();
                        drop(state);
                        drop(inner);
                        // The reader cache's pre-reserved B budget owns this
                        // allocation after publication; release call-local O.
                        drop(charges);
                        return Ok(BoundedPreparedObjectStream::Cached(prepared));
                    }

                    self.record_bypass();
                    self.remove_if_same(&mut inner, container_id, &cell, false);
                    let bypass_signal = Arc::new(IndexedReaderError::ObjectStreamCacheBypass {
                        container: container_id,
                    });
                    let mut state = cell.state.lock().unwrap();
                    *state = SharedCellState::Ready(Err(bypass_signal));
                    cell.ready.notify_all();
                    drop(state);
                    drop(inner);
                    return Ok(BoundedPreparedObjectStream::CallLocal {
                        prepared,
                        _charges: charges,
                    });
                }
            }
        }
    }
}

pub(super) fn atomic_saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

pub(super) fn usize_from_u64_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

pub(super) fn u64_from_usize_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn remove_key(queue: &mut VecDeque<crate::ObjectId>, id: crate::ObjectId) {
    if let Some(position) = queue.iter().position(|candidate| *candidate == id) {
        queue.remove(position);
    }
}

pub(super) fn object_retained_bytes(root: &Object) -> usize {
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

pub(super) fn dictionary_retained_bytes(dictionary: &Dictionary) -> usize {
    std::mem::size_of::<Dictionary>().saturating_add(scalar_dictionary_heap_bytes(dictionary))
}

fn encryption_state_retained_bytes(state: &EncryptionState) -> usize {
    let vectors = state
        .file_encryption_key
        .capacity()
        .saturating_add(state.stream_filter.capacity())
        .saturating_add(state.string_filter.capacity())
        .saturating_add(state.owner_value.capacity())
        .saturating_add(state.owner_encrypted.capacity())
        .saturating_add(state.user_value.capacity())
        .saturating_add(state.user_encrypted.capacity())
        .saturating_add(state.permission_encrypted.capacity());
    let filters = state.crypt_filters.iter().fold(0usize, |bytes, (name, _)| {
        bytes
            .saturating_add(name.capacity())
            // Conservative BTree node/Arc/filter allocation envelope.
            .saturating_add(256)
    });
    std::mem::size_of::<EncryptionState>()
        .saturating_add(vectors)
        .saturating_add(filters)
}

pub(super) fn index_retained_bytes(reader: &IndexedReader) -> usize {
    let location_bytes = reader.index.locations.len().saturating_mul(
        std::mem::size_of::<u32>()
            .saturating_add(std::mem::size_of::<ObjectLocation64>())
            // Conservative BTree node/key/value allocator envelope.
            .saturating_add(256),
    );
    std::mem::size_of::<IndexedReader>()
        .saturating_add(std::mem::size_of::<PdfIndex>())
        .saturating_add(2 * std::mem::size_of::<usize>()) // Arc allocations/headers.
        .saturating_add(reader.index.version.capacity())
        .saturating_add(location_bytes)
        .saturating_add(dictionary_retained_bytes(&reader.index.trailer))
        .saturating_add(
            reader
                .index
                .encryption_state
                .as_ref()
                .map_or(0, encryption_state_retained_bytes),
        )
}

pub(super) fn scalar_object_retained_bytes(object: &Object) -> usize {
    std::mem::size_of::<Object>().saturating_add(scalar_object_heap_bytes(object))
}

fn scalar_object_heap_bytes(object: &Object) -> usize {
    match object {
        Object::Name(value) | Object::String(value, _) => value.capacity(),
        Object::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<Object>())
            .saturating_add(
                values
                    .iter()
                    .map(scalar_object_heap_bytes)
                    .fold(0, usize::saturating_add),
            ),
        Object::Dictionary(dictionary) => scalar_dictionary_heap_bytes(dictionary),
        Object::Stream(stream) => stream
            .content
            .capacity()
            .saturating_add(scalar_dictionary_heap_bytes(&stream.dict)),
        Object::Null | Object::Boolean(_) | Object::Integer(_) | Object::Real(_) | Object::Reference(_) => 0,
    }
}

fn scalar_dictionary_heap_bytes(dictionary: &Dictionary) -> usize {
    dictionary.iter().fold(0, |bytes, (key, value)| {
        bytes
            .saturating_add(key.capacity())
            .saturating_add(std::mem::size_of::<(Vec<u8>, Object)>())
            // IndexMap's index/hash/control allocation is not visible through
            // the public iterator; use the same conservative node envelope as
            // the allocation-free preflight.
            .saturating_add(128)
            .saturating_add(scalar_object_heap_bytes(value))
    })
}

impl IndexedReader {
    pub(crate) fn configure_resolution_caches(
        &mut self, object_bytes: usize, object_entries: usize, object_stream_bytes: usize, object_stream_entries: usize,
    ) {
        let counters = Arc::new(CacheCounters::default());
        self.cache_counters = Some(Arc::clone(&counters));
        self.object_cache = (object_bytes > 0 && object_entries > 0).then(|| {
            ShardedCache::new(
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
}
