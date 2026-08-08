//! Compare the eager `Document` path against the read-only `IndexedReader` on
//! the same workload: open a PDF, enumerate its pages, then fetch and decompress
//! every page's content stream(s).
//!
//! One process does one (file, mode) measurement, so wall clock and peak RSS can
//! be attributed cleanly. Run it under `/usr/bin/time -l` (macOS) or
//! `/usr/bin/time -v` (GNU) to capture peak resident set size.
//!
//! ```text
//! cargo run --release --example indexed_vs_eager_bench -- eager   big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- indexed big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- eager-open   big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- indexed-open big.pdf
//! ```
//!
//! Modes:
//!
//! - `eager`: `Document::load` + `get_pages` + `get_page_content` per page.
//! - `indexed`: `IndexedReader::open_cached` + `page_map`, then per page locate each
//!   content stream, read its encoded payload through the bounded 64 KiB chunk
//!   reader, and decode it.
//! - `eager-open`: the open half of `eager` only (`Document::load` + `get_pages`).
//! - `indexed-open`: the open half of `indexed` only (`open_cached` + `page_map`).
//!
//! Every mode prints one JSON line. `digest` is a SHA-256 over every page's
//! decompressed content bytes in page order, so the two full modes can be checked
//! for byte identity by comparing that one field.

use std::process::ExitCode;
use std::time::Instant;

use lopdf::{
    Dictionary, Document, IndexedReader, IndexedReaderCacheOptions, IndexedReaderOptions, IndexedStreamReadError,
    Object, ObjectId, Stream,
};
use sha2::{Digest, Sha256};

/// Byte budget for the indexed reader's caches, split by the reader across
/// source chunks, resolved objects and decoded object streams.
const DEFAULT_CACHE_BYTES: u64 = 64 << 20;
/// Entry budget for the same caches.
const DEFAULT_CACHE_ENTRIES: usize = 4096;
/// Buffer handed to `EncodedStreamReader::read_chunk`, which fills at most 64 KiB.
const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Eager,
    Indexed,
    EagerOpen,
    IndexedOpen,
}

impl Mode {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "eager" => Some(Self::Eager),
            "indexed" => Some(Self::Indexed),
            "eager-open" => Some(Self::EagerOpen),
            "indexed-open" => Some(Self::IndexedOpen),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Eager => "eager",
            Self::Indexed => "indexed",
            Self::EagerOpen => "eager-open",
            Self::IndexedOpen => "indexed-open",
        }
    }

    const fn opens_only(self) -> bool {
        matches!(self, Self::EagerOpen | Self::IndexedOpen)
    }
}

struct Measurement {
    /// `Document::load` alone, or `IndexedReader::open_cached` alone: the point
    /// at which the first object could be asked for.
    index_ns: u128,
    /// `index_ns` plus page enumeration (`get_pages` / `page_map`).
    open_ns: u128,
    total_ns: u128,
    pages: usize,
    content_bytes: u64,
    digest: String,
    page_digests: Vec<String>,
}

/// Running SHA-256 over page content, plus per-page digests when asked for.
struct ContentHasher {
    overall: Sha256,
    page_digests: Vec<String>,
    per_page: bool,
    total_bytes: u64,
}

impl ContentHasher {
    fn new(per_page: bool) -> Self {
        Self {
            overall: Sha256::new(),
            page_digests: Vec::new(),
            per_page,
            total_bytes: 0,
        }
    }

    fn push_page(&mut self, index: usize, content: &[u8]) {
        // Length-prefix each page so a shift of bytes across a page boundary
        // cannot hash the same as the correct split.
        self.overall.update((index as u64).to_le_bytes());
        self.overall.update((content.len() as u64).to_le_bytes());
        self.overall.update(content);
        self.total_bytes += content.len() as u64;
        if self.per_page {
            let mut page = Sha256::new();
            page.update(content);
            self.page_digests.push(hex(&page.finalize()));
        }
    }

