//! What's in the new file: loading objects through the changes, dropping what refers to something gone, and the walk.

use super::removal::PageRemoval;
use super::resources::Pruner;
use super::*;

/// Decoded object streams kept between members. One is kept whatever its size, since the one in use has to be somewhere.
pub(super) const RETAINED_CONTAINER_BYTES: usize = 4 * 1_024 * 1_024;
/// Bytes one rewrite may parse and inflate at the least, however small the file is. Above that it's a multiple of the file,
/// which an honest file doesn't reach in the few passes a rewrite makes.
const WORK_ALLOWANCE: u64 = 2 * 1_024 * 1_024 * 1_024;
const WORK_PER_FILE_BYTE: u64 = 16;
/// Objects in one new file. Every section is read back with the reader's limit of a million entries.
pub(super) const MAX_REWRITE_OBJECTS: u64 = 900_000;
/// How long a chain of references a load follows.
const DEREFERENCE_LIMIT: usize = 32;

/// Arrays that are lists: one thing going leaves the others where they are, with no gap. In any other array a position
/// means something, so a reference that goes becomes `null`.
const LIST_KEYS: [&[u8]; 14] = [
    b"Kids",
    b"Annots",
    b"K",
    b"Fields",
    b"CO",
    b"OCGs",
    b"ON",
    b"OFF",
    b"Locked",
    b"Order",
    b"RBGroups",
    b"Threads",
    b"B",
    b"Ref",
];

/// One bit per object number.
pub(super) struct Bits {
    words: Vec<u64>,
    capacity: u32,
    count: u64,
}

impl Bits {
    pub(super) fn new(capacity: u32) -> Self {
        Self {
            words: vec![0; (capacity as usize).div_ceil(64)],
            capacity,
            count: 0,
        }
    }

    pub(super) fn get(&self, number: u32) -> bool {
        number < self.capacity && self.words[(number / 64) as usize] & (1 << (number % 64)) != 0
    }

    /// Whether it wasn't set before. A number past the capacity is never set.
    pub(super) fn set(&mut self, number: u32) -> bool {
        if number >= self.capacity || self.get(number) {
            return false;
        }
        self.words[(number / 64) as usize] |= 1 << (number % 64);
        self.count += 1;
        true
    }

    pub(super) fn count(&self) -> u64 {
        self.count
    }

    pub(super) fn clear(&mut self) {
        self.words.fill(0);
        self.count = 0;
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(position, word)| {
            let mut word = *word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros();
                word &= word - 1;
                Some(position as u32 * 64 + bit)
            })
        })
    }
}

/// Where a stream's bytes are.
#[derive(Clone, Copy)]
pub(super) enum StreamBody {
    /// In the original file. Never read here.
    Source { start: u64, length: u64 },
    /// Held by the caller's change.
    Given,
}

/// An object with its stream's bytes left where they are.
#[derive(Clone)]
pub(super) enum Loaded {
    Plain(Object),
    Stream(Dictionary, StreamBody),
}

impl Loaded {
    pub(super) fn dictionary(&self) -> Option<&Dictionary> {
        match self {
            Self::Plain(Object::Dictionary(dict)) | Self::Stream(dict, _) => Some(dict),
            _ => None,
        }
    }
}

struct RetainedContainer {
    number: u32,
    dictionary: Dictionary,
    decoded: Vec<u8>,
}

/// Decoded object streams in use, oldest first.
#[derive(Default)]
struct Containers {
    retained: VecDeque<RetainedContainer>,
    bytes: usize,
}

