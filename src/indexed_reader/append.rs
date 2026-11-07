//! The append writer: new and changed objects written as an incremental update.
//!
//! This is the only way this fork's indexed route changes a PDF. It adds bytes at the end and
//! never touches one before them, so a signature over the earlier bytes stays over the same
//! bytes. It works from an open [`IndexedReader`] and a streaming output:
//!
//! - **Memory** is proportional to what's written: about 40 bytes for each new object, a 64 KiB
//!   output buffer, and one 64 KiB buffer while copying. The original file is read in 64 KiB
//!   pieces, and not at all if the caller appends to the file itself ([`AppendWriter::tail_only`]).
//! - **Objects go out as they're given**, so a large stream can be written from any [`Read`] and is
//!   never in memory whole ([`AppendWriter::write_stream_from`]).
//! - **The cross-reference section matches the file's own form.** A table for a table or a hybrid
//!   file, a stream for a stream file. A hybrid's new table carries no `/XRefStm`, since its
//!   supplement belongs to the section it came with, which the `/Prev` still reaches.
//! - **Offsets come from the writer**, returned for each object, so nothing later has to search
//!   the tail for them, which attacker text could confuse.
//!
//! # What it refuses
//!
//! Each of these is a file where an append could damage the document or claim more than it knows,
//! so [`IndexedReader::check_appendable`] refuses it before a byte is written:
//!
//! - **A rebuilt index.** A file opened by rescanning has no `/Prev` to chain from. An update to it
//!   would leave the old sections unreachable.
//! - **Encryption.** New strings and streams would have to be encrypted with the file's key, and
//!   none of that is built here. Written in the clear, they'd read back as garbage.
//! - **A `%PDF-` header that isn't at byte 0.** Offsets in such a file count from the header, and
//!   not every reader agrees on that.
//! - **A `/Prev` loop.** The walk stops at the loop, so older sections were never read.
//! - **A full chain.** One more section than [`IndexedReaderOptions::max_xref_sections`] couldn't be
//!   opened afterwards.
//! - **An ambiguous hybrid.** A `/XRefStm` that redefines an object its own table has live, or one on
//!   a stream section. Readers differ on which wins, and this writer can't tell which one a later
//!   signature's checker would believe.
//! - **A `/Root` that doesn't resolve to a dictionary.** Such a file only looks like a document.
//!
//! Limits are refused too: a table's 10-digit offsets, and object numbers past what PDFium reads.

use std::io::{BufWriter, Read, Write};

use sha2::{Digest, Sha256};

use super::revisions::{ADDED_LINE_ENDING, COPY_CHUNK_BYTES, CopyError, copy_prefix};
use super::*;
use crate::writer::Writer;

/// PDFium ignores object numbers above this (`CPDF_Parser::kMaxObjectNumber`).
const MAX_OBJECT_NUMBER: u32 = 4 * 1_024 * 1_024;
/// Objects in one update. At 20 bytes each their table stays well inside the 16 MiB the reader
/// allows one section, so an update can always be read back.
const MAX_UPDATE_OBJECTS: usize = 500_000;
/// A classic table's offset field is 10 digits.
const MAX_TABLE_OFFSET: u64 = 9_999_999_999;
/// Nesting allowed in an object, the same as the parser's own.
const MAX_OBJECT_DEPTH: usize = 100;
/// What's dropped from the old trailer: entries this update states again, and entries that describe
/// the old section's own stream rather than the document.
const TRAILER_KEYS_NOT_CARRIED: [&[u8]; 13] = [
    b"Size",
    b"Prev",
    b"XRefStm",
    b"Type",
    b"W",
    b"Index",
    b"Length",
    b"Filter",
    b"DecodeParms",
    b"DL",
    b"F",
    b"FFilter",
    b"FDecodeParms",
];

/// Result of an append operation.
pub type AppendResult<T> = std::result::Result<T, AppendError>;

