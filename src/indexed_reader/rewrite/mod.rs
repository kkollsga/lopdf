//! The streaming rewrite: a whole new file holding only what's still reachable.
//!
//! [`AppendWriter`] adds to a file, which keeps everything that was ever in it. This is the other
//! kind of save, for when old data has to really be gone: a removed page, a redaction, text that was
//! replaced. It works from an open [`IndexedReader`] and a set of [`RewriteChanges`], and doesn't load
//! the file:
//!
//! - **Reachable objects, once each.** The walk starts at the new trailer's `/Root` and `/Info` and
//!   follows references. Old revisions, the `/Prev` chain, free entries, deleted and unreferenced
//!   objects, old object streams and old cross-reference streams are never copied, because nothing
//!   reachable names them. The trailer carries `/Root`, `/Info` and `/ID` and nothing else.
//! - **Stream data is copied as its raw bytes,** 64 KiB at a time and never decoded, so memory doesn't
//!   scale with the file. Everything else is parsed and written again, so a comment inside a live
//!   object doesn't survive either. Reals are `f32` in this crate, as in PDFium.
//! - **Object numbers don't change**, so what a caller holds stays valid, and references don't need
//!   rewriting. Numbers that went out are simply missing from the cross-reference data.
//! - **One clean cross-reference section at the end.** [`RewriteFormChoice::MatchSource`], the default,
//!   writes a table if the original had only tables and no object streams, since whatever reads it
//!   today may not read anything newer, and otherwise a cross-reference stream with new object streams,
//!   since a file that had compressed its objects would otherwise grow by all of them. Past 500,000 objects a
//!   table can't be read back, so those files get the stream form whatever was asked.
//! - **Memory** is two bitsets of one bit per object number, 16 bytes for each object written, a 64 KiB
//!   output buffer, a 64 KiB copy buffer, and at most `RETAINED_CONTAINER_BYTES` of decoded object
//!   streams (always at least the one in use). Removing pages adds what that needs: the page map,
//!   the outline, and, if a `/Resources` dictionary is shared, one content stream at a time, decoded.
//!
//! # Removing a page
//!
//! A removed page's number goes on a list of dead objects. Every reference to a dead object is then
//! dropped from whatever holds it as it's written: from a dictionary, the entry goes; from a list such
//! as `/Kids`, `/Annots` or `/K`, the element goes; from any other array, it becomes `null`, keeping the
//! other positions. So no path can bring a removed page back, whatever the file's structure, and anything
//! only that page could reach isn't reached. These structures get more than that:
//!
//! - **Page tree.** `/Kids` loses the page, `/Count` is corrected up the tree, and a `/Pages` node left
//!   with nothing goes too, unless a kept page's `/Parent` chain runs through it, since its inherited
//!   attributes are what that page reads.
//! - **Annotations.** Those on a removed page go with it unless a kept page lists them too, even if
//!   `/AcroForm` or something else still pointed at them. A link on a kept page that goes to a removed
//!   page is removed; `/Popup`, `/IRT` and `/Parent` entries that named a removed annotation go.
//! - **`/Outlines`.** A bookmark to a removed page goes and its children take its place. A heading with
//!   no target of its own goes if nothing is left under it. `/Prev`, `/Next`, `/First`, `/Last`, `/Parent`
//!   and `/Count` are all corrected. With no bookmarks left, `/Outlines` goes.
//! - **`/Dests`, `/Names`.** A named destination to a removed page goes, in either the old dictionary or the
//!   name tree, with `/Limits` recalculated, since a limit is itself a name. Links and bookmarks that used that name
//!   go too. `/Names` `/Pages` and `/Templates` entries for a removed page go.
//! - **Actions.** A `/GoTo` to a removed page, in `/OpenAction`, `/AA` or any `/A`, goes.
//! - **`/AcroForm`.** A field whose widgets are all gone goes, then a parent with no kids left, and
//!   `/CO` loses them. `/XFA` is dropped, since it's a whole second copy of the form and its data. With no
//!   fields left, `/AcroForm` goes.
//! - **`/StructTreeRoot`.** An element with nothing left goes, with its `/ActualText` and `/Alt`. An
//!   element that also has content on a kept page stays, without the marked content of the removed one.
//!   `/ParentTree` entries for removed pages and annotations go, and references in the others
//!   become `null`, keeping the marked-content numbers right. `/IDTree` loses them too.
//! - **`/PageLabels`.** Rebuilt, so that every kept page keeps the label it had.
//! - **`/OCProperties`.** A layer that nothing kept uses goes from `/OCGs`, `/ON`, `/OFF`, `/Order`,
//!   `/Locked`, `/RBGroups` and `/AS`.
//! - **`/Threads`.** Beads on a removed page leave the ring, and a thread with none left goes.
//! - **`/Thumb`, page `/Metadata`, `/PieceInfo` and the like.** Only the page reaches them.
//! - **Shared `/Resources`.** A dictionary a removed page shared with kept pages, inherited or by reference,
//!   would keep the removed page's images reachable although nothing kept draws them. So the content of everything kept
//!   that uses it is searched for names, and the entries none of it can name go. That's every `/Name` in
//!   the decoded bytes, in strings and image data too, so it can only keep too much. Whatever can't be decoded
//!   leaves the whole dictionary as it was, and [`RemovalReport::resources_left_whole`] says so.
//!
//! **What stays**: anything a kept page really uses, such as a font or a form XObject on both pages, and what's
//! the document's rather than a page's: the `/Metadata` XMP, `/Info`, `/Names` `/EmbeddedFiles` and `/JavaScript`,
//! and the catalog's own `/PieceInfo`.
//!
//! # What it refuses
//!
//! [`IndexedReader::check_rewritable`] refuses before anything is written:
//!
//! - **A rebuilt index, a `/Prev` loop or an ambiguous hybrid.** What's reachable isn't known for certain there, and
//!   a wrong guess either keeps what should go or loses what should stay.
//! - **Encryption.** Every string and stream would have to be written with the file's key, or in the clear, which
//!   nobody asked for.
//!
//! And while planning:
//!
//! - **A file with signatures,** unless [`RewritePolicy::allow_breaking_signatures`] asks for it, and even then never
//!   over the signed file itself, because a signed file's bytes never change. A rewrite always breaks a signature, and
//!   [`RewriteSummary::broken_signatures`] says which. Nothing here ever says one is valid. The broken signature stays
//!   in the new file, so that a viewer says it's broken, rather than showing a drawn-on "signed by" with nothing behind it.
//! - **An object that can be reached and can't be read,** which would otherwise be lost without a word. An entry that
//!   points at nothing isn't that: it reads as `null` everywhere.
//! - **Removing every page,** or a page tree the reader refuses.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use super::append::{MAX_OBJECT_NUMBER, Sink, check_dictionary, check_object};
use super::revisions::COPY_CHUNK_BYTES;
use super::*;

