//! A file's revisions and the bytes each one ends at.
//!
//! An incremental update leaves every earlier byte alone, so revision `n` of a file is
//! exactly its first `end` bytes. That is what lets a later feature say what a signature
//! covers, and hand back an earlier version byte for byte.
//!
//! Nothing here is computed at open. [`IndexedReader::revisions`] walks the `/Prev` chain
//! again when asked, under the same bounds, and keeps nothing afterwards.
//!
//! **A range is proved or absent.** A section's revision ends at the `startxref` that follows
//! it and names it. A section with no such marker, or a chain that runs backwards through the
//! file, has no range: hostile files do both, and a guessed range would claim bytes belong to
//! a revision that a reader of that prefix wouldn't see. Two shapes are one revision made of
//! two sections. A linearized file's first-page section comes first in the file, and the
//! `startxref` after its main section names it. And some writers close two sections, found
//! together, with one `startxref`, which names the newer.

use std::io::Write;
use std::ops::Range;

use super::*;

/// Bytes read past a section looking for its `startxref` and `%%EOF`.
const END_MARKER_WINDOW: u64 = 4 * 1_024;

/// Room after `startxref` for a number of 20 digits, the blanks around it, and `%%EOF`.
const END_MARKER_MAX_TAIL: u64 = 64;

/// Bytes of one copy step when extracting a revision.
pub(super) const COPY_CHUNK_BYTES: usize = 64 * 1_024;

/// What the append writer puts after a file whose last byte isn't a line ending. An object header has to
/// start a line, but a bare line ending here would look like the old revision's own, and the old revision
/// would then come back one byte longer than it was. The comment says the line ending is new. Nothing
/// depends on trusting it: at worst a revision's end moves by that one line ending, and `eof_marker_end`
/// doesn't move at all.
pub(super) const ADDED_LINE_ENDING: &[u8] = b"\n%line-ending-added\n";

/// The form of the cross-reference data that opens a revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexedXrefKind {
    /// A classic `xref` table.
    Table,
    /// A cross-reference stream.
    Stream,
    /// A classic table with a `/XRefStm` supplement.
    Hybrid,
}

/// One revision of a file. Offsets count bytes from the start of the file.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedRevision {
    /// Position in the chain: 0 is the oldest.
    pub index: usize,
    /// The bytes this revision added, and through `range.end` the whole of the file as it was
    /// then. `None` when no `startxref` proves where it ends.
    ///
    /// The end is past the line ending after `%%EOF`, if there is one. For a file this writer appended
    /// to, that's exactly the original. If another tool added the line ending when it appended, nothing
    /// in the file says so, and the two possible ends are this and `eof_marker_end`.
    pub range: Option<Range<u64>>,
    /// Just past this revision's `%%EOF`, before any line ending. A signature's `/ByteRange` can
    /// end here or at `range.end`, depending on what signed it.
    pub eof_marker_end: Option<u64>,
    /// The offset a reader starts this revision from, as its `startxref` says.
    pub startxref: u64,
    /// The form of that cross-reference section.
    pub xref_kind: IndexedXrefKind,
    /// The `/Prev` that leads to the revision before this one.
    pub prev: Option<u64>,
    /// Entries in this revision's own sections, not counting a hybrid supplement.
    pub entry_count: u64,
    /// Whether this revision is a linearized file's first-page and main sections together.
    pub linearized: bool,
}

/// Every revision a `/Prev` walk finds, oldest first.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedRevisionChain {
    /// Oldest first.
    pub revisions: Vec<IndexedRevision>,
    /// A `/Prev` led back to a section already read. There may be older revisions the walk can't
    /// reach.
    pub has_loop: bool,
    /// Bytes after the newest revision's end. These belong to no revision.
    pub trailing_bytes: Option<u64>,
}

impl IndexedRevisionChain {
    /// Whether every revision has a proved range and the chain has no loop.
    pub fn is_regular(&self) -> bool {
        !self.has_loop && self.revisions.iter().all(|revision| revision.range.is_some())
    }

    /// The revision that ends exactly at `offset`, the end of a signature's `/ByteRange`, for
    /// instance. A range that ends anywhere else isn't a whole revision and gets `None`.
    pub fn revision_ending_at(&self, offset: u64) -> Option<&IndexedRevision> {
        self.revisions.iter().find(|revision| {
            revision.range.as_ref().is_some_and(|range| range.end == offset) || revision.eof_marker_end == Some(offset)
        })
    }
}