/// Why an append was refused or failed. If this comes back, no finished update exists: at worst
/// `out` holds an unfinished tail, which [`append_atomically`] deletes for you.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AppendError {
    #[error("can't append: this file's cross-reference data is damaged and was rebuilt by a scan")]
    RecoveredIndex,
    #[error("can't append: this file is encrypted, and writing encrypted updates isn't supported")]
    Encrypted,
    #[error("can't append: the PDF header starts at byte {offset}, not byte 0")]
    HeaderNotAtStart { offset: u64 },
    #[error("can't append: this file's /Prev chain has a loop")]
    PrevLoop,
    #[error("can't append: this file already has {sections} sections, and one more couldn't be opened")]
    ChainFull { sections: usize },
    #[error("can't append: this file's /XRefStm can be read two ways (object {object})")]
    AmbiguousHybrid { object: u32 },
    #[error("can't append: the trailer's /Root isn't a reference to a dictionary")]
    InvalidRoot,
    #[error("can't append: offset {offset} doesn't fit a cross-reference table's 10 digits")]
    OffsetTooLarge { offset: u64 },
    #[error("can't append: object number {number} is above the limit of {limit}")]
    ObjectNumberTooLarge { number: u64, limit: u32 },
    #[error("can't append: one update can hold at most {limit} objects")]
    TooManyObjects { limit: usize },
    #[error("object {id:?} isn't in this file and wasn't reserved with allocate()")]
    UnknownObject { id: crate::ObjectId },
    #[error("object {id:?} is free. Use a new number from allocate()")]
    ObjectIsFree { id: crate::ObjectId },
    #[error("object {id:?} has generation {expected} in this file")]
    GenerationMismatch { id: crate::ObjectId, expected: u16 },
    #[error("object {id:?} was already written in this update")]
    AlreadyWritten { id: crate::ObjectId },
    #[error("object {id:?} is an object stream with live members. Replacing it would lose them")]
    ObjectStreamContainer { id: crate::ObjectId },
    #[error("object {id:?} can't be written: {reason}")]
    InvalidObject { id: crate::ObjectId, reason: &'static str },
    #[error("stream {id:?} declared {declared} bytes, and its reader gave a different number")]
    StreamLengthMismatch { id: crate::ObjectId, declared: u64 },
    #[error("object {id:?} was reserved and never written")]
    ReservedNotWritten { id: crate::ObjectId },
    #[error("the trailer would name object {id:?}, which isn't in the file")]
    TrailerTargetMissing { id: crate::ObjectId },
    #[error("an update has to hold at least one object")]
    EmptyUpdate,
    #[error("an earlier write failed, so this update can't be finished")]
    Abandoned,
    #[error("the written file failed its check: {reason}")]
    VerificationFailed { reason: &'static str },
    #[error(transparent)]
    Reader(#[from] IndexedReaderError),
    #[error("couldn't read the original file")]
    Source(#[from] SourceError),
    #[error("couldn't write the output")]
    Io(#[from] std::io::Error),
}

/// Where one object went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AppendedObject {
    pub id: crate::ObjectId,
    /// Offset of its `N G obj` from the start of the file.
    pub offset: u64,
    /// Bytes from `N G obj` through `endobj` and its line ending.
    pub length: u64,
}

/// What a finished update wrote.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct AppendSummary {
    /// Length of the file before the update. These bytes didn't change.
    pub previous_len: u64,
    /// Length with the update.
    pub new_len: u64,
    /// The update's cross-reference section, which `startxref` names.
    pub xref_offset: u64,
    /// The `/Prev` it states.
    pub prev: u64,
    /// The `/Size` it states.
    pub size: u64,
    /// The objects the caller wrote, in number order. A cross-reference stream's own object isn't
    /// among them.
    pub objects: Vec<AppendedObject>,
}

impl IndexedReader {
    /// Whether this file can take an incremental update, and if not, why.
    ///
    /// The checks cost resolving the catalog and nothing more, so a caller can use this to turn Save
    /// off before anyone edits anything.
    ///
    /// # Errors
    ///
    /// One of the refusals listed in the [module docs](self).
    pub fn check_appendable(&self) -> AppendResult<()> {
        let index = &self.index;
        self.whole_file_source().validate_unchanged()?;
        if index.recovered {
            return Err(AppendError::RecoveredIndex);
        }
        if self.is_encrypted() {
            return Err(AppendError::Encrypted);
        }
        if index.source_origin != 0 {
            return Err(AppendError::HeaderNotAtStart {
                offset: index.source_origin,
            });
        }
        if index.chain.has_loop {
            return Err(AppendError::PrevLoop);
        }
        if index.chain.sections >= self.options.max_xref_sections {
            return Err(AppendError::ChainFull {
                sections: index.chain.sections,
            });
        }
        if let Some(object) = index.chain.ambiguous_hybrid {
            return Err(AppendError::AmbiguousHybrid { object });
        }
        let root = index
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| AppendError::InvalidRoot)?;
        match self.resolve_object(root) {
            Ok(Object::Dictionary(_)) => {}
            Err(IndexedReaderError::Source(error)) => return Err(error.into()),
            _ => return Err(AppendError::InvalidRoot),
        }
        if index.xref_type == IndexXrefType::Table && index.source_len > MAX_TABLE_OFFSET {
            return Err(AppendError::OffsetTooLarge {
                offset: index.source_len,
            });
        }
        Ok(())
    }

    /// Check a finished update against what the writer says it wrote, and return a reader of the
    /// new file: a bad update is worse than none, so nothing should replace the original
    /// before this passes.
    ///
    /// Compares every original byte, in small pieces. Checks that the new file's chain starts at
    /// the update and reaches the old one, that every object written is where the summary says, and
    /// that every other object is exactly where it was.
    ///
    /// # Errors
    ///
    /// [`VerificationFailed`](AppendError::VerificationFailed) naming what didn't match, or a reader
    /// error if the new file doesn't open.
    pub fn verify_appended(
        &self, appended: Arc<dyn RandomAccessSource>, summary: &AppendSummary,
    ) -> AppendResult<IndexedReader> {
        let failed = |reason| Err(AppendError::VerificationFailed { reason });
        if appended.len()? != summary.new_len || summary.previous_len != self.index.source_len {
            return failed("its length isn't what was written");
        }
        let original = self.whole_file_source();
        let mut before = vec![0_u8; COPY_CHUNK_BYTES];
        let mut after = vec![0_u8; COPY_CHUNK_BYTES];
        let mut position = 0_u64;
        while position < summary.previous_len {
            let step = usize::try_from(summary.previous_len - position)
                .map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            original.read_exact_at(position, &mut before[..step])?;
            appended.read_exact_at(position, &mut after[..step])?;
            if before[..step] != after[..step] {
                return failed("the original bytes changed");
            }
            position += u64::try_from(step).unwrap_or(u64::MAX);
        }
        drop((before, after));

        let reader = Self::open_shared(appended, self.options.clone())?;
        let index = &reader.index;
        if index.recovered {
            return failed("it needed a rescan to open");
        }
        if index.xref_start != summary.xref_offset {
            return failed("startxref doesn't name the new section");
        }
        if trailer_offset(&index.trailer, b"Prev", "Prev")? != Some(self.index.xref_start) {
            return failed("/Prev doesn't name the old section");
        }
        if index.declared_size < self.index.declared_size || index.declared_size != summary.size {
            return failed("/Size is wrong");
        }

        let mut written = BTreeMap::new();
        for object in &summary.objects {
            let found = index.locations.get(&object.id.0);
            if found
                != Some(&ObjectLocation64::Normal {
                    offset: object.offset,
                    generation: object.id.1,
                })
            {
                return failed("an object isn't where it was written");
            }
            let header = format!("{} {} obj", object.id.0, object.id.1);
            let probe = reader.whole_file_source().read_range(
                object.offset,
                u64::try_from(header.len()).unwrap_or(u64::MAX),
                INDIRECT_HEADER_LIMIT,
            )?;
            if probe != header.as_bytes() {
                return failed("an object's header isn't at its offset");
            }
            written.insert(object.id.0, ());
        }
        for (number, location) in &self.index.locations {
            if !written.contains_key(number) && index.locations.get(number) != Some(location) {
                return failed("an object that wasn't written moved");
            }
        }
        let root =
            index
                .trailer
                .get(b"Root")
                .and_then(Object::as_reference)
                .map_err(|_| AppendError::VerificationFailed {
                    reason: "/Root is gone",
                })?;
        if !matches!(reader.resolve_object(root), Ok(Object::Dictionary(_))) {
            return failed("/Root no longer resolves");
        }
        Ok(reader)
    }
}

/// One object's place in the new cross-reference section. No offset means reserved, not yet
/// written.
#[derive(Clone, Copy)]
struct Pending {
    generation: u16,
    placed: Option<(u64, u64)>,
}

/// An output that counts its bytes and hashes them, for the changing half of `/ID`.
struct Sink<W: Write> {
    out: BufWriter<W>,
    position: u64,
    hash: Sha256,
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.position += u64::try_from(written).unwrap_or(u64::MAX);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// An incremental update being written. Build one with [`new`](Self::new) or
/// [`tail_only`](Self::tail_only), write objects, and call [`finish`](Self::finish).
///
/// Dropping it unfinished leaves `out` holding a partial tail with no `startxref`. If `out` is the
/// original file, readers still find the old `%%EOF`, but the bytes stay, so write to a temporary
/// file where that matters: [`append_atomically`] does.
pub struct AppendWriter<'r, W: Write> {
    reader: &'r IndexedReader,
    sink: Sink<W>,
    previous_len: u64,
    entries: BTreeMap<u32, Pending>,
    next_number: u32,
    /// Object-stream containers with live members, built the first time an object is replaced.
    containers: Option<HashSet<u32>>,
    new_root: Option<crate::ObjectId>,
    new_info: Option<Object>,
    abandoned: bool,
}

impl<'r, W: Write> AppendWriter<'r, W> {
    /// Start an update on a new output: the whole of the original file is copied to `out` first, in
    /// small pieces.
    ///
    /// # Errors
    ///
    /// Any refusal from [`IndexedReader::check_appendable`], and any read or write failure.
    pub fn new(reader: &'r IndexedReader, out: W) -> AppendResult<Self> {
        let mut writer = Self::start(reader, out)?;
        copy_prefix(
            reader.whole_file_source().as_ref(),
            reader.index.source_len,
            &mut writer.sink.out,
        )
        .map_err(|error| match error {
            CopyError::Source(error) => AppendError::Source(error),
            CopyError::Sink(error) => AppendError::Io(error),
        })?;
        writer.begin_tail()?;
        Ok(writer)
    }

    /// Start an update where `out` already holds the file, a file opened to append to, say. Only the
    /// new bytes are written, and the original isn't read beyond its last byte.
    ///
    /// A reader still open on a file appended to this way reports
    /// [`SourceChanged`](SourceError::SourceChanged) from then on, and needs reopening.
    ///
    /// # Errors
    ///
    /// Any refusal from [`IndexedReader::check_appendable`], and any read or write failure.
    pub fn tail_only(reader: &'r IndexedReader, out: W) -> AppendResult<Self> {
        let mut writer = Self::start(reader, out)?;
        writer.begin_tail()?;
        Ok(writer)
    }

    fn start(reader: &'r IndexedReader, out: W) -> AppendResult<Self> {
        reader.check_appendable()?;
        let source = reader.whole_file_source();
        let previous_len = reader.index.source_len;
        if source.len()? != previous_len {
            return Err(SourceError::SourceChanged.into());
        }
        let highest_known = reader
            .index
            .locations
            .keys()
            .next_back()
            .map_or(1, |highest| u64::from(*highest) + 1);
        let next_number = reader.index.declared_size.max(highest_known).max(1);
        // A number past u32 can't be allocated. Anything allocate() would refuse anyway stays at the top.
        let next_number = u32::try_from(next_number).unwrap_or(u32::MAX);
        Ok(Self {
            reader,
            sink: Sink {
                out: BufWriter::with_capacity(COPY_CHUNK_BYTES, out),
                position: previous_len,
                hash: Sha256::new(),
            },
            previous_len,
            entries: BTreeMap::new(),
            next_number,
            containers: None,
            new_root: None,
            new_info: None,
            abandoned: false,
        })
    }

    /// Whatever the file ends with, an object header has to start a line.
    fn begin_tail(&mut self) -> AppendResult<()> {
        let last = self
            .reader
            .whole_file_source()
            .read_range(self.previous_len.saturating_sub(1), 1, 1)?;
        if !matches!(last.first(), Some(b'\n' | b'\r')) {
            self.emit(ADDED_LINE_ENDING)?;
        }
        Ok(())
    }

    /// Reserve a new object number, so that objects can refer to each other before they're written.
    /// Every number reserved has to be written before [`finish`](Self::finish).
    ///
    /// # Errors
    ///
    /// [`ObjectNumberTooLarge`](AppendError::ObjectNumberTooLarge) past what PDFium would read, and
    /// [`TooManyObjects`](AppendError::TooManyObjects) if the update is full.
    pub fn allocate(&mut self) -> AppendResult<crate::ObjectId> {
        let number = self.take_number()?;
        self.check_room()?;
        self.entries.insert(
            number,
            Pending {
                generation: 0,
                placed: None,
            },
        );
        Ok((number, 0))
    }

    fn take_number(&mut self) -> AppendResult<u32> {
        let number = self.next_number;
        if number > MAX_OBJECT_NUMBER {
            return Err(AppendError::ObjectNumberTooLarge {
                number: u64::from(number),
                limit: MAX_OBJECT_NUMBER,
            });
        }
        self.next_number = number + 1;
        Ok(number)
    }

    fn check_room(&self) -> AppendResult<()> {
        if self.entries.len() >= MAX_UPDATE_OBJECTS {
            return Err(AppendError::TooManyObjects {
                limit: MAX_UPDATE_OBJECTS,
            });
        }
        Ok(())
    }

    /// Write a new object under a new number.
    ///
    /// # Errors
    ///
    /// As [`allocate`](Self::allocate) and [`write_object`](Self::write_object).
    pub fn add_object(&mut self, object: &Object) -> AppendResult<AppendedObject> {
        let id = self.allocate()?;
        self.write_object(id, object)
    }

    /// Write an object: one reserved with [`allocate`](Self::allocate), or a new version of a live
    /// object at its own generation. A stream's `/Length` is set to its content's length.
    ///
    /// # Errors
    ///
    /// [`UnknownObject`](AppendError::UnknownObject), [`ObjectIsFree`](AppendError::ObjectIsFree),
    /// [`GenerationMismatch`](AppendError::GenerationMismatch), [`AlreadyWritten`](AppendError::AlreadyWritten)
    /// or [`ObjectStreamContainer`](AppendError::ObjectStreamContainer) if the number can't take it;
    /// [`InvalidObject`](AppendError::InvalidObject) if the object has no valid PDF form.
    pub fn write_object(&mut self, id: crate::ObjectId, object: &Object) -> AppendResult<AppendedObject> {
        self.check_writable(id)?;
        check_object(id, object, 0, true)?;
        let start = self.begin_object(id)?;
        match object {
            Object::Stream(stream) => {
                self.emit_stream_head(id, &stream.dict, stream.content.len() as u64)?;
                self.emit(&stream.content)?;
                self.emit(b"\nendstream")?;
            }
            other => self.emit_object(other)?,
        }
        self.end_object(id, start)
    }

    /// Write a stream object whose content comes from `content`, exactly `length` bytes of it, without
    /// holding it in memory. `dict`'s `/Length` is set to `length`.
    ///
    /// # Errors
    ///
    /// As [`write_object`](Self::write_object), and
    /// [`StreamLengthMismatch`](AppendError::StreamLengthMismatch) if `content` holds fewer or more bytes
    /// than `length`, which abandons the update.
    pub fn write_stream_from<R: Read>(
        &mut self, id: crate::ObjectId, dict: &Dictionary, length: u64, content: &mut R,
    ) -> AppendResult<AppendedObject> {
        self.check_writable(id)?;
        check_dictionary(id, dict, 1)?;
        let start = self.begin_object(id)?;
        self.emit_stream_head(id, dict, length)?;
        let mut buffer = vec![0_u8; COPY_CHUNK_BYTES];
        let mut left = length;
        loop {
            let step = usize::try_from(left).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            // With nothing left to copy this reads one more byte, to prove the reader is empty too.
            let probe = if step == 0 { 1 } else { step };
            let read = match content.read(&mut buffer[..probe]) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.abandoned = true;
                    return Err(error.into());
                }
            };
            if read == 0 && left > 0 || read > 0 && left == 0 {
                self.abandoned = true;
                return Err(AppendError::StreamLengthMismatch { id, declared: length });
            }
            if read == 0 {
                break;
            }
            self.emit(&buffer[..read])?;
            left -= u64::try_from(read).unwrap_or(u64::MAX);
        }
        self.emit(b"\nendstream")?;
        self.end_object(id, start)
    }

    /// Point the new trailer's `/Info` at `info`, a reference or a direct dictionary.
    ///
    /// # Errors
    ///
    /// [`InvalidObject`](AppendError::InvalidObject) if it's neither, or has no valid PDF form.
    pub fn set_info(&mut self, info: Object) -> AppendResult<()> {
        if !matches!(info, Object::Reference(_) | Object::Dictionary(_)) {
            return Err(AppendError::InvalidObject {
                id: (0, 0),
                reason: "/Info has to be a reference or a dictionary",
            });
        }
        check_object((0, 0), &info, 0, false)?;
        self.new_info = Some(info);
        Ok(())
    }

    /// Point the new trailer's `/Root` at another object. Replacing the catalog at its own number needs
    /// no call to this.
    pub fn set_root(&mut self, root: crate::ObjectId) {
        self.new_root = Some(root);
    }

    /// Bytes the file holds so far, which is where the next object will go.
    pub fn position(&self) -> u64 {
        self.sink.position
    }

    fn check_writable(&mut self, id: crate::ObjectId) -> AppendResult<()> {
        if self.abandoned {
            return Err(AppendError::Abandoned);
        }
        if let Some(pending) = self.entries.get(&id.0) {
            if pending.placed.is_some() {
                return Err(AppendError::AlreadyWritten { id });
            }
            if pending.generation != id.1 {
                return Err(AppendError::GenerationMismatch {
                    id,
                    expected: pending.generation,
                });
            }
            return Ok(());
        }
        if id.0 == 0 {
            return Err(AppendError::UnknownObject { id });
        }
        self.check_room()?;
        let generation = match self.reader.index.locations.get(&id.0) {
            Some(ObjectLocation64::Normal { generation, .. }) => *generation,
            Some(ObjectLocation64::Compressed { .. }) => 0,
            Some(ObjectLocation64::Free { .. }) => return Err(AppendError::ObjectIsFree { id }),
            None => return Err(AppendError::UnknownObject { id }),
        };
        if generation != id.1 {
            return Err(AppendError::GenerationMismatch {
                id,
                expected: generation,
            });
        }
        let locations = &self.reader.index.locations;
        let containers = self.containers.get_or_insert_with(|| {
            locations
                .values()
                .filter_map(|location| match location {
                    ObjectLocation64::Compressed { container, .. } => Some(*container),
                    _ => None,
                })
                .collect()
        });
        if containers.contains(&id.0) {
            return Err(AppendError::ObjectStreamContainer { id });
        }
        Ok(())
    }

    fn begin_object(&mut self, id: crate::ObjectId) -> AppendResult<u64> {
        let start = self.sink.position;
        self.check_offset(start)?;
        self.emit(format!("{} {} obj\n", id.0, id.1).as_bytes())?;
        Ok(start)
    }

    fn end_object(&mut self, id: crate::ObjectId, start: u64) -> AppendResult<AppendedObject> {
        self.emit(b"\nendobj\n")?;
        let length = self.sink.position - start;
        self.entries.insert(
            id.0,
            Pending {
                generation: id.1,
                placed: Some((start, length)),
            },
        );
        Ok(AppendedObject {
            id,
            offset: start,
            length,
        })
    }

    fn check_offset(&self, offset: u64) -> AppendResult<()> {
        if self.reader.index.xref_type == IndexXrefType::Table && offset > MAX_TABLE_OFFSET {
            return Err(AppendError::OffsetTooLarge { offset });
        }
        Ok(())
    }

    fn emit(&mut self, bytes: &[u8]) -> AppendResult<()> {
        self.sink.write_all(bytes).map_err(|error| {
            self.abandoned = true;
            AppendError::Io(error)
        })
    }

    fn emit_object(&mut self, object: &Object) -> AppendResult<()> {
        Writer::write_object(&mut self.sink, object).map_err(|error| {
            self.abandoned = true;
            AppendError::Io(error)
        })
    }

    fn emit_stream_head(&mut self, id: crate::ObjectId, dict: &Dictionary, length: u64) -> AppendResult<()> {
        let length = i64::try_from(length).map_err(|_| AppendError::InvalidObject {
            id,
            reason: "its length doesn't fit a PDF integer",
        })?;
        let mut head = dict.clone();
        head.set("Length", length);
        self.emit_object(&Object::Dictionary(head))?;
        self.emit(b"\nstream\n")
    }

    /// Write the cross-reference section and the trailer, and flush.
    ///
    /// # Errors
    ///
    /// [`EmptyUpdate`](AppendError::EmptyUpdate) with no objects,
    /// [`ReservedNotWritten`](AppendError::ReservedNotWritten) if a reserved number has no object,
    /// [`TrailerTargetMissing`](AppendError::TrailerTargetMissing) if a new `/Root` or `/Info` names
    /// nothing, [`Abandoned`](AppendError::Abandoned) after an earlier failure, and any write failure.
    pub fn finish(mut self) -> AppendResult<AppendSummary> {
        if self.abandoned {
            return Err(AppendError::Abandoned);
        }
        if self.entries.is_empty() {
            return Err(AppendError::EmptyUpdate);
        }
        if let Some((number, pending)) = self.entries.iter().find(|(_, pending)| pending.placed.is_none()) {
            return Err(AppendError::ReservedNotWritten {
                id: (*number, pending.generation),
            });
        }
        let mut trailer = self.new_trailer()?;
        let xref_offset = self.sink.position;
        self.check_offset(xref_offset)?;
        match self.reader.index.xref_type {
            IndexXrefType::Table => self.emit_table(&mut trailer, xref_offset)?,
            IndexXrefType::Stream => self.emit_xref_stream(&mut trailer, xref_offset)?,
        }
        self.emit(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes())?;
        self.sink.flush().map_err(AppendError::Io)?;
        let size = trailer
            .get(b"Size")
            .and_then(Object::as_i64)
            .map_or(0, |size| u64::try_from(size).unwrap_or(0));
        let objects = self
            .entries
            .iter()
            .filter(|(number, _)| {
                // A cross-reference stream's own object is the writer's business, not the caller's.
                self.reader.index.xref_type != IndexXrefType::Stream || **number != self.next_number - 1
            })
            .filter_map(|(number, pending)| {
                let (offset, length) = pending.placed?;
                Some(AppendedObject {
                    id: (*number, pending.generation),
                    offset,
                    length,
                })
            })
            .collect();
        Ok(AppendSummary {
            previous_len: self.previous_len,
            new_len: self.sink.position,
            xref_offset,
            prev: self.reader.index.xref_start,
            size,
            objects,
        })
    }

    fn new_trailer(&mut self) -> AppendResult<Dictionary> {
        let mut trailer = Dictionary::new();
        for (key, value) in self.reader.index.trailer.iter() {
            if !TRAILER_KEYS_NOT_CARRIED.contains(&key.as_slice()) {
                trailer.set(key.clone(), value.clone());
            }
        }
        if let Some(root) = self.new_root {
            self.check_trailer_target(root)?;
            trailer.set("Root", Object::Reference(root));
        }
        if let Some(info) = self.new_info.take() {
            if let Object::Reference(id) = info {
                self.check_trailer_target(id)?;
            }
            trailer.set("Info", info);
        }
        trailer.set("Prev", i64::try_from(self.reader.index.xref_start).unwrap_or(i64::MAX));
        self.set_file_id(&mut trailer);
        Ok(trailer)
    }

    fn check_trailer_target(&self, id: crate::ObjectId) -> AppendResult<()> {
        let live = match self.entries.get(&id.0) {
            Some(pending) => pending.generation == id.1,
            None => self.reader.object_location(id).is_some(),
        };
        if live {
            Ok(())
        } else {
            Err(AppendError::TrailerTargetMissing { id })
        }
    }

    /// The first string of `/ID` is the file's permanent identifier and never changes. The second
    /// changes with each update (ISO 32000-2, 14.4), so it's taken from what this update wrote. A
    /// file without an `/ID` gets one. An `/ID` that isn't two direct strings stays as it was, rather
    /// than this writer guessing what it meant.
    fn set_file_id(&mut self, trailer: &mut Dictionary) {
        let existing = trailer.get(b"ID").ok();
        let permanent = match existing {
            None => None,
            Some(Object::Array(parts))
                if parts.len() == 2 && parts.iter().all(|part| matches!(part, Object::String(..))) =>
            {
                Some(parts[0].clone())
            }
            Some(_) => return,
        };
        let previous = existing
            .and_then(|id| id.as_array().ok())
            .and_then(|parts| parts.get(1));
        let mut hash = self.sink.hash.clone();
        hash.update(self.previous_len.to_be_bytes());
        if let Some(Object::String(bytes, _)) = previous {
            hash.update(bytes);
        }
        let digest = hash.finalize();
        let changing = Object::String(digest[..16].to_vec(), crate::StringFormat::Hexadecimal);
        let permanent = permanent.unwrap_or_else(|| changing.clone());
        trailer.set("ID", Object::Array(vec![permanent, changing]));
    }

    fn size_for(&self) -> i64 {
        let size = self.reader.index.declared_size.max(u64::from(self.next_number));
        i64::try_from(size).unwrap_or(i64::MAX)
    }

    fn emit_table(&mut self, trailer: &mut Dictionary, _xref_offset: u64) -> AppendResult<()> {
        let mut table = Vec::with_capacity(self.entries.len().saturating_mul(20).saturating_add(64));
        table.extend_from_slice(b"xref\n");
        for run in runs(&self.entries) {
            table.extend_from_slice(format!("{} {}\n", run.first, run.entries.len()).as_bytes());
            for pending in run.entries {
                let offset = pending.placed.map_or(0, |(offset, _)| offset);
                self.check_offset(offset)?;
                table.extend_from_slice(format!("{offset:010} {:05} n \n", pending.generation).as_bytes());
            }
        }
        table.extend_from_slice(b"trailer\n");
        self.emit(&table)?;
        trailer.set("Size", self.size_for());
        self.emit_object(&Object::Dictionary(trailer.clone()))?;
        self.emit(b"\n")
    }

    fn emit_xref_stream(&mut self, trailer: &mut Dictionary, xref_offset: u64) -> AppendResult<()> {
        let number = self.take_number()?;
        // The section's own entry: a reader can then find this object like any other.
        self.entries.insert(
            number,
            Pending {
                generation: 0,
                placed: Some((xref_offset, 0)),
            },
        );
        let offset_width = usize::try_from((u64::BITS - xref_offset.leading_zeros()).div_ceil(8))
            .unwrap_or(8)
            .max(1);
        let mut index = Vec::new();
        let mut content = Vec::with_capacity(self.entries.len().saturating_mul(offset_width + 3));
        for run in runs(&self.entries) {
            index.push(Object::Integer(i64::from(run.first)));
            index.push(Object::Integer(run.entries.len() as i64));
            for pending in run.entries {
                let offset = pending.placed.map_or(0, |(offset, _)| offset);
                content.push(1);
                content.extend_from_slice(&offset.to_be_bytes()[8 - offset_width..]);
                content.extend_from_slice(&pending.generation.to_be_bytes());
            }
        }
        trailer.set("Type", Object::Name(b"XRef".to_vec()));
        trailer.set("Size", self.size_for());
        trailer.set(
            "W",
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(offset_width as i64),
                Object::Integer(2),
            ]),
        );
        trailer.set("Index", Object::Array(index));
        let id = (number, 0);
        let start = self.begin_object(id)?;
        self.emit_stream_head(id, trailer, content.len() as u64)?;
        self.emit(&content)?;
        self.emit(b"\nendstream")?;
        let placed = self.end_object(id, start)?;
        debug_assert_eq!(placed.offset, xref_offset);
        Ok(())
    }
}