mod emit;
mod plan;
mod removal;
mod resources;
mod signatures;

use self::emit::*;
use self::plan::*;

pub use self::signatures::{IndexedSignature, IndexedSignatureKind};

/// Result of a rewrite.
pub type RewriteResult<T> = std::result::Result<T, RewriteError>;

/// Why a rewrite was refused or failed. If this comes back, no finished file exists: at worst `out` holds part of
/// one, which [`rewrite_atomically`] deletes for you.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RewriteError {
    #[error("can't rewrite: this file's cross-reference data is damaged and was rebuilt by a scan")]
    RecoveredIndex,
    #[error("can't rewrite: this file is encrypted, and writing encrypted files isn't supported")]
    Encrypted,
    #[error("can't rewrite: this file's /Prev chain has a loop, so older sections were never read")]
    PrevLoop,
    #[error("can't rewrite: this file's /XRefStm can be read two ways (object {object})")]
    AmbiguousHybrid { object: u32 },
    #[error("can't rewrite: the trailer's /Root isn't a reference to a dictionary")]
    InvalidRoot,
    /// A rewritten copy of a signed file is unsigned, which needs [`RewritePolicy::remove_signatures`].
    #[error("this file has {} signature(s), and a rewritten copy would be unsigned, which wasn't asked for", signatures.len())]
    SignedNeedsConsent { signatures: Vec<IndexedSignature> },
    #[error("this file is signed, so its bytes never change: save a rewrite to a new file")]
    SignedFileCannotBeReplaced { signatures: Vec<IndexedSignature> },
    #[error("page index {index} is past the end of this document's {pages} pages")]
    PageOutOfRange { index: usize, pages: usize },
    #[error("that would remove every page")]
    NoPagesLeft,
    #[error("object {id:?} can be reached and can't be read")]
    UnreadableObject {
        id: crate::ObjectId,
        #[source]
        source: Box<IndexedReaderError>,
    },
    #[error("the file would hold {objects} objects, and a rewrite can hold at most {limit}")]
    TooManyObjects { objects: u64, limit: u64 },
    #[error("object number {number} is above the limit of {limit}")]
    ObjectNumberTooLarge { number: u64, limit: u32 },
    #[error("too much work for one rewrite: {what} is over {limit}")]
    WorkLimitExceeded { what: &'static str, limit: u64 },
    #[error("this document's {what} has more than {limit} entries")]
    StructureTooLarge { what: &'static str, limit: u64 },
    #[error("object {id:?} isn't in this file and wasn't reserved with allocate()")]
    UnknownObject { id: crate::ObjectId },
    #[error("object {id:?} is free. Use a new number from allocate()")]
    ObjectIsFree { id: crate::ObjectId },
    #[error("object {id:?} has generation {expected} in this file")]
    GenerationMismatch { id: crate::ObjectId, expected: u16 },
    #[error("object {id:?} was reserved and never written")]
    ReservedNotWritten { id: crate::ObjectId },
    #[error("object {id:?} can't be written: {reason}")]
    InvalidObject { id: crate::ObjectId, reason: &'static str },
    #[error("stream {id:?} declared {declared} bytes, and its reader gave a different number")]
    StreamLengthMismatch { id: crate::ObjectId, declared: u64 },
    #[error("the trailer would name object {id:?}, which isn't in the new file")]
    TrailerTargetMissing { id: crate::ObjectId },
    #[error("offset {offset} doesn't fit a cross-reference table's 10 digits")]
    OffsetTooLarge { offset: u64 },
    #[error("the step that prepares the replacement file failed")]
    PrepareReplacement(#[source] std::io::Error),
    #[error("the written file failed its check: {reason}")]
    VerificationFailed { reason: &'static str },
    /// Where the file was going: [`AppendError::TargetChanged`], [`AppendError::TargetExists`] and the others
    /// [`AppendDestination`] gives.
    #[error(transparent)]
    Destination(AppendError),
    #[error(transparent)]
    Reader(#[from] IndexedReaderError),
    #[error("couldn't read the original file")]
    Source(#[from] SourceError),
    #[error("couldn't write the output")]
    Io(#[from] std::io::Error),
}

impl From<AppendError> for RewriteError {
    fn from(error: AppendError) -> Self {
        match error {
            AppendError::InvalidObject { id, reason } => Self::InvalidObject { id, reason },
            AppendError::Reader(error) => Self::Reader(error),
            AppendError::Source(error) => Self::Source(error),
            AppendError::Io(error) => Self::Io(error),
            other => Self::Destination(other),
        }
    }
}

/// The form a rewritten file's cross-reference data takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RewriteForm {
    /// A classic table, with every object at the top level.
    Table,
    /// A cross-reference stream, with objects that aren't streams in new object streams.
    Stream,
}

/// Which form to write.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum RewriteFormChoice {
    /// A table for a file with only tables and no object streams, or a stream form for any other.
    #[default]
    MatchSource,
    /// This form. Past 500,000 objects a table can't be read back, so those get the stream form regardless.
    Force(RewriteForm),
}

/// What a rewrite is allowed to do, wherever it's writing to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[must_use]
pub struct RewritePolicy {
    remove_signatures: bool,
    form: RewriteFormChoice,
}

impl RewritePolicy {
    /// The defaults: a signed file is refused, and the form matches the original's.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrite a signed file into an unsigned copy, with the caller's consent. A rewrite can't keep a signature whole, so it
    /// doesn't keep one broken either: every signature goes, with what a viewer would show of it. That's each signature field, its value
    /// and its widgets with their appearances, `/Perms` (certification and usage rights), the form's `/SigFlags` and the `/DSS`
    /// security store. A signature drawn into page content isn't a field, and stays as content. [`RewriteSummary::removed_signatures`] and
    /// [`RewriteSummary::signature_removal`] say what went. For a new file: a rewrite over the signed file itself is refused whatever this says,
    /// since a signed file's bytes never change.
    pub fn remove_signatures(mut self) -> Self {
        self.remove_signatures = true;
        self
    }

    /// Choose the output's form.
    pub fn form(mut self, form: RewriteFormChoice) -> Self {
        self.form = form;
        self
    }
}

/// What removing pages took out, by kind of thing. Everything is a count of what's gone from the new file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct RemovalReport {
    /// The pages themselves, once each however often the tree listed them.
    pub pages: Vec<crate::ObjectId>,
    /// `/Pages` nodes left with nothing under them.
    pub page_tree_nodes: u64,
    /// Annotations on removed pages that no kept page lists.
    pub annotations: u64,
    /// Links on kept pages that went to a removed page.
    pub links: u64,
    /// Bookmarks removed.
    pub outline_items: u64,
    /// Bookmarks that took the place of a removed parent.
    pub outline_items_promoted: u64,
    /// Named destinations removed.
    pub named_destinations: u64,
    /// Nodes of the form's field tree removed: fields, their parents and their widgets.
    pub form_fields: u64,
    /// `/XFA` was there, and was dropped.
    pub xfa_dropped: bool,
    /// Structure elements removed.
    pub structure_elements: u64,
    /// `/ParentTree` entries removed.
    pub parent_tree_entries: u64,
    /// `/PageLabels` was rebuilt.
    pub page_labels_rebuilt: bool,
    /// Layers removed from `/OCProperties`.
    pub optional_content_groups: u64,
    /// Beads removed from article threads.
    pub thread_beads: u64,
    /// Article threads left with nothing.
    pub threads: u64,
    /// Entries taken out of `/Resources` dictionaries a removed page shared with kept pages.
    pub resource_names_pruned: u64,
    /// Shared resource dictionaries left as they were because the content using them couldn't be searched, so
    /// what a removed page used through them may still be in the file.
    pub resources_left_whole: Vec<crate::ObjectId>,
}

/// What removing signatures took out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SignatureRemoval {
    /// Signature fields and their widgets.
    pub fields: u64,
    /// `/Perms` was there: certification, usage rights or both.
    pub permissions: bool,
    /// The form's `/SigFlags` was there.
    pub signature_flags: bool,
    /// The `/DSS` security store was there.
    pub security_store: bool,
}

/// What a finished rewrite wrote.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RewriteSummary {
    /// Length of the original file.
    pub previous_len: u64,
    /// Length of the new one.
    pub new_len: u64,
    /// The form it was written in.
    pub form: RewriteForm,
    /// Its cross-reference section, which `startxref` names.
    pub xref_offset: u64,
    /// The `/Size` it states.
    pub size: u64,
    /// Objects from the original or the caller that went into the new file.
    pub objects_written: u64,
    /// How many of those are in object streams.
    pub objects_compressed: u64,
    /// New object streams.
    pub object_streams: u64,
    /// Objects the original's index lists as live that aren't in the new file. This doesn't count what only an old
    /// revision held, which also goes.
    pub objects_dropped: u64,
    /// Stream data copied without being decoded, in bytes.
    pub stream_bytes_copied: u64,
    /// What removing pages took out.
    pub removal: RemovalReport,
    /// Every signature the copy no longer has. Empty for a file with none. A signature here wasn't checked, and this
    /// doesn't say that it was ever valid.
    pub removed_signatures: Vec<IndexedSignature>,
    /// What of the signatures went, besides the signatures themselves.
    pub signature_removal: SignatureRemoval,
    /// SHA-256 of the whole new file.
    pub sha256: [u8; 32],
}

impl RewriteSummary {
    /// Bytes saved against the original, negative if the new file is larger.
    pub fn bytes_saved(&self) -> i64 {
        i64::try_from(self.previous_len)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(self.new_len).unwrap_or(i64::MAX))
    }
}

impl IndexedReader {
    /// Whether this file can be rewritten, and if not, why. This doesn't look for signatures: see
    /// [`signatures`](Self::signatures).
    ///
    /// # Errors
    ///
    /// One of the refusals listed in the [module docs](self).
    pub fn check_rewritable(&self) -> RewriteResult<()> {
        let index = &self.index;
        self.whole_file_source().validate_unchanged()?;
        if index.recovered {
            return Err(RewriteError::RecoveredIndex);
        }
        if self.is_encrypted() {
            return Err(RewriteError::Encrypted);
        }
        if index.chain.has_loop {
            return Err(RewriteError::PrevLoop);
        }
        if let Some(object) = index.chain.ambiguous_hybrid {
            return Err(RewriteError::AmbiguousHybrid { object });
        }
        let root = index
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| RewriteError::InvalidRoot)?;
        match self.resolve_object(root) {
            Ok(Object::Dictionary(_)) => Ok(()),
            Err(IndexedReaderError::Source(error)) => Err(error.into()),
            _ => Err(RewriteError::InvalidRoot),
        }
    }

    /// A reader of the same file that goes straight to the source, with no caches, since a walk over
    /// the whole file would only empty them.
    pub(super) fn uncached_view(&self) -> IndexedReader {
        IndexedReader {
            source: Arc::clone(self.whole_file_source()),
            cached_source: None,
            index: Arc::clone(&self.index),
            limits: self.limits,
            options: self.options.clone(),
            object_cache: None,
            object_stream_cache: None,
            cache_counters: None,
        }
    }
}