struct SectionFacts {
    offset: u64,
    kind: IndexedXrefKind,
    prev: Option<u64>,
    entry_count: u64,
    /// The last place this section needs a reader to reach: itself, or its `/XRefStm`.
    reach: u64,
    marker: Option<EndMarker>,
}

#[derive(Clone, Copy)]
struct EndMarker {
    /// The value after `startxref`.
    names: u64,
    eof_marker_end: u64,
    end: u64,
}

impl IndexedReader {
    /// List this file's revisions, oldest first.
    ///
    /// Walks the `/Prev` chain again, under the bounds the reader was opened with, and reads one
    /// small window past each section. Keeps nothing afterwards.
    ///
    /// # Errors
    ///
    /// [`RevisionsUnavailable`](IndexedReaderError::RevisionsUnavailable) if the index came from a
    /// body scan, which has no chain to walk.
    pub fn revisions(&self) -> IndexedReaderResult<IndexedRevisionChain> {
        if self.index.recovered {
            return Err(IndexedReaderError::RevisionsUnavailable);
        }
        let source = self.whole_file_source();
        let origin = self.index.source_origin;
        let source_len = self.index.source_len;
        let mut budget = XrefWalkBudget::new(self.options.max_xref_sections, source_len);
        let mut seen = HashSet::new();
        let mut sections = Vec::new();
        let mut next = Some(self.index.xref_start);
        let mut has_loop = false;
        while let Some(offset) = next {
            if !seen.insert(offset) {
                has_loop = true;
                break;
            }
            budget.charge_section()?;
            let physical = origin
                .checked_add(offset)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let section = read_xref_section(source.as_ref(), source_len, physical, &mut budget)?;
            let prev = trailer_offset(&section.trailer, b"Prev", "Prev")?;
            let kind = match (section.kind, section.trailer.has(b"XRefStm")) {
                (IndexXrefType::Stream, _) => IndexedXrefKind::Stream,
                (IndexXrefType::Table, true) => IndexedXrefKind::Hybrid,
                (IndexXrefType::Table, false) => IndexedXrefKind::Table,
            };
            let after = physical
                .checked_add(section.length)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let supplement = trailer_offset(&section.trailer, b"XRefStm", "XRefStm")?;
            sections.push(SectionFacts {
                offset,
                kind,
                prev,
                entry_count: u64::try_from(section.entries.len()).unwrap_or(u64::MAX),
                reach: offset.max(supplement.unwrap_or(0)),
                marker: read_end_marker(source.as_ref(), source_len, after)?,
            });
            next = prev;
        }

        let (mut revisions, mut reaches) = group_sections(&sections);
        revisions.reverse();
        reaches.reverse();
        settle_ranges(&mut revisions, &reaches, origin);
        let trailing_bytes = revisions
            .last()
            .and_then(|newest| newest.range.as_ref())
            .and_then(|range| source_len.checked_sub(range.end));
        Ok(IndexedRevisionChain {
            revisions,
            has_loop,
            trailing_bytes,
        })
    }

    /// Write a revision's bytes, `0..range.end`, to `out`, streaming them in small pieces.
    /// Returns the bytes written.
    ///
    /// # Errors
    ///
    /// [`RevisionRangeUnavailable`](IndexedReaderError::RevisionRangeUnavailable) if the revision has
    /// no proved range. An I/O failure from `out` comes back as a source I/O error.
    pub fn copy_revision_to<W: Write>(&self, revision: &IndexedRevision, out: &mut W) -> IndexedReaderResult<u64> {
        let range = revision
            .range
            .as_ref()
            .ok_or(IndexedReaderError::RevisionRangeUnavailable { index: revision.index })?;
        copy_prefix(self.whole_file_source().as_ref(), range.end, out).map_err(|error| match error {
            CopyError::Source(error) => IndexedReaderError::Source(error),
            CopyError::Sink(error) => IndexedReaderError::Source(SourceError::Io(error)),
        })?;
        Ok(range.end)
    }