pub(super) struct Plan<'c> {
    pub(super) view: IndexedReader,
    pub(super) objects: BTreeMap<u32, Change<'c>>,
    pub(super) policy: RewritePolicy,
    pub(super) root: u32,
    pub(super) root_generation: u16,
    pub(super) info: Option<Object>,
    pub(super) next_number: u32,
    /// Whatever refers to one of these loses the reference.
    pub(super) dead: Bits,
    /// What goes in the new file.
    pub(super) live: Bits,
    seen: Bits,
    /// Objects the plan changed, taking the place of what the file or the caller says.
    pub(super) edits: HashMap<u32, Loaded>,
    /// Indirect arrays that hold a list, found at the key that names them.
    list_arrays: HashSet<u32>,
    /// Destination names that went, which links and bookmarks may still use.
    pub(super) dead_names: HashSet<Vec<u8>>,
    pub(super) report: RemovalReport,
    pub(super) signatures: Vec<IndexedSignature>,
    pub(super) expected_pages: Option<usize>,
    containers: Containers,
    work: u64,
    work_limit: u64,
}

/// Whether what a rewrite would break is allowed to break. The bytes of a signed file never change, whatever is asked for.
fn check_signatures(signatures: &[IndexedSignature], policy: RewritePolicy, replaces_original: bool) -> RewriteResult<()> {
    if signatures.is_empty() {
        return Ok(());
    }
    if replaces_original {
        return Err(RewriteError::SignedFileCannotBeReplaced {
            signatures: signatures.to_vec(),
        });
    }
    if !policy.allow_breaking_signatures {
        return Err(RewriteError::SignaturesWouldBreak {
            signatures: signatures.to_vec(),
        });
    }
    Ok(())
}

impl<'c> Plan<'c> {
    /// Everything up to the first byte written: what goes, what stays and whether that's allowed.
    pub(super) fn prepare(changes: RewriteChanges<'_, 'c>, policy: RewritePolicy, replaces_original: bool) -> RewriteResult<Self> {
        let RewriteChanges {
            reader,
            objects,
            removed_pages,
            removed_objects,
            next_number,
            new_root,
            new_info,
        } = changes;
        if let Some((number, _)) = objects.iter().find(|(_, change)| matches!(change, Change::Reserved)) {
            return Err(RewriteError::ReservedNotWritten { id: (*number, 0) });
        }
        let signatures = reader.signatures()?;
        check_signatures(&signatures, policy, replaces_original)?;
        let (root, root_generation) = match new_root {
            Some(root) => root,
            None => reader
                .index
                .trailer
                .get(b"Root")
                .and_then(Object::as_reference)
                .map_err(|_| RewriteError::InvalidRoot)?,
        };
        let info = new_info.or_else(|| reader.index.trailer.get(b"Info").ok().cloned());
        let capacity = next_number.min(MAX_OBJECT_NUMBER).saturating_add(1);
        let identity = objects.is_empty() && removed_pages.is_empty() && removed_objects.is_empty();
        let mut plan = Self {
            view: reader.uncached_view(),
            objects,
            policy,
            root,
            root_generation,
            info,
            next_number,
            dead: Bits::new(capacity),
            live: Bits::new(capacity),
            seen: Bits::new(capacity),
            edits: HashMap::new(),
            list_arrays: HashSet::new(),
            dead_names: HashSet::new(),
            report: RemovalReport::default(),
            signatures,
            expected_pages: None,
            containers: Containers::default(),
            work: 0,
            work_limit: reader.index.source_len.saturating_mul(WORK_PER_FILE_BYTE).max(WORK_ALLOWANCE),
        };
        if plan.load(root)?.is_none_or(|object| object.dictionary().is_none()) {
            return Err(RewriteError::InvalidRoot);
        }
        for number in removed_objects {
            plan.dead.set(number);
        }
        let mut removal = None;
        if !removed_pages.is_empty() {
            removal = Some(plan.plan_page_removal(&removed_pages)?);
            if plan.objects.is_empty() {
                plan.expected_pages = removal.as_ref().map(PageRemoval::kept_count);
            }
        } else if identity {
            plan.expected_pages = reader.page_count().ok();
        }
        plan.walk(removal)?;
        if plan.live.count() > MAX_REWRITE_OBJECTS {
            return Err(RewriteError::TooManyObjects {
                objects: plan.live.count(),
                limit: MAX_REWRITE_OBJECTS,
            });
        }
        if !plan.live.get(root) {
            return Err(RewriteError::TrailerTargetMissing {
                id: (root, root_generation),
            });
        }
        check_signatures(&plan.signatures, policy, replaces_original)?;
        Ok(plan)
    }