/// A new or replaced object, held until it's written.
pub(super) enum Change<'c> {
    Reserved,
    Object {
        generation: u16,
        object: Object,
    },
    Stream {
        generation: u16,
        dict: Dictionary,
        length: u64,
        content: Box<dyn Read + 'c>,
    },
}

/// The changes one rewrite makes, built like an [`AppendWriter`] but written all at once. New and replaced
/// objects stay in memory until then, except what's given to [`write_stream_from`](Self::write_stream_from). For a
/// file on disk, use [`rewrite_atomically`], which hands you one of these.
///
/// This has to keep failing to compile, like the same line for an [`AppendWriter`]. `File::create` would have truncated the original
/// before this line even ran, and nothing about where the output goes would have been checked.
///
/// ```compile_fail,E0277
/// # use lopdf::{IndexedReader, RewriteChanges, RewritePolicy};
/// # fn demo(reader: &IndexedReader, file: std::fs::File) {
/// let _ = RewriteChanges::new(reader).unwrap().write_unverified_to(RewritePolicy::new(), file);
/// # }
/// ```
pub struct RewriteChanges<'r, 'c> {
    pub(super) reader: &'r IndexedReader,
    pub(super) objects: BTreeMap<u32, Change<'c>>,
    pub(super) removed_pages: Vec<usize>,
    pub(super) removed_objects: Vec<u32>,
    pub(super) next_number: u32,
    pub(super) new_root: Option<crate::ObjectId>,
    pub(super) new_info: Option<Object>,
}