struct Run {
    first: u32,
    entries: Vec<Pending>,
}

/// The entries grouped into runs of consecutive numbers, which is how both forms of cross-reference
/// section list them.
fn runs(entries: &BTreeMap<u32, Pending>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (number, pending) in entries {
        match runs.last_mut() {
            Some(run) if u64::from(run.first) + run.entries.len() as u64 == u64::from(*number) => {
                run.entries.push(*pending);
            }
            _ => runs.push(Run {
                first: *number,
                entries: vec![*pending],
            }),
        }
    }
    runs
}

/// Refuse what the serializer would write as something a reader takes differently, or can't read.
fn check_object(id: crate::ObjectId, object: &Object, depth: usize, top_level: bool) -> AppendResult<()> {
    let invalid = |reason| Err(AppendError::InvalidObject { id, reason });
    if depth > MAX_OBJECT_DEPTH {
        return invalid("it nests too deeply");
    }
    match object {
        Object::Real(value) if !value.is_finite() => invalid("a number isn't finite"),
        Object::Name(name) if name.contains(&0) => invalid("a name holds a zero byte"),
        Object::Array(items) => items
            .iter()
            .try_for_each(|item| check_object(id, item, depth + 1, false)),
        Object::Dictionary(dict) => check_dictionary(id, dict, depth + 1),
        Object::Stream(stream) => {
            if !top_level {
                return invalid("a stream has to be an object of its own");
            }
            check_dictionary(id, &stream.dict, depth + 1)
        }
        _ => Ok(()),
    }
}