    pub(super) fn charge(&mut self, bytes: u64) -> RewriteResult<()> {
        self.work = self.work.saturating_add(bytes);
        if self.work > self.work_limit {
            return Err(RewriteError::WorkLimitExceeded {
                what: "bytes parsed and inflated",
                limit: self.work_limit,
            });
        }
        Ok(())
    }

    pub(super) fn generation(&self, number: u32) -> u16 {
        match self.objects.get(&number) {
            Some(Change::Object { generation, .. } | Change::Stream { generation, .. }) => *generation,
            Some(Change::Reserved) => 0,
            None => match self.view.index.locations.get(&number) {
                Some(ObjectLocation64::Normal { generation, .. }) => *generation,
                _ => 0,
            },
        }
    }

    /// Whether anything is there to refer to. A stale entry that points at nothing is found out when it's loaded.
    fn exists(&self, number: u32) -> bool {
        self.objects.contains_key(&number)
            || matches!(
                self.view.index.locations.get(&number),
                Some(ObjectLocation64::Normal { .. } | ObjectLocation64::Compressed { .. })
            )
    }

    /// Whether a reference to this has to go.
    pub(super) fn gone(&self, number: u32) -> bool {
        self.dead.get(number) || !self.exists(number)
    }

    /// An object as the plan so far has it. `None` if nothing is there, which reads as `null`.
    ///
    /// # Errors
    ///
    /// [`UnreadableObject`](RewriteError::UnreadableObject) if it's there and can't be read.
    pub(super) fn load(&mut self, number: u32) -> RewriteResult<Option<Loaded>> {
        if let Some(edit) = self.edits.get(&number) {
            return Ok(Some(edit.clone()));
        }
        match self.objects.get(&number) {
            Some(Change::Object {
                object: Object::Stream(stream),
                ..
            }) => {
                return Ok(Some(Loaded::Stream(stream.dict.clone(), StreamBody::Given)));
            }
            Some(Change::Object { object, .. }) => return Ok(Some(Loaded::Plain(object.clone()))),
            Some(Change::Stream { dict, .. }) => return Ok(Some(Loaded::Stream(dict.clone(), StreamBody::Given))),
            Some(Change::Reserved) => return Ok(None),
            None => {}
        }
        self.read_source(number)
    }

    /// As [`load`](Self::load), for looking at a structure rather than copying it: what can't be read isn't there.
    /// The walk will still refuse it, if it stays reachable.
    pub(super) fn peek(&mut self, number: u32) -> RewriteResult<Option<Loaded>> {
        match self.load(number) {
            Err(RewriteError::UnreadableObject { .. }) => Ok(None),
            other => other,
        }
    }

    pub(super) fn peek_dict(&mut self, number: u32) -> RewriteResult<Option<Dictionary>> {
        Ok(match self.peek(number)? {
            Some(Loaded::Plain(Object::Dictionary(dict)) | Loaded::Stream(dict, _)) => Some(dict),
            _ => None,
        })
    }