impl std::fmt::Debug for RewriteChanges<'_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RewriteChanges")
            .field("objects", &self.objects.len())
            .field("removed_pages", &self.removed_pages)
            .field("removed_objects", &self.removed_objects)
            .finish_non_exhaustive()
    }
}

impl<'r, 'c> RewriteChanges<'r, 'c> {
    /// Start with no changes. Writing that is a clean copy with only what's reachable.
    ///
    /// # Errors
    ///
    /// Any refusal from [`IndexedReader::check_rewritable`].
    pub fn new(reader: &'r IndexedReader) -> RewriteResult<Self> {
        reader.check_rewritable()?;
        // A reference in a kept object that names nothing is dropped, so the new numbers can't give it a meaning.
        let next_number = reader
            .index
            .locations
            .keys()
            .next_back()
            .map_or(1, |highest| u64::from(*highest) + 1);
        Ok(Self {
            reader,
            objects: BTreeMap::new(),
            removed_pages: Vec::new(),
            removed_objects: Vec::new(),
            next_number: u32::try_from(next_number).unwrap_or(u32::MAX),
            new_root: None,
            new_info: None,
        })
    }

    /// Reserve a new object number, so that objects can refer to each other before they're given. Every number
    /// reserved has to be written before the rewrite is.
    ///
    /// # Errors
    ///
    /// [`ObjectNumberTooLarge`](RewriteError::ObjectNumberTooLarge) past what PDFium would read.
    pub fn allocate(&mut self) -> RewriteResult<crate::ObjectId> {
        let number = self.next_number;
        if number > MAX_OBJECT_NUMBER {
            return Err(RewriteError::ObjectNumberTooLarge {
                number: u64::from(number),
                limit: MAX_OBJECT_NUMBER,
            });
        }
        self.next_number = number + 1;
        self.objects.insert(number, Change::Reserved);
        Ok((number, 0))
    }

