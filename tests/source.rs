use lopdf::{BytesSource, RandomAccessSource, SourceError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct PartialSource {
    bytes: Arc<[u8]>,
    offsets: Mutex<Vec<u64>>,
}

struct InterruptingSource {
    bytes: Arc<[u8]>,
    calls: AtomicUsize,
}

struct OverreportingSource;

impl RandomAccessSource for OverreportingSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(4)
    }

    fn read_at(&self, _offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        Ok(out.len() + 1)
    }
}

impl RandomAccessSource for InterruptingSource {
    fn len(&self) -> Result<u64, SourceError> {
        u64::try_from(self.bytes.len()).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: u64::MAX,
            limit: u64::MAX,
        })
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
        }
        let start = usize::try_from(offset).unwrap();
        let read = out.len().min(self.bytes.len() - start);
        out[..read].copy_from_slice(&self.bytes[start..start + read]);
        Ok(read)
    }
}

impl PartialSource {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: Arc::from(bytes),
            offsets: Mutex::new(Vec::new()),
        }
    }
}

impl RandomAccessSource for PartialSource {
    fn len(&self) -> Result<u64, SourceError> {
        u64::try_from(self.bytes.len()).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: u64::MAX,
            limit: u64::MAX,
        })
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        self.offsets.lock().unwrap().push(offset);
        let start = usize::try_from(offset).unwrap();
        let available = &self.bytes[start..];
        let read = available.len().min(out.len()).min(2);
        out[..read].copy_from_slice(&available[..read]);
        Ok(read)
    }
}

#[test]
fn exact_reads_trace_each_partial_offset() {
    let source = PartialSource::new(b"0123456789");
    let bytes = source.read_range(1, 5, 5).unwrap();

    assert_eq!(bytes, b"12345");
    assert_eq!(*source.offsets.lock().unwrap(), vec![1, 3, 5]);
}

#[test]
fn exact_reads_retry_interrupted_sources() {
    let source = InterruptingSource {
        bytes: Arc::from(&b"retry"[..]),
        calls: AtomicUsize::new(0),
    };
    let mut output = [0_u8; 5];

    source.read_exact_at(0, &mut output).unwrap();
    assert_eq!(&output, b"retry");
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn exact_reads_reject_a_custom_source_that_overreports_without_panicking() {
    let mut output = [0_u8; 4];
    assert!(matches!(
        OverreportingSource.read_exact_at(0, &mut output),
        Err(SourceError::InvalidReadCount {
            returned: 5,
            buffer_len: 4
        })
    ));
}

#[test]
fn byte_source_reports_eof_overflow_and_limits_structurally() {
    let source = BytesSource::from(b"abcdef".to_vec());

    assert!(matches!(
        source.read_range(4, 3, 3),
        Err(SourceError::OutOfBounds {
            offset: 4,
            length: 3,
            source_len: 6
        })
    ));
    assert!(matches!(
        source.read_range(u64::MAX, 1, 1),
        Err(SourceError::RangeOverflow {
            offset: u64::MAX,
            length: 1
        })
    ));
    assert!(matches!(
        source.read_range(0, 5, 4),
        Err(SourceError::ReadLimitExceeded { requested: 5, limit: 4 })
    ));
}

#[test]
fn byte_source_is_a_shared_immutable_snapshot() {
    let bytes: Arc<[u8]> = Arc::from(&b"snapshot"[..]);
    let source = BytesSource::new(Arc::clone(&bytes));

    assert!(Arc::ptr_eq(&bytes, &source.bytes()));
    assert_eq!(source.read_range(0, 8, 8).unwrap(), b"snapshot");
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn sources_are_send_and_sync() {
    assert_send_sync::<BytesSource>();
    #[cfg(any(unix, windows))]
    assert_send_sync::<lopdf::FileSource>();
}

#[cfg(any(unix, windows))]
mod file {
    use super::*;
    use lopdf::FileSource;
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::Barrier;

    #[test]
    fn sparse_file_offsets_remain_u64_beyond_u32() {
        let mut file = tempfile::tempfile().unwrap();
        let offset = u64::from(u32::MAX) + 17;
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"Z").unwrap();
        file.flush().unwrap();

        let source = FileSource::from_file(file).unwrap();
        assert_eq!(source.len().unwrap(), offset + 1);
        assert_eq!(source.read_range(offset, 1, 1).unwrap(), b"Z");
    }

    #[test]
    fn file_and_bytes_raw_boundaries_match() {
        let bytes = BytesSource::from(b"abcdef".to_vec());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcdef").unwrap();
        file.flush().unwrap();
        let file = FileSource::from_file(file).unwrap();

        for offset in [7, u64::MAX] {
            let mut byte_output = [0_u8; 1];
            let mut file_output = [0_u8; 1];
            assert!(matches!(
                bytes.read_at(offset, &mut byte_output),
                Err(SourceError::OutOfBounds {
                    offset: actual,
                    length: 0,
                    source_len: 6
                }) if actual == offset
            ));
            assert!(matches!(
                file.read_at(offset, &mut file_output),
                Err(SourceError::OutOfBounds {
                    offset: actual,
                    length: 0,
                    source_len: 6
                }) if actual == offset
            ));
        }

        let mut byte_empty = [];
        let mut file_empty = [];
        assert_eq!(bytes.read_at(6, &mut byte_empty).unwrap(), 0);
        assert_eq!(file.read_at(6, &mut file_empty).unwrap(), 0);

        let mut byte_tail = [0_u8; 1];
        let mut file_tail = [0_u8; 1];
        bytes.read_exact_at(5, &mut byte_tail).unwrap();
        file.read_exact_at(5, &mut file_tail).unwrap();
        assert_eq!(byte_tail, [b'f']);
        assert_eq!(file_tail, byte_tail);
    }

    #[test]
    fn truncation_after_open_reports_structured_unexpected_eof() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcdef").unwrap();
        file.flush().unwrap();
        let source = FileSource::from_file(file.try_clone().unwrap()).unwrap();

        file.set_len(2).unwrap();
        assert!(matches!(
            source.read_range(0, 6, 6),
            Err(SourceError::UnexpectedEof {
                offset: 0,
                expected: 6,
                actual: 2
            })
        ));
    }

    #[test]
    fn concurrent_reads_do_not_share_a_seek_cursor() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes: Vec<u8> = (0_u8..=255).cycle().take(16 * 1024).collect();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();

        let source = Arc::new(FileSource::from_file(file).unwrap());
        let barrier = Arc::new(Barrier::new(5));
        let mut threads = Vec::new();
        for worker in 0_u64..4 {
            let source = Arc::clone(&source);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for iteration in 0_u64..128 {
                    let offset = worker * 1024 + iteration * 17;
                    let actual = source.read_range(offset, 64, 64).unwrap();
                    let expected: Vec<u8> = (0..64).map(|delta| ((offset + delta) % 256) as u8).collect();
                    assert_eq!(actual, expected);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
