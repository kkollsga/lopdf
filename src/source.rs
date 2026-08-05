//! Checked random-access byte sources.
//!
//! These sources provide cursor-free reads for indexed PDF parsing. They do not
//! prescribe caching or scheduling policy and never fall back to reading an
//! entire file into memory.

use std::sync::Arc;

use thiserror::Error;

/// Result type used by random-access sources.
pub type SourceResult<T> = std::result::Result<T, SourceError>;

/// Errors produced while validating or reading a source range.
#[derive(Debug, Error)]
pub enum SourceError {
    /// Adding a range's offset and length overflowed `u64`.
    #[error("source range overflows: offset {offset}, length {length}")]
    RangeOverflow { offset: u64, length: u64 },

    /// A complete range was requested beyond the source's captured length.
    #[error("source range is out of bounds: offset {offset}, length {length}, source length {source_len}")]
    OutOfBounds { offset: u64, length: u64, source_len: u64 },

    /// A caller requested a range larger than its explicit allocation limit.
    #[error("source read of {requested} bytes exceeds the {limit}-byte limit")]
    ReadLimitExceeded { requested: u64, limit: u64 },

    /// The requested range cannot fit in this platform's address space.
    #[error("source read of {requested} bytes exceeds the platform limit of {limit} bytes")]
    PlatformLimitExceeded { requested: u64, limit: u64 },

    /// Reserving the requested output buffer failed.
    #[error("could not allocate {requested} bytes for a source read")]
    AllocationFailed { requested: u64 },

    /// A source ended after its length check but before the requested range was complete.
    #[error("source ended during read: offset {offset}, expected {expected} bytes, read {actual} bytes")]
    UnexpectedEof { offset: u64, expected: u64, actual: u64 },

    /// The platform's positional read failed.
    #[error("source I/O error")]
    Io(#[from] std::io::Error),
}

/// A cursor-free byte source that supports concurrent independent reads.
///
/// Implementations may return fewer bytes than `out.len()` from [`read_at`].
/// Callers that require a complete range should use [`read_exact_at`] or
/// [`read_range`], which loop over partial reads and validate all arithmetic.
///
/// [`read_at`]: RandomAccessSource::read_at
/// [`read_exact_at`]: RandomAccessSource::read_exact_at
/// [`read_range`]: RandomAccessSource::read_range
pub trait RandomAccessSource: Send + Sync + 'static {
    /// Return the source length used for range validation.
    fn len(&self) -> SourceResult<u64>;

    /// Read up to `out.len()` bytes starting at `offset` without changing a
    /// shared cursor.
    fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize>;

    /// Return whether the source is empty.
    fn is_empty(&self) -> SourceResult<bool> {
        Ok(self.len()? == 0)
    }

    /// Fill `out` from `offset`, retrying partial and interrupted reads.
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<()> {
        let length = u64::try_from(out.len()).map_err(|_| SourceError::RangeOverflow {
            offset,
            length: u64::MAX,
        })?;
        validate_range(offset, length, self.len()?)?;

        let mut actual = 0_u64;
        while actual < length {
            let completed = usize::try_from(actual).map_err(|_| SourceError::PlatformLimitExceeded {
                requested: actual,
                limit: platform_limit(),
            })?;
            match self.read_at(offset + actual, &mut out[completed..]) {
                Ok(0) => {
                    return Err(SourceError::UnexpectedEof {
                        offset,
                        expected: length,
                        actual,
                    });
                }
                Ok(read) => {
                    let read = u64::try_from(read).map_err(|_| SourceError::RangeOverflow {
                        offset,
                        length: u64::MAX,
                    })?;
                    actual = actual
                        .checked_add(read)
                        .ok_or(SourceError::RangeOverflow { offset, length })?;
                }
                Err(SourceError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Read an owned range after enforcing an explicit allocation limit.
    fn read_range(&self, offset: u64, length: u64, limit: u64) -> SourceResult<Vec<u8>> {
        if length > limit {
            return Err(SourceError::ReadLimitExceeded {
                requested: length,
                limit,
            });
        }
        validate_range(offset, length, self.len()?)?;

        let output_len = usize::try_from(length).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: length,
            limit: platform_limit(),
        })?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| SourceError::AllocationFailed { requested: length })?;
        output.resize(output_len, 0);
        self.read_exact_at(offset, &mut output)?;
        Ok(output)
    }
}

fn platform_limit() -> u64 {
    u64::try_from(usize::MAX).unwrap_or(u64::MAX)
}

fn validate_range(offset: u64, length: u64, source_len: u64) -> SourceResult<()> {
    let end = offset
        .checked_add(length)
        .ok_or(SourceError::RangeOverflow { offset, length })?;
    if end > source_len {
        return Err(SourceError::OutOfBounds {
            offset,
            length,
            source_len,
        });
    }
    Ok(())
}

/// An immutable random-access source backed by shared owned bytes.
#[derive(Clone, Debug)]
pub struct BytesSource {
    bytes: Arc<[u8]>,
}

impl BytesSource {
    /// Create a source without copying the provided shared bytes.
    pub fn new(bytes: Arc<[u8]>) -> Self {
        Self { bytes }
    }

    /// Return a clone of the source's shared byte allocation.
    pub fn bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

impl From<Arc<[u8]>> for BytesSource {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self::new(bytes)
    }
}

impl From<Vec<u8>> for BytesSource {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes.into())
    }
}

impl RandomAccessSource for BytesSource {
    fn len(&self) -> SourceResult<u64> {
        u64::try_from(self.bytes.len()).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: u64::MAX,
            limit: u64::MAX,
        })
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
        let source_len = self.len()?;
        if offset > source_len {
            return Err(SourceError::OutOfBounds {
                offset,
                length: 0,
                source_len,
            });
        }
        let start = usize::try_from(offset).map_err(|_| SourceError::PlatformLimitExceeded {
            requested: offset,
            limit: platform_limit(),
        })?;
        let available = &self.bytes[start..];
        let read = available.len().min(out.len());
        out[..read].copy_from_slice(&available[..read]);
        Ok(read)
    }
}

/// A file-backed source using operating-system positional reads.
///
/// The file length is captured when the source is created. Later truncation
/// therefore fails closed as an unexpected EOF; path replacement cannot
/// retarget the owned descriptor.
#[cfg(any(unix, windows))]
#[derive(Debug)]
pub struct FileSource {
    file: std::fs::File,
    len: u64,
}

#[cfg(any(unix, windows))]
impl FileSource {
    /// Open a path read-only and capture its current length.
    pub fn open(path: impl AsRef<std::path::Path>) -> SourceResult<Self> {
        Self::from_file(std::fs::File::open(path)?)
    }

    /// Adopt an already-open read-only file and capture its current length.
    pub fn from_file(file: std::fs::File) -> SourceResult<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

#[cfg(any(unix, windows))]
impl RandomAccessSource for FileSource {
    fn len(&self) -> SourceResult<u64> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
        if offset > self.len {
            return Err(SourceError::OutOfBounds {
                offset,
                length: 0,
                source_len: self.len,
            });
        }
        if out.is_empty() || offset == self.len {
            return Ok(0);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(out, offset).map_err(SourceError::from)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(out, offset).map_err(SourceError::from)
        }
    }
}