    /// Add a new object under a new number.
    ///
    /// # Errors
    ///
    /// As [`allocate`](Self::allocate) and [`write_object`](Self::write_object).
    pub fn add_object(&mut self, object: Object) -> RewriteResult<crate::ObjectId> {
        let id = self.allocate()?;
        self.write_object(id, object)?;
        Ok(id)
    }

    /// Give an object: one reserved with [`allocate`](Self::allocate), or a new version of a live object at its own
    /// generation, which the old one is then gone in favour of. Doing it twice, the last one counts. A stream's `/Length` is set to its content's length.
    ///
    /// # Errors
    ///
    /// [`UnknownObject`](RewriteError::UnknownObject), [`ObjectIsFree`](RewriteError::ObjectIsFree) or
    /// [`GenerationMismatch`](RewriteError::GenerationMismatch) if the number can't take it, and
    /// [`InvalidObject`](RewriteError::InvalidObject) if the object has no valid PDF form.
    pub fn write_object(&mut self, id: crate::ObjectId, object: Object) -> RewriteResult<()> {
        let generation = self.check_writable(id)?;
        check_object(id, &object, 0, true)?;
        self.objects.insert(id.0, Change::Object { generation, object });
        Ok(())
    }

    /// Give a stream object whose content comes from `content`, exactly `length` bytes of it, read when
    /// the file is written and never held in memory. Its bytes go in as they are, so `dict` has to say how they're encoded.
    ///
    /// # Errors
    ///
    /// As [`write_object`](Self::write_object). If `content` holds fewer or more bytes than `length`, the
    /// rewrite fails with [`StreamLengthMismatch`](RewriteError::StreamLengthMismatch) when it's written.
    pub fn write_stream_from(
        &mut self, id: crate::ObjectId, dict: Dictionary, length: u64, content: impl Read + 'c,
    ) -> RewriteResult<()> {
        let generation = self.check_writable(id)?;
        check_dictionary(id, &dict, 1)?;
        self.objects.insert(
            id.0,
            Change::Stream {
                generation,
                dict,
                length,
                content: Box::new(content),
            },
        );
        Ok(())
    }