    /// A value with references followed, for looking at it.
    pub(super) fn deref(&mut self, value: &Object) -> RewriteResult<Option<Object>> {
        let Object::Reference((number, _)) = value else {
            return Ok(Some(value.clone()));
        };
        let mut number = *number;
        for _ in 0..DEREFERENCE_LIMIT {
            match self.peek(number)? {
                Some(Loaded::Plain(Object::Reference((next, _)))) => number = next,
                Some(Loaded::Plain(object)) => return Ok(Some(object)),
                Some(Loaded::Stream(dict, _)) => return Ok(Some(Object::Dictionary(dict))),
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    pub(super) fn deref_dict(&mut self, value: Option<&Object>) -> RewriteResult<Option<Dictionary>> {
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(match self.deref(value)? {
            Some(Object::Dictionary(dict)) => Some(dict),
            _ => None,
        })
    }

    pub(super) fn deref_array(&mut self, value: Option<&Object>) -> RewriteResult<Vec<Object>> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };
        Ok(match self.deref(value)? {
            Some(Object::Array(items)) => items,
            _ => Vec::new(),
        })
    }

    fn read_source(&mut self, number: u32) -> RewriteResult<Option<Loaded>> {
        match self.view.index.locations.get(&number).cloned() {
            Some(ObjectLocation64::Normal { generation, .. }) => self.read_normal((number, generation)),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.read_member((number, 0), container, index).map(Some)
            }
            Some(ObjectLocation64::Free { .. }) | None => Ok(None),
        }
    }

    fn read_normal(&mut self, id: crate::ObjectId) -> RewriteResult<Option<Loaded>> {
        let unreadable = |error: IndexedReaderError| match error {
            IndexedReaderError::Source(error) => RewriteError::Source(error),
            other => RewriteError::UnreadableObject {
                id,
                source: Box::new(other),
            },
        };
        let (body_offset, source_len, parsed) = match self.view.resolve_normal_framed(id) {
            Ok(framed) => framed,
            // The entry names nothing. Everyone reads that as null.
            Err(IndexedReaderError::MissingNormalObject { .. } | IndexedReaderError::MissingNormalObjectAtXref { .. }) => {
                return Ok(None);
            }
            Err(error) => return Err(unreadable(error)),
        };
        self.charge(u64::try_from(parsed.consumed).unwrap_or(u64::MAX))?;
        let Some(prefix) = parsed.stream_prefix else {
            return Ok(Some(Loaded::Plain(parsed.object)));
        };
        let stream_start = body_offset
            .checked_add(u64::try_from(parsed.consumed).unwrap_or(u64::MAX))
            .and_then(|offset| offset.checked_add(prefix))
            .ok_or_else(|| unreadable(IndexedReaderError::InvalidIndirectObject { id, offset: body_offset }))?;
        let fallback = parsed.object.as_dict().cloned().ok();
        let mut state = ResolutionState::default();
        let metadata = self.view.finish_stream_metadata(
            id,
            body_offset,
            source_len,
            parsed,
            &mut state,
            StreamSpanPolicy::Encoded(self.view.limits.max_encoded_stream_bytes),
        );
        match metadata {
            Ok(FramedStreamMetadata::Span {
                dictionary,
                encoded_start,
                encoded_len,
            }) => Ok(Some(Loaded::Stream(
                dictionary,
                StreamBody::Source {
                    start: encoded_start,
                    length: encoded_len,
                },
            ))),
            Err(StreamMetadataError::EncodedLimit { id, length, limit }) => {
                Err(unreadable(IndexedReaderError::StreamLimitExceeded { id, length, limit }))
            }
            Err(error) => {
                let error = error.into_reader_error();
                match (&error, fallback) {
                    (
                        IndexedReaderError::MissingEndstream { .. } | IndexedReaderError::NegativeStreamLength { .. },
                        Some(dictionary),
                    ) => self.stream_by_scanning(id, dictionary, stream_start, source_len).map(Some),
                    _ => Err(unreadable(error)),
                }
            }
            // A wrong /Length. PDFium looks for `endstream`, so what it draws comes from there, and the new file has to hold the same bytes.
            Ok(FramedStreamMetadata::MissingLength { dictionary, .. } | FramedStreamMetadata::Scalar(Object::Dictionary(dictionary))) => {
                self.stream_by_scanning(id, dictionary, stream_start, source_len).map(Some)
            }
            Ok(FramedStreamMetadata::Scalar(object)) => Ok(Some(Loaded::Plain(object))),
        }
    }