    /// Open a revision as the file it once was, without copying it. The result reads through this
    /// reader's source.
    ///
    /// # Errors
    ///
    /// [`RevisionRangeUnavailable`](IndexedReaderError::RevisionRangeUnavailable) if the revision has
    /// no proved range, or if those bytes open only by a rescan. Otherwise, whatever opening them as a file gives.
    pub fn open_revision(
        &self, revision: &IndexedRevision, options: IndexedReaderOptions,
    ) -> IndexedReaderResult<IndexedReader> {
        let range = revision
            .range
            .as_ref()
            .ok_or(IndexedReaderError::RevisionRangeUnavailable { index: revision.index })?;
        let prefix = PrefixSource {
            inner: Arc::clone(self.whole_file_source()),
            len: range.end,
        };
        let reader = Self::open_shared(Arc::new(prefix), options)?;
        // A rescan would answer with some reading of those bytes, which isn't the file as it was.
        if reader.index.recovered {
            return Err(IndexedReaderError::RevisionRangeUnavailable { index: revision.index });
        }
        Ok(reader)
    }

    /// The source for a pass over much of the file. It skips the reader's chunk cache, which that
    /// pass would otherwise empty for no gain.
    pub(super) fn whole_file_source(&self) -> &Arc<dyn RandomAccessSource> {
        match &self.cached_source {
            Some(cached) => cached.backing(),
            None => &self.source,
        }
    }
}

/// Newest first, as the walk found them, with how far into the file each revision's sections reach.
fn group_sections(sections: &[SectionFacts]) -> (Vec<IndexedRevision>, Vec<u64>) {
    let mut revisions = Vec::with_capacity(sections.len());
    let mut reaches = Vec::with_capacity(sections.len());
    let mut position = 0;
    while let Some(section) = sections.get(position) {
        let linearized_tail = sections.get(position + 1).filter(|tail| {
            section.marker.is_none_or(|marker| marker.names != section.offset)
                && section.prev == Some(tail.offset)
                && tail.offset > section.offset
                && tail.marker.is_some_and(|marker| marker.names == section.offset)
        });
        let marker = match linearized_tail {
            Some(tail) => tail.marker,
            None => section.marker.filter(|marker| marker.names == section.offset),
        };
        // Sections written together and closed by one marker: the older ones have no marker of their
        // own, so the first one after them is the one that names this section.
        let mut together = 0;
        let mut absorbed_entries = 0_u64;
        if linearized_tail.is_none() && marker.is_some() {
            let mut newer = section;
            while let Some(older) = sections.get(position + 1 + together) {
                let closed_by_this_marker = newer.prev == Some(older.offset)
                    && older.offset < newer.offset
                    && older.marker.is_some_and(|marker| marker.names == section.offset);
                if !closed_by_this_marker {
                    break;
                }
                absorbed_entries = absorbed_entries.saturating_add(older.entry_count);
                together += 1;
                newer = older;
            }
        }
        revisions.push(IndexedRevision {
            index: 0,
            // Settled later, when the revision before it is known. Until then this holds its end.
            range: marker.map(|marker| marker.end..marker.end),
            eof_marker_end: marker.map(|marker| marker.eof_marker_end),
            startxref: section.offset,
            xref_kind: section.kind,
            prev: linearized_tail
                .or_else(|| sections.get(position + together))
                .map_or(section.prev, |oldest| oldest.prev),
            entry_count: section
                .entry_count
                .saturating_add(absorbed_entries)
                .saturating_add(linearized_tail.map_or(0, |tail| tail.entry_count)),
            linearized: linearized_tail.is_some(),
        });
        let used = 1 + together + usize::from(linearized_tail.is_some());
        reaches.push(
            sections[position..position + used]
                .iter()
                .map(|used| used.reach)
                .max()
                .unwrap_or(0),
        );
        position += used;
    }
    (revisions, reaches)
}

/// Number the revisions and give each its start, the end of the one before. A revision has no range if its
/// section isn't inside the bytes it would add, or if anything down its chain, a `/Prev` or a `/XRefStm`, is
/// past its end: a reader of that prefix wouldn't find them, and a signature report must never call such a
/// prefix whole. Only a linearized file's first revision starts its section early.
fn settle_ranges(revisions: &mut [IndexedRevision], reaches: &[u64], origin: u64) {
    let mut last_end = 0_u64;
    let mut chain_reach = 0_u64;
    for (index, revision) in revisions.iter_mut().enumerate() {
        revision.index = index;
        let end = revision.range.as_ref().map(|range| range.end);
        let section = origin.saturating_add(revision.startxref);
        chain_reach = chain_reach.max(reaches.get(index).copied().unwrap_or(u64::MAX));
        let reached = origin.saturating_add(chain_reach);
        let provable = end.is_some_and(|end| {
            let after_previous = index == 0 || last_end <= section;
            reached < end && end > last_end && section < end && after_previous && (!revision.linearized || index == 0)
        });
        if provable {
            let end = end.unwrap_or(last_end);
            revision.range = Some(last_end..end);
            last_end = end;
        } else {
            revision.range = None;
            revision.eof_marker_end = None;
        }
    }
}

