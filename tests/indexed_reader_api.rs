use lopdf::{
    BytesSource, Dictionary, Document, IndexedReader, IndexedReaderCacheOptions, IndexedReaderError,
    IndexedReaderOptions, Object, RandomAccessSource, SourceError, Stream, dictionary,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

fn generated_pdf() -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![
                Object::Reference((3, 0)),
                Object::Reference((4, 0)),
                Object::Reference((5, 0)),
                Object::Reference((6, 0)),
            ],
            "Count" => 4,
            "Resources" => Object::Reference((7, 0)),
        }),
    );
    for id in 3..=6 {
        document.objects.insert(
            (id, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference((2, 0)),
                "Value" => i64::from(id),
            }),
        );
    }
    document.objects.insert(
        (7, 0),
        Object::Dictionary(dictionary! { "SharedDependency" => "stable" }),
    );
    document.max_id = 7;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn stream_pdf(streams: usize, stream_bytes: usize) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    for id in 1..=streams {
        let id = u32::try_from(id).unwrap();
        document.objects.insert(
            (id, 0),
            Object::Stream(Stream::new(
                Dictionary::new(),
                vec![u8::try_from(id % 251).unwrap(); stream_bytes],
            )),
        );
    }
    let root = u32::try_from(streams + 1).unwrap();
    document
        .objects
        .insert((root, 0), Object::Dictionary(dictionary! { "Type" => "Catalog" }));
    document.max_id = root;
    document.trailer.set("Root", Object::Reference((root, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

#[test]
fn public_api_opens_owned_bytes_and_reports_only_scalar_metadata() {
    let pdf = generated_pdf();
    let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();

    assert_eq!(reader.version(), "1.7");
    assert_eq!(reader.source_len(), u64::try_from(pdf.len()).unwrap());
    assert!(!reader.is_encrypted());
    assert!(!reader.is_authenticated());
    assert_eq!(reader.page_count().unwrap(), 4);

    let pages = reader.page_map().unwrap();
    assert_eq!(pages.len(), 4);
    assert_eq!(pages.get(0).unwrap().id(), (3, 0));
    assert_eq!(
        pages.iter().map(|page| page.id()).collect::<Vec<_>>(),
        vec![(3, 0), (4, 0), (5, 0), (6, 0)]
    );
    assert_eq!(pages.get(0).unwrap().inherited().resources(), Some((2, 0)));
    assert_eq!(reader.resolve_object((7, 0)).unwrap().as_dict().unwrap().len(), 1);
}

#[test]
fn cached_api_is_opt_in_bounded_and_matches_uncached_results() {
    let pdf = generated_pdf();
    let uncached = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let cache_options = IndexedReaderCacheOptions::new(4 * 64 * 1024, 16);
    assert_eq!(cache_options.max_bytes(), 4 * 64 * 1024);
    assert_eq!(cache_options.max_entries(), 16);
    assert_eq!(
        format!("{cache_options:?}"),
        "IndexedReaderCacheOptions { max_bytes: 262144, max_entries: 16 }"
    );
    assert_eq!(IndexedReaderCacheOptions::default().max_bytes(), 32 * 1024 * 1024);
    assert_eq!(IndexedReaderCacheOptions::default().max_entries(), 16 * 1024);

    let cached =
        IndexedReader::open_cached(BytesSource::from(pdf), IndexedReaderOptions::default(), cache_options).unwrap();
    for _ in 0..3 {
        assert_eq!(cached.page_map().unwrap(), uncached.page_map().unwrap());
        for id in 1..=7 {
            assert_eq!(
                cached.resolve_object((id, 0)).unwrap(),
                uncached.resolve_object((id, 0)).unwrap()
            );
        }
    }

    let stats = cached.cache_stats();
    assert!(stats.source().hits() > 0);
    assert!(stats.source().loads() > 0);
    assert!(stats.source().retained_bytes() <= cache_options.max_bytes() / 4);
    assert!(stats.source().retained_entries() <= cache_options.max_entries() / 4);
    assert!(stats.current_bytes() <= cache_options.max_bytes());
    assert!(stats.peak_bytes() <= cache_options.max_bytes());
    assert!(stats.current_entries() <= cache_options.max_entries());
    assert!(stats.peak_entries() <= cache_options.max_entries());
    assert_eq!(uncached.cache_stats(), Default::default());
}

#[test]
fn total_cache_budgets_bound_all_partitions_at_8_32_and_128_mib() {
    const MIB: u64 = 1024 * 1024;
    let pdf = stream_pdf(40, 512 * 1024);
    for total_mib in [8_u64, 32, 128] {
        let options = IndexedReaderCacheOptions::new(total_mib * MIB, 16 * 1024);
        let reader =
            IndexedReader::open_cached(BytesSource::from(pdf.clone()), IndexedReaderOptions::default(), options)
                .unwrap();
        for id in 1..=40 {
            assert_eq!(
                reader
                    .resolve_object_shared((id, 0))
                    .unwrap()
                    .as_stream()
                    .unwrap()
                    .content
                    .len(),
                512 * 1024
            );
        }

        let stats = reader.cache_stats();
        let object = stats.object();
        let object_bytes = object.probation_bytes.saturating_add(object.protected_bytes);
        let object_entries = object.probation_entries.saturating_add(object.protected_entries);
        assert!(stats.source().retained_bytes() + stats.source().in_flight_bytes() <= options.max_bytes() / 4);
        assert!(object_bytes <= usize::try_from(options.max_bytes() / 2).unwrap());
        assert!(object.probation_bytes <= usize::try_from(options.max_bytes() / 8).unwrap());
        assert!(object.protected_bytes <= usize::try_from(options.max_bytes() * 3 / 8).unwrap());
        assert!(object_entries <= options.max_entries() / 2);
        assert!(stats.object_stream().bytes <= usize::try_from(options.max_bytes() / 4).unwrap());
        assert!(stats.current_bytes() <= options.max_bytes());
        assert!(stats.peak_bytes() <= options.max_bytes());
        assert!(stats.current_entries() <= options.max_entries());
        assert!(stats.peak_entries() <= options.max_entries());
        assert!(object.object_loads >= 40);
        assert!(object.probation_bytes > usize::try_from(options.max_bytes() / 16).unwrap());
    }
}

#[test]
fn unique_three_mib_object_is_shared_promoted_and_large_source_read_bypasses() {
    const MIB: usize = 1024 * 1024;
    let pdf = stream_pdf(1, 3 * MIB);
    let options = IndexedReaderCacheOptions::default();
    let reader = IndexedReader::open_cached(BytesSource::from(pdf), IndexedReaderOptions::default(), options).unwrap();
    let first = reader.resolve_object_shared((1, 0)).unwrap();
    let second = reader.resolve_object_shared((1, 0)).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.as_stream().unwrap().content.len(), 3 * MIB);

    let stats = reader.cache_stats();
    assert_eq!(stats.object().object_loads, 1);
    assert_eq!(stats.object().object_hits, 1);
    assert_eq!(stats.object().object_promotions, 1);
    assert_eq!(stats.object().probation_entries, 0);
    assert_eq!(stats.object().protected_entries, 1);
    assert!(stats.source().bypass_reads() > 0);
    assert!(stats.source().bypass_bytes() >= 3 * u64::try_from(MIB).unwrap());
    assert!(stats.current_bytes() <= options.max_bytes());
    assert!(stats.peak_bytes() <= options.max_bytes());
}

#[test]
fn uncached_constructors_do_not_retain_shared_objects_or_cache_state() {
    let pdf = generated_pdf();
    let reader = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let first = reader.resolve_object_shared((7, 0)).unwrap();
    let second = reader.resolve_object_shared((7, 0)).unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(reader.cache_stats(), Default::default());
    assert_eq!(reader.object_cache_stats(), Default::default());
    assert_eq!(reader.object_stream_cache_stats(), Default::default());

    let erased: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::from(pdf));
    let shared = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();
    assert_eq!(shared.cache_stats(), Default::default());
}

#[test]
fn zero_cache_constructor_has_legacy_value_and_error_parity() {
    let pdf = generated_pdf();
    let uncached = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let cached = IndexedReader::open_cached(
        BytesSource::from(pdf),
        IndexedReaderOptions::default(),
        IndexedReaderCacheOptions::new(0, 0),
    )
    .unwrap();
    assert_eq!(cached.page_map().unwrap(), uncached.page_map().unwrap());
    assert_eq!(
        cached.resolve_object((7, 0)).unwrap(),
        uncached.resolve_object((7, 0)).unwrap()
    );
    let stats = cached.cache_stats();
    assert_eq!(stats.source().retained_bytes(), 0);
    assert_eq!(stats.source().retained_entries(), 0);
    assert!(stats.source().bypass_reads() > 0);
}

#[test]
fn public_options_redact_password_and_enforce_wired_caps() {
    let redacted_options = IndexedReaderOptions {
        password: Some(b"never-print-this".to_vec()),
        ..IndexedReaderOptions::default()
    };
    let debug = format!("{redacted_options:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("never-print-this"));

    let pdf = generated_pdf();
    let options = IndexedReaderOptions {
        object_bytes: 8,
        ..IndexedReaderOptions::default()
    };
    let reader = IndexedReader::open_with_options(BytesSource::from(pdf.clone()), options).unwrap();
    assert!(matches!(
        reader.resolve_object((3, 0)),
        Err(IndexedReaderError::ObjectLimitExceeded { limit: 8, .. })
    ));

    let options = IndexedReaderOptions {
        max_pages: 3,
        ..IndexedReaderOptions::default()
    };
    let reader = IndexedReader::open_with_options(BytesSource::from(pdf), options).unwrap();
    assert!(matches!(
        reader.page_count(),
        Err(IndexedReaderError::PageCountLimitExceeded { limit: 3 })
    ));
}

struct BarrierSource {
    bytes: Arc<[u8]>,
    enabled: AtomicBool,
    synchronized_calls: AtomicUsize,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    barrier: Barrier,
}

struct GateFailureSource {
    bytes: Arc<[u8]>,
    armed: AtomicBool,
    entered: Barrier,
    release: Barrier,
}

impl RandomAccessSource for GateFailureSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
            return Err(SourceError::Io(std::io::Error::other(
                "injected transient read failure",
            )));
        }
        let start = usize::try_from(offset).unwrap();
        let read = out.len().min(self.bytes.len().saturating_sub(start));
        out[..read].copy_from_slice(&self.bytes[start..start + read]);
        Ok(read)
    }
}