    /// Find a stream's end the way a viewer does when `/Length` is wrong, reading in pieces. Each piece is charged, so a
    /// file full of such streams can't make this quadratic.
    fn stream_by_scanning(
        &mut self, id: crate::ObjectId, dictionary: Dictionary, start: u64, source_len: u64,
    ) -> RewriteResult<Loaded> {
        const MARKER: &[u8] = b"endstream";
        let mut buffer = vec![0_u8; COPY_CHUNK_BYTES];
        let mut position = start;
        let mut found = None;
        while position < source_len && found.is_none() {
            let step = usize::try_from(source_len - position).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            self.view.source.read_exact_at(position, &mut buffer[..step])?;
            self.charge(step as u64)?;
            found = buffer[..step]
                .windows(MARKER.len())
                .position(|window| window == MARKER)
                .map(|at| position + at as u64);
            if step < COPY_CHUNK_BYTES {
                break;
            }
            position += (COPY_CHUNK_BYTES - MARKER.len()) as u64;
        }
        let Some(end) = found else {
            return Err(RewriteError::UnreadableObject {
                id,
                source: Box::new(IndexedReaderError::MissingEndstream { id }),
            });
        };
        // The line ending before `endstream` isn't data.
        let mut length = end - start;
        if length > 0 {
            let mut tail = [0_u8; 2];
            let probe = length.min(2) as usize;
            self.view.source.read_exact_at(end - probe as u64, &mut tail[..probe])?;
            if tail[..probe].ends_with(b"\r\n") {
                length -= 2;
            } else if tail[..probe].ends_with(b"\n") || tail[..probe].ends_with(b"\r") {
                length -= 1;
            }
        }
        Ok(Loaded::Stream(dictionary, StreamBody::Source { start, length }))
    }

    fn read_member(&mut self, id: crate::ObjectId, container: u32, index: u32) -> RewriteResult<Loaded> {
        let unreadable = |source: IndexedReaderError| match source {
            IndexedReaderError::Source(error) => RewriteError::Source(error),
            source => RewriteError::UnreadableObject {
                id,
                source: Box::new(source),
            },
        };
        let container_id = (container, 0);
        if !self.containers.retained.iter().any(|entry| entry.number == container) {
            let mut state = ResolutionState::default();
            let object = self.view.resolve_normal(container_id, &mut state).map_err(unreadable)?;
            let Object::Stream(stream) = object else {
                return Err(unreadable(IndexedReaderError::ObjectStreamContainerNotStream {
                    id,
                    container: container_id,
                }));
            };
            let limit = usize::try_from(self.view.limits.max_stream_bytes).unwrap_or(usize::MAX);
            let decoded = ObjectStream::decode_selected_member_source(&stream, Some(limit))
                .map_err(|source| {
                    unreadable(IndexedReaderError::ObjectStreamMember {
                        id,
                        container: container_id,
                        index,
                        source,
                    })
                })?
                .into_owned();
            self.charge((stream.content.len() + decoded.len()) as u64)?;
            self.containers.bytes += decoded.len();
            self.containers.retained.push_back(RetainedContainer {
                number: container,
                dictionary: stream.dict,
                decoded,
            });
            while self.containers.bytes > RETAINED_CONTAINER_BYTES && self.containers.retained.len() > 1 {
                if let Some(evicted) = self.containers.retained.pop_front() {
                    self.containers.bytes -= evicted.decoded.len();
                }
            }
        }
        let entry = self
            .containers
            .retained
            .iter()
            .find(|entry| entry.number == container)
            .expect("the container was just made sure of");
        let object = ObjectStream::parse_selected_member_from_decoded(&entry.dictionary, &entry.decoded, id, index)
            .map_err(|source| {
                unreadable(IndexedReaderError::ObjectStreamMember {
                    id,
                    container: container_id,
                    index,
                    source,
                })
            })?;
        Ok(Loaded::Plain(object))
    }