fn check_dictionary(id: crate::ObjectId, dict: &Dictionary, depth: usize) -> AppendResult<()> {
    for (key, value) in dict.iter() {
        if key.contains(&0) {
            return Err(AppendError::InvalidObject {
                id,
                reason: "a name holds a zero byte",
            });
        }
        check_object(id, value, depth, false)?;
    }
    Ok(())
}

/// Write an update and put the result at `target` in one step: into a temporary file beside it, synced,
/// checked with [`IndexedReader::verify_appended`], then renamed over it, so at no moment does `target`
/// hold anything partial. `target` can be the file `reader` has open, and `reader` keeps reading the old
/// bytes. Returns a reader of the new file alongside the summary.
///
/// # Errors
///
/// Anything `build` returns, any refusal, write failure or failed check. The temporary file is removed
/// and `target` isn't touched.
#[cfg(any(unix, windows))]
pub fn append_atomically<F>(
    reader: &IndexedReader, target: &std::path::Path, build: F,
) -> AppendResult<(AppendSummary, IndexedReader)>
where
    F: FnOnce(&mut AppendWriter<'_, &mut std::fs::File>) -> AppendResult<()>,
{
    reader.check_appendable()?;
    let (mut file, temporary) = TemporaryFile::create_beside(target)?;
    let summary = {
        let mut writer = AppendWriter::new(reader, &mut file)?;
        build(&mut writer)?;
        writer.finish()?
    };
    file.sync_all()?;
    drop(file);

    let written = crate::source::FileSource::open(&temporary.path)?;
    let verified = reader.verify_appended(Arc::new(written), &summary)?;
    if let Ok(existing) = std::fs::metadata(target) {
        std::fs::set_permissions(&temporary.path, existing.permissions())?;
    }
    temporary.commit(target)?;
    Ok((summary, verified))
}

#[cfg(any(unix, windows))]
struct TemporaryFile {
    path: std::path::PathBuf,
    committed: bool,
}

#[cfg(any(unix, windows))]
impl TemporaryFile {
    fn create_beside(target: &std::path::Path) -> std::io::Result<(std::fs::File, Self)> {
        let folder = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => std::path::Path::new("."),
        };
        let stem = target
            .file_name()
            .map_or_else(|| std::ffi::OsString::from("document"), std::ffi::OsStr::to_os_string);
        let process = std::process::id();
        let mut attempt = 0_u32;
        loop {
            let mut name = std::ffi::OsString::from(".");
            name.push(&stem);
            name.push(format!(".append-{process}-{attempt}.tmp"));
            let path = folder.join(name);
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok((file, Self { path, committed: false }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 1_024 => {
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn commit(mut self, target: &std::path::Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, target)?;
        self.committed = true;
        #[cfg(unix)]
        if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(any(unix, windows))]
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
