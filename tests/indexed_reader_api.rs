use lopdf::{
    BytesSource, Document, IndexedReader, IndexedReaderError, IndexedReaderOptions, Object, RandomAccessSource,
    SourceError, dictionary,
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