    /// An object as it'll be written: with no `/Length`, which is stated when it's written, and nothing that refers to
    /// something gone. `None` if it means nothing any more.
    pub(super) fn load_for_output(&mut self, number: u32) -> RewriteResult<Option<Loaded>> {
        let Some(mut loaded) = self.load(number)? else {
            return Ok(None);
        };
        match &mut loaded {
            Loaded::Stream(dict, _) => {
                // An old object stream holds its neighbours as they were, and a cross-reference stream old offsets. Whatever refers to
                // one, copying it would bring all of that back.
                if dict.has_type(b"ObjStm") || dict.has_type(b"XRef") {
                    return Ok(None);
                }
                dict.remove(b"Length");
                self.scrub_dict(dict);
            }
            Loaded::Plain(Object::Dictionary(dict)) => {
                if !self.scrub_dict(dict) {
                    return Ok(None);
                }
            }
            Loaded::Plain(Object::Array(items)) => {
                if self.list_arrays.contains(&number) {
                    self.scrub_list(b"", items, false);
                } else if self.is_gone_destination(items) {
                    return Ok(None);
                } else {
                    self.scrub_array(items);
                }
            }
            Loaded::Plain(Object::Reference((target, _))) => {
                if self.gone(*target) {
                    return Ok(None);
                }
            }
            Loaded::Plain(_) => {}
        }
        Ok(Some(loaded))
    }

    fn is_gone_destination(&self, items: &[Object]) -> bool {
        matches!(items, [Object::Reference((target, _)), Object::Name(_), ..] if self.gone(*target))
    }

    /// Drop everything in a dictionary that refers to something gone. False if what's left means nothing.
    pub(super) fn scrub_dict(&self, dict: &mut Dictionary) -> bool {
        let is_gone_ref = |value: &Object| matches!(value, Object::Reference((target, _)) if self.gone(*target));
        let page_gone = dict.get(b"Pg").is_ok_and(is_gone_ref);
        let object_gone = dict.get(b"Obj").is_ok_and(is_gone_ref);
        let is_content_reference = dict.has_type(b"MCR") || dict.has_type(b"OBJR");
        if is_content_reference && (page_gone || object_gone) {
            return false;
        }
        let is_goto = matches!(dict.get(b"S"), Ok(Object::Name(name)) if name == b"GoTo");
        let held_target = is_goto && dict.has(b"D");
        let dead_names = &self.dead_names;
        let names_gone = |value: &Object| match value {
            Object::String(name, _) | Object::Name(name) => dead_names.contains(name),
            _ => false,
        };
        let name_target_gone = !dead_names.is_empty();
        dict.as_hashmap_mut().retain(|key, value| {
            if name_target_gone && (key == b"Dest" || (is_goto && key == b"D")) && names_gone(value) {
                return false;
            }
            self.scrub_entry(key, value, page_gone)
        });
        // A jump with nowhere to go. The rest of its chain, if any, has no way of being run without it.
        !(held_target && !dict.has(b"D"))
    }

    fn scrub_entry(&self, key: &[u8], value: &mut Object, page_gone: bool) -> bool {
        match value {
            Object::Reference((target, _)) => !self.gone(*target),
            Object::Integer(_) => !(page_gone && key == b"K"),
            Object::Array(items) => {
                if LIST_KEYS.contains(&key) {
                    self.scrub_list(key, items, page_gone);
                    true
                } else if self.is_gone_destination(items) {
                    false
                } else {
                    self.scrub_array(items);
                    true
                }
            }
            Object::Dictionary(inner) => {
                let is_structure_kid = key == b"K";
                if is_structure_kid && !self.structure_kid_stays(inner, page_gone) {
                    return false;
                }
                self.scrub_dict(inner)
            }
            _ => true,
        }
    }

    fn structure_kid_stays(&self, kid: &Dictionary, page_gone: bool) -> bool {
        !(page_gone && (kid.has_type(b"MCR") || kid.has_type(b"OBJR")) && !kid.has(b"Pg"))
    }