    fn finish(self) -> (String, Vec<String>, u64) {
        (hex(&self.overall.finalize()), self.page_digests, self.total_bytes)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(mode_text) = args.next() else {
        eprintln!("usage: indexed_vs_eager_bench <eager|indexed|eager-open|indexed-open> <file.pdf> [options]");
        return ExitCode::from(2);
    };
    let Some(mode) = Mode::parse(&mode_text) else {
        eprintln!("unknown mode: {mode_text}");
        return ExitCode::from(2);
    };
    let Some(path) = args.next() else {
        eprintln!("missing PDF path");
        return ExitCode::from(2);
    };

    let mut cache_bytes = DEFAULT_CACHE_BYTES;
    let mut cache_entries = DEFAULT_CACHE_ENTRIES;
    let mut per_page = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--cache-bytes" => match args.next().and_then(|value| value.parse().ok()) {
                Some(value) => cache_bytes = value,
                None => {
                    eprintln!("--cache-bytes needs a number");
                    return ExitCode::from(2);
                }
            },
            "--cache-entries" => match args.next().and_then(|value| value.parse().ok()) {
                Some(value) => cache_entries = value,
                None => {
                    eprintln!("--cache-entries needs a number");
                    return ExitCode::from(2);
                }
            },
            "--page-digests" => per_page = true,
            other => {
                eprintln!("unknown option: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let file_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);

    let outcome = match mode {
        Mode::Eager | Mode::EagerOpen => run_eager(&path, mode, per_page),
        Mode::Indexed | Mode::IndexedOpen => run_indexed(&path, mode, cache_bytes, cache_entries, per_page),
    };

    match outcome {
        Ok(measurement) => {
            let page_digests = if per_page {
                let joined = measurement
                    .page_digests
                    .iter()
                    .map(|digest| format!("\"{digest}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(",\"page_digests\":[{joined}]")
            } else {
                String::new()
            };
            println!(
                "{{\"mode\":\"{}\",\"path\":\"{}\",\"file_bytes\":{},\"index_ns\":{},\"open_ns\":{},\"total_ns\":{},\"pages\":{},\"content_bytes\":{},\"digest\":\"{}\"{}}}",
                mode.name(),
                path.replace('\\', "\\\\").replace('"', "\\\""),
                file_bytes,
                measurement.index_ns,
                measurement.open_ns,
                measurement.total_ns,
                measurement.pages,
                measurement.content_bytes,
                measurement.digest,
                page_digests,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{}: {message}", mode.name());
            ExitCode::FAILURE
        }
    }
}

fn run_eager(path: &str, mode: Mode, per_page: bool) -> Result<Measurement, String> {
    let started = Instant::now();
    let document = Document::load(path).map_err(|error| error.to_string())?;
    let index_ns = started.elapsed().as_nanos();
    let pages = document.get_pages();
    let open_ns = started.elapsed().as_nanos();

    let mut hasher = ContentHasher::new(per_page);
    if !mode.opens_only() {
        for (index, (_, page_id)) in pages.iter().enumerate() {
            let content = document.get_page_content(*page_id);
            hasher.push_page(index, &content);
        }
    }
    let total_ns = started.elapsed().as_nanos();
    let (digest, page_digests, content_bytes) = hasher.finish();

    Ok(Measurement {
        index_ns,
        open_ns,
        total_ns,
        pages: pages.len(),
        content_bytes,
        digest,
        page_digests,
    })
}

fn run_indexed(
    path: &str, mode: Mode, cache_bytes: u64, cache_entries: usize, per_page: bool,
) -> Result<Measurement, String> {
    let started = Instant::now();
    let source = lopdf::FileSource::open(path).map_err(|error| error.to_string())?;
    let reader = IndexedReader::open_cached(
        source,
        IndexedReaderOptions::default(),
        IndexedReaderCacheOptions::new(cache_bytes, cache_entries),
    )
    .map_err(|error| error.to_string())?;
    let index_ns = started.elapsed().as_nanos();
    let page_map = reader.page_map().map_err(|error| error.to_string())?;
    let open_ns = started.elapsed().as_nanos();

    let mut hasher = ContentHasher::new(per_page);
    if !mode.opens_only() {
        for (index, entry) in page_map.iter().enumerate() {
            let content = indexed_page_content(&reader, entry.id())?;
            hasher.push_page(index, &content);
        }
    }
    let total_ns = started.elapsed().as_nanos();
    let (digest, page_digests, content_bytes) = hasher.finish();

    Ok(Measurement {
        index_ns,
        open_ns,
        total_ns,
        pages: page_map.len(),
        content_bytes,
        digest,
        page_digests,
    })
}

/// Mirror of `Document::get_page_content` over the indexed reader.
///
/// Same content-stream selection, same per-stream decode, same `\n` separator,
/// same fall back to the raw bytes when a stream will not decode.
fn indexed_page_content(reader: &IndexedReader, page_id: ObjectId) -> Result<Vec<u8>, String> {
    let page = reader.resolve_object(page_id).map_err(|error| error.to_string())?;
    let Ok(dictionary) = page.as_dict() else {
        return Ok(Vec::new());
    };
    let mut content = Vec::new();
    for stream_id in indexed_page_content_ids(reader, dictionary) {
        let Some((dictionary, encoded)) = read_encoded_stream(reader, stream_id)? else {
            continue;
        };
        let stream = Stream::new(dictionary, encoded);
        match stream.decompressed_content() {
            Ok(decoded) => content.extend_from_slice(&decoded),
            Err(_) => content.extend_from_slice(&stream.content),
        }
        content.push(b'\n');
    }
    Ok(content)
}

/// The `/Contents` entry is either one stream reference or an array of them, and
/// may sit behind a chain of indirect references.
fn indexed_page_content_ids(reader: &IndexedReader, page: &Dictionary) -> Vec<ObjectId> {
    const DEREF_LIMIT: usize = 128;

    let mut ids = Vec::new();
    let Ok(first) = page.get(b"Contents") else {
        return ids;
    };
    let mut current = first.clone();
    let mut dereferences = 0usize;
    loop {
        match current {
            Object::Reference(id) => {
                // A reference straight to a stream is the common case; anything
                // else is followed like the eager reader follows it. A compressed
                // object is never a stream, so `NotNormalObject` is also a
                // "keep dereferencing" answer, not a failure.
                match reader.resolve_stream_descriptor(id) {
                    Ok(_) => {
                        ids.push(id);
                        break;
                    }
                    Err(IndexedStreamReadError::NotStream { .. })
                    | Err(IndexedStreamReadError::NotNormalObject { .. }) => {
                        dereferences += 1;
                        if dereferences >= DEREF_LIMIT {
                            break;
                        }
                        match reader.resolve_object(id) {
                            Ok(object) => current = object,
                            Err(_) => break,
                        }
                    }
                    Err(_) => {
                        ids.push(id);
                        break;
                    }
                }
            }
            Object::Array(items) => {
                for item in items {
                    if let Ok(id) = item.as_reference() {
                        ids.push(id);
                    }
                }
                break;
            }
            _ => break,
        }
    }
    ids
}

/// Locate a stream's encoded payload and pull it in through the bounded chunk
/// reader, which never hands back more than 64 KiB per call.
///
/// Falls back to ordinary object resolution for a payload the chunk reader will
/// not expose, which is what encryption is: the descriptor refuses plaintext
/// rather than handing back ciphertext, and resolution decrypts.
fn read_encoded_stream(reader: &IndexedReader, id: ObjectId) -> Result<Option<(Dictionary, Vec<u8>)>, String> {
    let descriptor = match reader.resolve_stream_descriptor(id) {
        Ok(descriptor) => descriptor,
        Err(IndexedStreamReadError::NotStream { .. }) | Err(IndexedStreamReadError::NotNormalObject { .. }) => {
            return Ok(None);
        }
        Err(_) => return resolve_stream_fallback(reader, id),
    };
    let mut chunks = match descriptor.open_plain_encoded() {
        Ok(chunks) => chunks,
        Err(_) => return resolve_stream_fallback(reader, id),
    };
    let capacity = usize::try_from(descriptor.encoded_len().unwrap_or(0)).unwrap_or(0);
    let mut encoded = Vec::with_capacity(capacity);
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = chunks.read_chunk(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        encoded.extend_from_slice(&buffer[..read]);
    }
    Ok(Some((descriptor.dictionary().clone(), encoded)))
}

fn resolve_stream_fallback(reader: &IndexedReader, id: ObjectId) -> Result<Option<(Dictionary, Vec<u8>)>, String> {
    match reader.resolve_object(id) {
        Ok(Object::Stream(stream)) => Ok(Some((stream.dict, stream.content))),
        Ok(_) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