    /// Remove a page by its position in [`IndexedReader::page_map`], and everything only it can reach. See the
    /// [module docs](self) for what that covers. Positions are the original's, however many are removed. A page object the tree lists more
    /// than once goes from all of those places.
    ///
    /// # Errors
    ///
    /// None here: [`PageOutOfRange`](RewriteError::PageOutOfRange) and [`NoPagesLeft`](RewriteError::NoPagesLeft) come
    /// when the rewrite is written, when all of them are known.
    pub fn remove_page(&mut self, index: usize) {
        self.removed_pages.push(index);
    }

    /// Remove an object, however it's reached: every reference to it is dropped as described in the
    /// [module docs](self). For a page use [`remove_page`](Self::remove_page), which also deals with what depends on it.
    pub fn remove_object(&mut self, id: crate::ObjectId) {
        self.removed_objects.push(id.0);
    }

    /// Point the new trailer's `/Info` at `info`, a reference or a direct dictionary.
    ///
    /// # Errors
    ///
    /// [`InvalidObject`](RewriteError::InvalidObject) if it's neither, or has no valid PDF form.
    pub fn set_info(&mut self, info: Object) -> RewriteResult<()> {
        if !matches!(info, Object::Reference(_) | Object::Dictionary(_)) {
            return Err(RewriteError::InvalidObject {
                id: (0, 0),
                reason: "/Info has to be a reference or a dictionary",
            });
        }
        check_object((0, 0), &info, 1, false)?;
        self.new_info = Some(info);
        Ok(())
    }

    /// Point the new trailer's `/Root` at another object. Replacing the catalog at its own number needs no call to this.
    pub fn set_root(&mut self, root: crate::ObjectId) {
        self.new_root = Some(root);
    }

    fn check_writable(&self, id: crate::ObjectId) -> RewriteResult<u16> {
        if let Some(change) = self.objects.get(&id.0) {
            let generation = match change {
                Change::Reserved => 0,
                Change::Object { generation, .. } | Change::Stream { generation, .. } => *generation,
            };
            if generation != id.1 {
                return Err(RewriteError::GenerationMismatch {
                    id,
                    expected: generation,
                });
            }
            return Ok(generation);
        }
        let generation = match self.reader.index.locations.get(&id.0) {
            Some(ObjectLocation64::Normal { generation, .. }) => *generation,
            Some(ObjectLocation64::Compressed { .. }) => 0,
            Some(ObjectLocation64::Free { .. }) => return Err(RewriteError::ObjectIsFree { id }),
            None => return Err(RewriteError::UnknownObject { id }),
        };
        if generation != id.1 {
            return Err(RewriteError::GenerationMismatch {
                id,
                expected: generation,
            });
        }
        Ok(generation)
    }