    fn scrub_array(&self, items: &mut [Object]) {
        for item in items {
            match item {
                Object::Reference((target, _)) if self.gone(*target) => *item = Object::Null,
                Object::Array(inner) => {
                    if self.is_gone_destination(inner) {
                        *item = Object::Null;
                    } else {
                        self.scrub_array(inner);
                    }
                }
                Object::Dictionary(inner) => {
                    if !self.scrub_dict(inner) {
                        *item = Object::Null;
                    }
                }
                _ => {}
            }
        }
    }

    fn scrub_list(&self, key: &[u8], items: &mut Vec<Object>, page_gone: bool) {
        items.retain_mut(|item| match item {
            Object::Reference((target, _)) => !self.gone(*target),
            Object::Integer(_) => !(page_gone && key == b"K"),
            Object::Array(inner) => {
                self.scrub_list(key, inner, page_gone);
                true
            }
            Object::Dictionary(inner) => {
                if key == b"K" && !self.structure_kid_stays(inner, page_gone) {
                    return false;
                }
                let had_kids = key == b"K" && inner.has(b"K");
                let link_with_target = key == b"Annots"
                    && matches!(inner.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Link")
                    && (inner.has(b"Dest") || inner.has(b"A"));
                if !self.scrub_dict(inner) {
                    return false;
                }
                // A link with nowhere to go. One that's an object of its own was dealt with before this.
                if link_with_target && !inner.has(b"Dest") && !inner.has(b"A") {
                    return false;
                }
                // A direct structure element with nothing left, which nothing else can remove since nothing can name it.
                let lost_kids = had_kids
                    && match inner.get(b"K") {
                        Ok(Object::Array(kids)) => kids.is_empty(),
                        Ok(_) => false,
                        Err(_) => true,
                    };
                !lost_kids
            }
            _ => true,
        });
    }

    fn note_list_arrays(&mut self, dict: &Dictionary) {
        for (key, value) in dict.iter() {
            if let Object::Reference((target, _)) = value
                && LIST_KEYS.contains(&key.as_slice())
            {
                self.list_arrays.insert(*target);
            }
        }
    }

    fn enqueue_references(&mut self, object: &Object, next: &mut Vec<u32>) -> RewriteResult<()> {
        match object {
            Object::Reference((number, _)) => self.enqueue(*number, next)?,
            Object::Array(items) => {
                for item in items {
                    self.enqueue_references(item, next)?;
                }
            }
            Object::Dictionary(dict) => {
                self.note_signature_dictionary(None, dict);
                self.note_list_arrays(dict);
                for (_, value) in dict.iter() {
                    self.enqueue_references(value, next)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn enqueue(&mut self, number: u32, next: &mut Vec<u32>) -> RewriteResult<()> {
        if number > MAX_OBJECT_NUMBER {
            return Err(RewriteError::ObjectNumberTooLarge {
                number: u64::from(number),
                limit: MAX_OBJECT_NUMBER,
            });
        }
        if self.seen.set(number) {
            next.push(number);
        }
        Ok(())
    }

    /// Follow references from what's queued. Each round goes in the order of the file's object streams, so a stream is decoded
    /// as few times as can be however far apart the references are.
    fn reach(&mut self, mut frontier: Vec<u32>, mut pruner: Option<&mut Pruner>, hold_back: Option<&[u8]>) -> RewriteResult<()> {
        while !frontier.is_empty() {
            let locations = &self.view.index.locations;
            frontier.sort_by_cached_key(|number| match locations.get(number) {
                Some(ObjectLocation64::Compressed { container, .. }) => (*container, *number),
                _ => (u32::MAX, *number),
            });
            let mut next = Vec::new();
            for number in frontier {
                let Some(mut loaded) = self.load_for_output(number)? else {
                    // Whatever refers to this has to lose the reference, since it won't be there.
                    self.dead.set(number);
                    continue;
                };
                self.live.set(number);
                if number == self.root
                    && let (Some(key), Loaded::Plain(Object::Dictionary(dict))) = (hold_back, &mut loaded)
                {
                    dict.remove(key);
                }
                if let Some(pruner) = pruner.as_deref_mut()
                    && let Some(dict) = loaded.dictionary()
                {
                    pruner.note_reached(number, dict);
                }
                match loaded {
                    Loaded::Plain(object) => self.enqueue_top_level(number, object, &mut next)?,
                    Loaded::Stream(dict, _) => {
                        self.enqueue_top_level(number, Object::Dictionary(dict), &mut next)?;
                    }
                }
            }
            frontier = next;
        }
        Ok(())
    }

    fn enqueue_top_level(&mut self, number: u32, object: Object, next: &mut Vec<u32>) -> RewriteResult<()> {
        if let Object::Dictionary(dict) = &object {
            self.note_signature_dictionary(Some(number), dict);
        }
        self.enqueue_references(&object, next)
    }

    fn roots(&mut self) -> RewriteResult<Vec<u32>> {
        self.seen.clear();
        self.live.clear();
        let mut roots = Vec::new();
        self.enqueue(self.root, &mut roots)?;
        if let Some(mut info) = self.info.take() {
            match &mut info {
                Object::Reference((number, _)) => {
                    let number = *number;
                    if !self.gone(number) {
                        self.enqueue(number, &mut roots)?;
                    }
                }
                Object::Dictionary(dict) => {
                    self.scrub_dict(dict);
                    let value = Object::Dictionary(dict.clone());
                    self.enqueue_references(&value, &mut roots)?;
                }
                _ => {}
            }
            self.info = Some(info);
        }
        Ok(roots)
    }

    /// Settle what goes in the file. Without pages removed that's one walk. With them, what's reachable decides which layers go,
    /// and shared resources can go through more than one walk: each can only find more that's used, so it settles.
    fn walk(&mut self, removal: Option<PageRemoval>) -> RewriteResult<()> {
        let Some(removal) = removal else {
            let roots = self.roots()?;
            return self.reach(roots, None, None);
        };
        const LAYERS: &[u8] = b"OCProperties";
        let mut pruner = self.start_pruner(&removal)?;
        // Anything in the shared dictionaries that kept content uses, then whatever else turns up using them.
        const MAX_ROUNDS: usize = 4;
        let mut rounds = 0;
        loop {
            self.apply_pruner(&mut pruner)?;
            let roots = self.roots()?;
            self.reach(roots, Some(&mut pruner), Some(LAYERS))?;
            if !self.scan_new_users(&mut pruner)? {
                break;
            }
            rounds += 1;
            if rounds >= MAX_ROUNDS {
                pruner.give_up();
                self.apply_pruner(&mut pruner)?;
                let roots = self.roots()?;
                self.reach(roots, None, Some(LAYERS))?;
                break;
            }
        }
        self.finish_pruner(pruner);

        // A layer nothing kept can reach goes. Only now is it known what's reachable without the list of layers itself.
        let catalog = self.peek_dict(self.root)?;
        let layers = catalog.as_ref().and_then(|catalog| catalog.get(LAYERS).ok()).cloned();
        if let Some(layers) = layers {
            let dict = self.deref_dict(Some(&layers))?;
            for group in self.deref_array(dict.as_ref().and_then(|dict| dict.get(b"OCGs").ok()))? {
                if let Object::Reference((number, _)) = group
                    && !self.live.get(number)
                    && self.dead.set(number)
                {
                    self.report.optional_content_groups += 1;
                }
            }
            let mut value = layers;
            match &mut value {
                Object::Dictionary(dict) => {
                    self.scrub_dict(dict);
                }
                Object::Reference((number, _)) if self.gone(*number) => value = Object::Null,
                _ => {}
            }
            let mut next = Vec::new();
            self.enqueue_references(&value, &mut next)?;
            self.reach(next, None, None)?;
        }
        Ok(())
    }
}