fn read_end_marker(
    source: &dyn RandomAccessSource, source_len: u64, after: u64,
) -> IndexedReaderResult<Option<EndMarker>> {
    let Some(remaining) = source_len.checked_sub(after) else {
        return Ok(None);
    };
    // `startxref` is looked for in the window. Past that, what follows a match has to fit too: its number, `%%EOF` and an
    // added line ending, or a marker near the window's edge would come out one byte too long.
    let limit = END_MARKER_WINDOW + END_MARKER_MAX_TAIL + ADDED_LINE_ENDING.len() as u64;
    let window = source.read_range(after, remaining.min(limit), limit)?;
    let search = window.len().min(END_MARKER_WINDOW as usize);
    let Some(keyword) = find(&window[..search], b"startxref") else {
        return Ok(None);
    };
    let tail = &window[keyword + b"startxref".len()..];
    let mut cursor = TokenCursor::new(tail);
    let Some(names) = cursor.unsigned() else {
        return Ok(None);
    };
    // Not `skip_space`, which would take `%%EOF` for a comment.
    let rest = cursor.remaining();
    let blank = rest.iter().take_while(|byte| is_pdf_whitespace(**byte)).count();
    if !rest[blank..].starts_with(b"%%EOF") {
        return Ok(None);
    }
    let marker_start = keyword + b"startxref".len() + (tail.len() - rest.len()) + blank;
    let marker_end = marker_start + b"%%EOF".len();
    let eol = match (window.get(marker_end), window.get(marker_end + 1)) {
        _ if window[marker_end..].starts_with(ADDED_LINE_ENDING) => 0,
        (Some(b'\r'), Some(b'\n')) => 2,
        (Some(b'\r' | b'\n'), _) => 1,
        _ => 0,
    };
    let at = |position: usize| after.saturating_add(u64::try_from(position).unwrap_or(u64::MAX));
    Ok(Some(EndMarker {
        names,
        eof_marker_end: at(marker_end),
        end: at(marker_end + eol),
    }))
}

fn find(input: &[u8], pattern: &[u8]) -> Option<usize> {
    input.windows(pattern.len()).position(|window| window == pattern)
}

pub(super) enum CopyError {
    Source(SourceError),
    Sink(std::io::Error),
}

/// Stream the source's first `length` bytes to `out`, one small buffer at a time.
pub(super) fn copy_prefix<W: Write>(
    source: &dyn RandomAccessSource, length: u64, out: &mut W,
) -> std::result::Result<(), CopyError> {
    let mut buffer = vec![0_u8; COPY_CHUNK_BYTES];
    let mut position = 0_u64;
    while position < length {
        let step = usize::try_from(length - position).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
        source
            .read_exact_at(position, &mut buffer[..step])
            .map_err(CopyError::Source)?;
        out.write_all(&buffer[..step]).map_err(CopyError::Sink)?;
        position += u64::try_from(step).unwrap_or(u64::MAX);
    }
    Ok(())
}

/// The first `len` bytes of another source, as a file of that length.
struct PrefixSource {
    inner: Arc<dyn RandomAccessSource>,
    len: u64,
}

impl RandomAccessSource for PrefixSource {
    fn len(&self) -> crate::source::SourceResult<u64> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> crate::source::SourceResult<usize> {
        let Some(left) = self.len.checked_sub(offset) else {
            return Err(SourceError::OutOfBounds {
                offset,
                length: 0,
                source_len: self.len,
            });
        };
        let step = usize::try_from(left).map_or(out.len(), |left| left.min(out.len()));
        self.inner.read_at(offset, &mut out[..step])
    }

    fn validate_unchanged(&self) -> crate::source::SourceResult<()> {
        self.inner.validate_unchanged()
    }
}