    /// Write the new file to memory or nowhere, unchecked. For a file on disk, use [`rewrite_atomically`], which checks it before anything is
    /// replaced. What this gives back says in its type that nothing has read the result yet: [`UnverifiedRewrite::verify`] does that, from
    /// the bytes written. Writing to [`std::io::sink`] gives the summary, so a caller can show what a save would do before doing it.
    ///
    /// # Errors
    ///
    /// Any refusal described in the [module docs](self), and any read or write failure.
    pub fn write_unverified_to<W: AppendOutput>(self, policy: RewritePolicy, out: W) -> RewriteResult<UnverifiedRewrite> {
        let reader = self.reader;
        let plan = Plan::prepare(self, policy, false)?;
        let (summary, written) = write_plan(reader, plan, out)?;
        Ok(UnverifiedRewrite { summary, written })
    }
}

/// A rewrite that has been written and not yet read back. Use what it wrote only after [`verify`](Self::verify).
#[must_use]
pub struct UnverifiedRewrite {
    summary: RewriteSummary,
    written: Written,
}

impl std::fmt::Debug for UnverifiedRewrite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnverifiedRewrite")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

impl UnverifiedRewrite {
    /// What was written, as the writer says.
    pub fn summary(&self) -> &RewriteSummary {
        &self.summary
    }

    /// Check what was written, read back from `written`, as [`rewrite_atomically`] does before it replaces anything: every byte by hash,
    /// one cross-reference section with nothing missing or dead, the pages there should be, and no signature in a copy that was to have
    /// none. `original` is the reader the rewrite came from. Gives back the summary and a reader of the new file.
    ///
    /// # Errors
    ///
    /// [`VerificationFailed`](RewriteError::VerificationFailed) saying what didn't match, or a reader error.
    pub fn verify(
        self, original: &IndexedReader, written: Arc<dyn RandomAccessSource>,
    ) -> RewriteResult<(RewriteSummary, IndexedReader)> {
        let reader = original.verify_rewritten(written, &self.summary, &self.written)?;
        Ok((self.summary, reader))
    }
}

/// How [`rewrite_atomically`] treats what it replaces, and what it may do. As with [`AppendOptions`], a replaced file is
/// a new file: it gets the old one's name and mode bits, and nothing else unless the caller carries it over in
/// [`prepare_replacement`](Self::prepare_replacement).
#[cfg(any(unix, windows))]
#[derive(Default)]
#[must_use]
pub struct RewriteOptions<'h> {
    policy: RewritePolicy,
    prepare: Option<PrepareReplacement<'h>>,
    allow_breaking_hard_links: bool,
}

#[cfg(any(unix, windows))]
type PrepareReplacement<'h> = Box<dyn FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()> + 'h>;

#[cfg(any(unix, windows))]
impl<'h> RewriteOptions<'h> {
    /// The defaults: a signed file is refused, a target with other hard links is refused, and there's no extra step.
    pub fn new() -> Self {
        Self::default()
    }

    /// What the rewrite may do, including breaking signatures.
    pub fn policy(mut self, policy: RewritePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// As [`AppendOptions::prepare_replacement`]: a step that runs on the finished temporary file before it's
    /// checked and moved into place. The check after it reads every byte again, so a step that damages the file fails the save.
    pub fn prepare_replacement(
        mut self, step: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()> + 'h,
    ) -> Self {
        self.prepare = Some(Box::new(step));
        self
    }

    /// Replace a file that has other hard links. Those names keep the old bytes from then on, which for a rewrite includes
    /// whatever it was meant to remove, and this one gets the new file.
    pub fn allow_breaking_hard_links(mut self) -> Self {
        self.allow_breaking_hard_links = true;
        self
    }
}

#[cfg(any(unix, windows))]
impl std::fmt::Debug for RewriteOptions<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RewriteOptions")
            .field("policy", &self.policy)
            .field("prepare_replacement", &self.prepare.is_some())
            .field("allow_breaking_hard_links", &self.allow_breaking_hard_links)
            .finish()
    }
}

/// A save that happened. The new file is in place even if `durability_warning` is set.
#[cfg(any(unix, windows))]
#[non_exhaustive]
pub struct RewrittenFile {
    /// What the rewrite wrote.
    pub summary: RewriteSummary,
    /// A reader of the new file, which is the one to use from now on. The old reader keeps reading the old bytes.
    pub reader: IndexedReader,
    /// The file was replaced, but syncing its folder failed, so a power cut in the next moments could bring the old
    /// name back. For a rewrite that means what was removed could come back too, so say so.
    pub durability_warning: Option<std::io::Error>,
}

#[cfg(any(unix, windows))]
impl std::fmt::Debug for RewrittenFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RewrittenFile")
            .field("summary", &self.summary)
            .field("durability_warning", &self.durability_warning)
            .finish_non_exhaustive()
    }
}