impl BarrierSource {
    fn enable(&self) {
        self.synchronized_calls.store(0, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
    }
}

impl RandomAccessSource for BarrierSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let synchronize =
            self.enabled.load(Ordering::SeqCst) && self.synchronized_calls.fetch_add(1, Ordering::SeqCst) < 4;
        if synchronize {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_active.fetch_max(active, Ordering::SeqCst);
            self.barrier.wait();
        }

        let start = usize::try_from(offset).unwrap();
        let read = out.len().min(self.bytes.len().saturating_sub(start));
        out[..read].copy_from_slice(&self.bytes[start..start + read]);

        if synchronize {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(read)
    }
}

fn resolved_digest(reader: &IndexedReader, ids: &[u32]) -> Vec<(u32, Object)> {
    ids.iter()
        .map(|id| (*id, reader.resolve_object((*id, 0)).unwrap()))
        .collect()
}

#[test]
fn four_threads_overlap_reads_and_repeat_the_single_thread_digest() {
    let pdf = generated_pdf();
    let source = Arc::new(BarrierSource {
        bytes: Arc::from(pdf),
        enabled: AtomicBool::new(false),
        synchronized_calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        peak_active: AtomicUsize::new(0),
        barrier: Barrier::new(4),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = Arc::new(IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap());
    let expected = resolved_digest(&reader, &[3, 4, 5, 6, 7]);

    for _ in 0..3 {
        source.enable();
        let mut threads = Vec::new();
        for id in 3..=6 {
            let reader = Arc::clone(&reader);
            threads.push(std::thread::spawn(move || {
                (id, reader.resolve_object((id, 0)).unwrap())
            }));
        }
        let mut actual: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        actual.push((7, reader.resolve_object((7, 0)).unwrap()));
        actual.sort_by_key(|(id, _)| *id);
        assert_eq!(actual, expected);
        assert_eq!(
            reader
                .page_map()
                .unwrap()
                .iter()
                .map(|page| page.id())
                .collect::<Vec<_>>(),
            vec![(3, 0), (4, 0), (5, 0), (6, 0)]
        );
    }
    assert!(source.peak_active.load(Ordering::SeqCst) >= 4);
}

#[test]
fn cached_source_and_objects_saturate_concurrently_with_shared_error_identity() {
    const MIB: usize = 1024 * 1024;
    const STREAM_BYTES: usize = 768 * 1024;
    let pdf = stream_pdf(16, STREAM_BYTES);
    let source = Arc::new(GateFailureSource {
        bytes: Arc::from(pdf),
        armed: AtomicBool::new(false),
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let options = IndexedReaderCacheOptions::new(8 * u64::try_from(MIB).unwrap(), 8);
    let reader = Arc::new(IndexedReader::open_shared_cached(erased, IndexedReaderOptions::default(), options).unwrap());

    source.armed.store(true, Ordering::SeqCst);
    let leader_reader = Arc::clone(&reader);
    let leader = std::thread::spawn(move || leader_reader.resolve_object_shared((1, 0)));
    source.entered.wait();
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let reader = Arc::clone(&reader);
            std::thread::spawn(move || reader.resolve_object_shared((1, 0)))
        })
        .collect();
    while reader.cache_stats().object().object_waits < 3 {
        std::thread::yield_now();
    }
    source.release.wait();
    let leader_error = leader.join().unwrap().unwrap_err();
    let waiter_errors: Vec<_> = waiters
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap_err())
        .collect();
    assert!(waiter_errors.iter().all(|error| Arc::ptr_eq(&leader_error, error)));
    assert!(matches!(leader_error.as_ref(), IndexedReaderError::Source(_)));

    let recovered = reader.resolve_object_shared((1, 0)).unwrap();
    assert_eq!(recovered.as_stream().unwrap().content.len(), STREAM_BYTES);
    let fatal_first = reader.resolve_object_shared((999, 0)).unwrap_err();
    let fatal_second = reader.resolve_object_shared((999, 0)).unwrap_err();
    assert!(Arc::ptr_eq(&fatal_first, &fatal_second));

    let workers: Vec<_> = (2..=16)
        .map(|id| {
            let reader = Arc::clone(&reader);
            std::thread::spawn(move || {
                assert_eq!(
                    reader
                        .resolve_object_shared((id, 0))
                        .unwrap()
                        .as_stream()
                        .unwrap()
                        .content
                        .len(),
                    STREAM_BYTES
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    let stats = reader.cache_stats();
    assert_eq!(stats.object().transient_failures, 1);
    assert_eq!(stats.object().negative_hits, 1);
    assert!(stats.object().object_evictions > 0);
    assert!(stats.source().loads() > 0);
    assert!(stats.source().bypass_reads() > 0);
    assert!(stats.current_bytes() <= options.max_bytes());
    assert!(stats.peak_bytes() <= options.max_bytes());
    assert!(stats.current_entries() <= options.max_entries());
    assert!(stats.peak_entries() <= options.max_entries());
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn indexed_reader_is_send_sync_and_drops_its_shared_source() {
    assert_send_sync::<IndexedReader>();
    let source = Arc::new(BytesSource::from(generated_pdf()));
    let weak = Arc::downgrade(&source);
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();
    drop(source);
    assert!(weak.upgrade().is_some());
    drop(reader);
    assert!(weak.upgrade().is_none());
}

#[cfg(any(unix, windows))]
#[test]
fn file_and_bytes_public_readers_match() {
    use lopdf::FileSource;
    use std::io::Write;

    let pdf = generated_pdf();
    let bytes = IndexedReader::open(BytesSource::from(pdf.clone())).unwrap();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&pdf).unwrap();
    file.flush().unwrap();
    let file = IndexedReader::open(FileSource::from_file(file).unwrap()).unwrap();

    assert_eq!(file.version(), bytes.version());
    assert_eq!(file.source_len(), bytes.source_len());
    assert_eq!(file.page_map().unwrap(), bytes.page_map().unwrap());
    assert_eq!(
        file.resolve_object((7, 0)).unwrap(),
        bytes.resolve_object((7, 0)).unwrap()
    );
}