/// The steps of [`rewrite_atomically`], which a test can stop at.
#[cfg(any(unix, windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RewriteStep {
    TemporaryCreated,
    Written,
    Synced,
    Prepared,
    PermissionsSet,
    Verified,
    AboutToCommit,
    Committed,
}

/// Write a rewrite and put the result at a destination in one step: into a temporary file beside it, synced, checked, then
/// moved into place, so at no moment does the path hold anything partial, and a crash at any step leaves the original as it was.
/// `reader` keeps reading the old bytes.
///
/// # Errors
///
/// Anything `build` returns, any refusal, write failure or failed check. From the destination,
/// [`Destination`](RewriteError::Destination) with what [`AppendDestination`] gives. [`SignedFileCannotBeReplaced`](RewriteError::SignedFileCannotBeReplaced)
/// if the destination is the signed file itself, whatever the policy. [`PrepareReplacement`](RewriteError::PrepareReplacement) if that step fails.
/// The temporary file is removed and the destination isn't touched. Once the file is in place, the result is never an error: see
/// [`RewrittenFile::durability_warning`].
#[cfg(any(unix, windows))]
pub fn rewrite_atomically<'c, F>(
    reader: &IndexedReader, destination: AppendDestination<'_>, options: RewriteOptions<'_>, build: F,
) -> RewriteResult<RewrittenFile>
where
    F: FnOnce(&mut RewriteChanges<'_, 'c>) -> RewriteResult<()>,
{
    rewrite_atomically_with_steps(reader, destination, options, build, |_| Ok(()))
}

#[cfg(any(unix, windows))]
pub(super) fn rewrite_atomically_with_steps<'c, F>(
    reader: &IndexedReader, destination: AppendDestination<'_>, options: RewriteOptions<'_>, build: F,
    mut step: impl FnMut(RewriteStep) -> std::io::Result<()>,
) -> RewriteResult<RewrittenFile>
where
    F: FnOnce(&mut RewriteChanges<'_, 'c>) -> RewriteResult<()>,
{
    reader.check_rewritable()?;
    let seen = destination.check(reader)?;
    let target = destination.real_path()?;
    let target = target.as_path();
    super::append::check_replaceable(target, options.allow_breaking_hard_links)?;
    let replaces_original = is_original(reader, &destination, seen.as_ref());
    let mut changes = RewriteChanges::new(reader)?;
    build(&mut changes)?;
    let plan = Plan::prepare(changes, options.policy, replaces_original)?;

    let permissions = super::append::final_permissions(reader, target);
    let (mut file, temporary) = super::append::TemporaryFile::create_beside(target, permissions.as_ref())?;
    step(RewriteStep::TemporaryCreated)?;
    let (summary, written) = write_plan(reader, plan, TemporaryOutput { file: &mut file })?;
    step(RewriteStep::Written)?;
    file.sync_all()?;
    drop(file);
    step(RewriteStep::Synced)?;
    if let Some(prepare) = options.prepare {
        prepare(&temporary.path, target).map_err(RewriteError::PrepareReplacement)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temporary.path)?
            .sync_all()?;
    }
    step(RewriteStep::Prepared)?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&temporary.path, permissions)?;
        #[cfg(unix)]
        std::fs::File::open(&temporary.path)?.sync_all()?;
    }
    step(RewriteStep::PermissionsSet)?;

    let source = crate::source::FileSource::open(&temporary.path)?;
    let verified = reader.verify_rewritten(Arc::new(source), &summary, &written)?;
    step(RewriteStep::Verified)?;
    destination.check_again(reader, seen.as_ref())?;
    super::append::check_replaceable(target, options.allow_breaking_hard_links)?;
    step(RewriteStep::AboutToCommit)?;
    let durability_warning = match destination {
        AppendDestination::NewFile(_) => temporary.commit_new(target)?,
        _ => temporary.commit(target)?,
    };
    step(RewriteStep::Committed).ok();
    Ok(RewrittenFile {
        summary,
        reader: verified,
        durability_warning,
    })
}

/// Whether this destination puts the new file over the file the reader has open, whatever it was called.
#[cfg(any(unix, windows))]
fn is_original(reader: &IndexedReader, destination: &AppendDestination<'_>, seen: Option<&std::fs::Metadata>) -> bool {
    match destination {
        AppendDestination::SameFile(_) => true,
        _ => seen.is_some_and(|metadata| reader.whole_file_source().is_same_file(metadata) == Some(true)),
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests;
