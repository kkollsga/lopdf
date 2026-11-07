//! Shared `/Resources`: taking out what only a removed page used.
//!
//! A kept page and a removed page can share one resource dictionary, by inheriting it or by reference, and some
//! producers put every image in the file in one. Everything in it stays reachable from the kept page, drawn or not, so
//! reachability alone would keep the removed page's images.
//!
//! So every name the kept content could use is found by searching its decoded bytes, and the other entries go. The search takes
//! every `/Name` in the bytes, wherever it is, with no attempt to tell a string or an inline image from an operator. That can
//! only find too many, which keeps too much, and never too few, which would break a kept page. Anything that can't be decoded
//! gives up on that dictionary, and it's reported. Forms, patterns, Type 3 fonts and soft masks with no `/Resources` of their
//! own are searched under the dictionary of whatever uses them, as viewers fall back to it.

use std::collections::BTreeSet;

use super::*;

/// The most content one rewrite decodes to search for names, at the least. Past it what's shared stays whole.
const SCAN_ALLOWANCE: u64 = 4 * 1_024 * 1_024 * 1_024;
const SCAN_PER_FILE_BYTE: u64 = 32;
/// Holders of shared names one rewrite keeps track of. Past it the rest stay whole.
const MAX_SHARED_HOLDERS: usize = 4_096;
/// Resource categories whose names content can use.
const CATEGORIES: [&[u8]; 7] = [
    b"XObject",
    b"ExtGState",
    b"Font",
    b"Pattern",
    b"Shading",
    b"ColorSpace",
    b"Properties",
];
/// Names a viewer looks up by itself, which no content has to mention.
const NAMES_LOOKED_UP_IMPLICITLY: [&[u8]; 3] = [b"DefaultGray", b"DefaultRGB", b"DefaultCMYK"];

/// A dictionary of resource names, and how to get to it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum Holder {
    /// An object that's one category's dictionary, such as `/XObject 9 0 R`.
    Category(u32),
    /// `/Category << ... >>` directly in this object, which is a resource dictionary.
    InResources(u32, usize),
    /// `/Resources << /Category << ... >> >>` directly in this object, a page or a `/Pages` node.
    InOwner(u32, usize),
}

impl Holder {
    fn object(self) -> u32 {
        match self {
            Self::Category(number) | Self::InResources(number, _) | Self::InOwner(number, _) => number,
        }
    }

    fn names(self, base: &Dictionary) -> Option<&Dictionary> {
        match self {
            Self::Category(_) => Some(base),
            Self::InResources(_, category) => base.get(CATEGORIES[category]).and_then(Object::as_dict).ok(),
            Self::InOwner(_, category) => base
                .get(b"Resources")
                .and_then(Object::as_dict)
                .ok()?
                .get(CATEGORIES[category])
                .and_then(Object::as_dict)
                .ok(),
        }
    }

    fn names_mut(self, base: &mut Dictionary) -> Option<&mut Dictionary> {
        match self {
            Self::Category(_) => Some(base),
            Self::InResources(_, category) => base.get_mut(CATEGORIES[category]).and_then(Object::as_dict_mut).ok(),
            Self::InOwner(_, category) => base
                .get_mut(b"Resources")
                .and_then(Object::as_dict_mut)
                .ok()?
                .get_mut(CATEGORIES[category])
                .and_then(Object::as_dict_mut)
                .ok(),
        }
    }
}

/// What removed pages used, gathered before anything is known about who else uses it.
#[derive(Default)]
pub(super) struct SharedResources {
    holders: BTreeSet<Holder>,
    /// Each kept page, and the object its resources came from.
    kept_pages: Vec<(u32, Option<u32>)>,
}

impl SharedResources {
    pub(super) fn remember_kept_pages(&mut self, kept: &[&PageMapEntry]) {
        self.kept_pages = kept
            .iter()
            .map(|entry| (entry.id.0, entry.inherited.resources.map(|owner| owner.0)))
            .collect();
    }
}

struct HolderState {
    /// As it was, from which what's kept is worked out again each time.
    used: HashSet<Vec<u8>>,
    /// Something kept uses it, and that was searched.
    has_user: bool,
    /// Something kept uses it, and that couldn't be searched.
    whole: bool,
}

/// One content stream to search, and the holders whose names it can use.
struct Search {
    stream: u32,
    scope: Rc<Vec<Holder>>,
}

/// The state of the search.
pub(super) struct Pruner {
    holders: BTreeMap<Holder, HolderState>,
    /// The objects holding holders, as they were before any pruning.
    bases: BTreeMap<u32, Loaded>,
    searched: HashSet<(u32, Vec<Holder>)>,
    pending: Vec<Search>,
    /// Objects the walk reached that have `/Resources`, to look at next.
    found: BTreeSet<u32>,
    /// `/DR` anywhere, or `/Resources` inside something that isn't an object of its own, with the object it's in. None of that says what it uses,
    /// so what it holds has to stay.
    opaque: Vec<(u32, Object)>,
    decoded: u64,
    decoded_limit: u64,
}

impl Pruner {
    /// A dictionary the walk reached. If it has `/Resources` in a shared holder, it uses them, which is looked at after the walk.
    pub(super) fn note_reached(&mut self, number: u32, dict: &Dictionary) {
        if self.holders.is_empty() {
            return;
        }
        if dict.has(b"Resources") && !dict.has_type(b"Pages") {
            self.found.insert(number);
        }
        self.note_opaque(number, dict, true);
    }

    fn note_opaque(&mut self, number: u32, dict: &Dictionary, top_level: bool) {
        if let Ok(default_resources) = dict.get(b"DR") {
            // A form's fields use these by a name in a string, /DA, which no content search can see.
            self.opaque.push((number, default_resources.clone()));
        }
        if !top_level && let Ok(resources) = dict.get(b"Resources") {
            self.opaque.push((number, resources.clone()));
        }
        for (_, value) in dict.iter() {
            self.note_opaque_value(number, value);
        }
    }

    fn note_opaque_value(&mut self, number: u32, value: &Object) {
        match value {
            Object::Dictionary(dict) => self.note_opaque(number, dict, false),
            Object::Array(items) => {
                for item in items {
                    self.note_opaque_value(number, item);
                }
            }
            _ => {}
        }
    }

    pub(super) fn give_up(&mut self) {
        for holder in self.holders.values_mut() {
            holder.whole = true;
        }
    }
}

/// Call `found` with every `/Name` anywhere in these bytes, with `#xx` decoded. Nothing about the bytes around one matters, on purpose.
pub(super) fn every_name(bytes: &[u8], mut found: impl FnMut(&[u8])) {
    let is_end = |byte: u8| {
        matches!(
            byte,
            0 | b'\t'
                | b'\n'
                | 0x0C
                | b'\r'
                | b' '
                | b'('
                | b')'
                | b'<'
                | b'>'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b'/'
                | b'%'
        )
    };
    let mut decoded = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'/' {
            at += 1;
            continue;
        }
        at += 1;
        let start = at;
        while at < bytes.len() && !is_end(bytes[at]) {
            at += 1;
        }
        let raw = &bytes[start..at];
        if raw.contains(&b'#') {
            decoded.clear();
            let mut position = 0;
            while position < raw.len() {
                let hex = (raw[position] == b'#')
                    .then(|| raw.get(position + 1..position + 3))
                    .flatten()
                    .and_then(|digits| std::str::from_utf8(digits).ok())
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok());
                if let Some(byte) = hex {
                    decoded.push(byte);
                    position += 3;
                } else {
                    decoded.push(raw[position]);
                    position += 1;
                }
            }
            found(&decoded);
        }
        // The bytes as they are too, in case a `#` was what the name really held.
        found(raw);
    }
}

/// A stream's decoded bytes, all of them or none. The general decoder gives what it can of a damaged stream, which is right for showing
/// it and wrong here: names past the damage wouldn't be seen, a viewer might get further, and a page would lose what it uses.
fn decode_all_or_nothing(dict: Dictionary, content: Vec<u8>, limit: usize) -> Option<Vec<u8>> {
    if !dict.has(b"Filter") {
        return Some(content);
    }
    let flate = |name: &[u8]| name == b"FlateDecode" || name == b"Fl";
    let filters: Vec<Vec<u8>> = Stream::new(dict.clone(), Vec::new())
        .filters()
        .ok()?
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    if filters.iter().any(|filter| flate(filter)) && dict.has(b"DecodeParms") {
        // Predictors aren't something content streams use. Whatever this is, it isn't looked into.
        return None;
    }
    let mut bytes = content;
    for filter in &filters {
        bytes = if flate(filter) {
            inflate_whole(&bytes, limit)?
        } else {
            let mut stage = Dictionary::new();
            stage.set("Filter", Object::Name(filter.clone()));
            if let Ok(parameters) = dict.get(b"DecodeParms")
                && filters.len() == 1
            {
                stage.set("DecodeParms", parameters.clone());
            }
            Stream::new(stage, bytes).decompressed_content_with_limit(limit).ok()?
        };
    }
    Some(bytes)
}

/// Inflate to the end of the deflate stream, or not at all: truncated data doesn't count.
pub(super) fn inflate_whole(input: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut decoder = flate2::Decompress::new(true);
    let mut output = Vec::with_capacity(input.len().saturating_mul(3).clamp(4_096, limit.max(4_096)));
    loop {
        if output.len() == output.capacity() {
            if output.len() >= limit {
                return None;
            }
            output.reserve_exact(output.len().min(limit - output.len()).max(4_096));
        }
        let (read_before, written_before) = (decoder.total_in(), decoder.total_out());
        let consumed = usize::try_from(read_before).ok()?;
        match decoder
            .decompress_vec(&input[consumed..], &mut output, flate2::FlushDecompress::None)
            .ok()?
        {
            flate2::Status::StreamEnd => return Some(output),
            flate2::Status::Ok | flate2::Status::BufError => {
                let stuck = decoder.total_in() == read_before && decoder.total_out() == written_before;
                if stuck && output.len() < output.capacity() {
                    return None;
                }
            }
        }
    }
}

impl Plan<'_> {
    /// The holders a resource dictionary has, as held by `owner` if it's direct there.
    fn holders_of(&mut self, resources: &Object, owner: u32) -> RewriteResult<Vec<Holder>> {
        let (dict, in_resources) = match resources {
            Object::Reference((number, _)) => (self.peek_dict(*number)?, Some(*number)),
            Object::Dictionary(dict) => (Some(dict.clone()), None),
            _ => (None, None),
        };
        let Some(dict) = dict else {
            return Ok(Vec::new());
        };
        let mut holders = Vec::new();
        for (category, key) in CATEGORIES.iter().enumerate() {
            match dict.get(key) {
                Ok(Object::Reference((number, _))) => holders.push(Holder::Category(*number)),
                Ok(Object::Dictionary(_)) => holders.push(match in_resources {
                    Some(number) => Holder::InResources(number, category),
                    None => Holder::InOwner(owner, category),
                }),
                _ => {}
            }
        }
        Ok(holders)
    }

    /// Note what a removed page's resources are. `owner` is the page, or the node it inherits them from.
    pub(super) fn note_removed_resources(
        &mut self, owner: crate::ObjectId, shared: &mut SharedResources,
    ) -> RewriteResult<()> {
        let Some(dict) = self.peek_dict(owner.0)? else {
            return Ok(());
        };
        let Ok(resources) = dict.get(b"Resources") else {
            return Ok(());
        };
        for holder in self.holders_of(&resources.clone(), owner.0)? {
            if shared.holders.len() >= MAX_SHARED_HOLDERS {
                break;
            }
            // What's directly in a removed page dies with it.
            if !self.dead.get(holder.object()) {
                shared.holders.insert(holder);
            }
        }
        Ok(())
    }

    /// Set up the search from a removal: the kept pages that share a holder with a removed page are searched first.
    pub(super) fn start_pruner(&mut self, removal: &super::removal::PageRemoval) -> RewriteResult<Pruner> {
        let shared = &removal.shared;
        let mut pruner = Pruner {
            holders: shared
                .holders
                .iter()
                .map(|holder| {
                    (
                        *holder,
                        HolderState {
                            used: HashSet::new(),
                            has_user: false,
                            whole: false,
                        },
                    )
                })
                .collect(),
            bases: BTreeMap::new(),
            searched: HashSet::new(),
            pending: Vec::new(),
            found: BTreeSet::new(),
            opaque: Vec::new(),
            decoded: 0,
            decoded_limit: self
                .view
                .index
                .source_len
                .saturating_mul(SCAN_PER_FILE_BYTE)
                .max(SCAN_ALLOWANCE),
        };
        if pruner.holders.is_empty() {
            return Ok(pruner);
        }
        // A page the caller added takes its resources from wherever it was put, which isn't known here.
        let new_inheriting_page = self.objects.values().any(|change| {
            matches!(change, Change::Object { object: Object::Dictionary(dict), .. }
                if dict.has_type(b"Page") && !dict.has(b"Resources"))
        });
        if new_inheriting_page {
            pruner.give_up();
        }
        for holder in shared.holders.iter().copied() {
            let number = holder.object();
            if let std::collections::btree_map::Entry::Vacant(slot) = pruner.bases.entry(number)
                && let Some(base) = self.load(number)?
            {
                slot.insert(base);
            }
        }
        for (page, owner) in &shared.kept_pages {
            let Some(owner) = owner else { continue };
            let Some(dict) = self.peek_dict(*owner)? else { continue };
            let Ok(resources) = dict.get(b"Resources") else {
                continue;
            };
            let scope = self.scope_within(&resources.clone(), *owner, &pruner)?;
            if scope.is_empty() {
                continue;
            }
            let scope = Rc::new(scope);
            for holder in scope.iter() {
                if let Some(state) = pruner.holders.get_mut(holder) {
                    state.has_user = true;
                }
            }
            let Some(page_dict) = self.peek_dict(*page)? else {
                continue;
            };
            self.queue_page_content(&page_dict, &scope, &mut pruner)?;
        }
        self.run_searches(&mut pruner)?;
        Ok(pruner)
    }

    /// Holders of this dictionary that matter.
    fn scope_within(&mut self, resources: &Object, owner: u32, pruner: &Pruner) -> RewriteResult<Vec<Holder>> {
        let mut holders = self.holders_of(resources, owner)?;
        holders.retain(|holder| pruner.holders.contains_key(holder));
        Ok(holders)
    }

    fn queue_page_content(
        &mut self, page: &Dictionary, scope: &Rc<Vec<Holder>>, pruner: &mut Pruner,
    ) -> RewriteResult<()> {
        match page.get(b"Contents") {
            Ok(Object::Reference((number, _))) => {
                let number = *number;
                match self.peek(number)? {
                    Some(Loaded::Plain(Object::Array(items))) => {
                        for item in items {
                            if let Object::Reference((stream, _)) = item {
                                pruner.pending.push(Search {
                                    stream,
                                    scope: Rc::clone(scope),
                                });
                            }
                        }
                    }
                    _ => pruner.pending.push(Search {
                        stream: number,
                        scope: Rc::clone(scope),
                    }),
                }
            }
            Ok(Object::Array(items)) => {
                for item in items {
                    if let Object::Reference((stream, _)) = item {
                        pruner.pending.push(Search {
                            stream: *stream,
                            scope: Rc::clone(scope),
                        });
                    }
                }
            }
            _ => {}
        }
        // An appearance with no /Resources of its own: PDFium falls back to the page's.
        for annotation in self.deref_array(page.get(b"Annots").ok())? {
            let Some(annotation) = self.deref_dict(Some(&annotation))? else {
                continue;
            };
            let Some(appearance) = self.deref_dict(annotation.get(b"AP").ok())? else {
                continue;
            };
            for value in appearance.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>() {
                self.follow(&value, scope, pruner, 0)?;
            }
        }
        Ok(())
    }

    /// Something a used name led to. If it has content that falls back on this scope, or has its own in a holder that matters, queue it.
    /// A soft mask's group, a Type 3 font's glyphs and an appearance's states can all be direct.
    fn follow(
        &mut self, value: &Object, scope: &Rc<Vec<Holder>>, pruner: &mut Pruner, depth: usize,
    ) -> RewriteResult<()> {
        if depth > 8 {
            return Ok(());
        }
        let (number, loaded) = match value {
            Object::Reference((number, _)) => match self.peek(*number)? {
                Some(loaded) => (Some(*number), loaded),
                None => return Ok(()),
            },
            Object::Dictionary(dict) => (None, Loaded::Plain(Object::Dictionary(dict.clone()))),
            _ => return Ok(()),
        };
        // What's direct can't be a holder's owner in the sense that matters: it isn't shared.
        let owner = number.unwrap_or(u32::MAX);
        match loaded {
            Loaded::Stream(dict, _) => {
                let is_form = matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Form");
                let is_pattern = matches!(dict.get(b"PatternType"), Ok(Object::Integer(1)));
                let inner_scope = self.content_scope(&dict, owner, scope, pruner)?;
                if (is_form || is_pattern || dict.has(b"BBox")) && !inner_scope.is_empty() {
                    pruner.pending.push(Search {
                        stream: owner,
                        scope: inner_scope,
                    });
                }
            }
            Loaded::Plain(Object::Dictionary(dict)) => {
                if matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Type3") {
                    let inner_scope = self.content_scope(&dict, owner, scope, pruner)?;
                    if !inner_scope.is_empty()
                        && let Some(procs) = self.deref_dict(dict.get(b"CharProcs").ok())?
                    {
                        for (_, proc_value) in procs.iter() {
                            if let Object::Reference((stream, _)) = proc_value {
                                pruner.pending.push(Search {
                                    stream: *stream,
                                    scope: Rc::clone(&inner_scope),
                                });
                            }
                        }
                    }
                } else if let Some(mask) = self.deref_dict(dict.get(b"SMask").ok())? {
                    if let Ok(group) = mask.get(b"G") {
                        self.follow(&group.clone(), scope, pruner, depth + 1)?;
                    }
                } else if !dict.has(b"Type") && !dict.has(b"Subtype") {
                    // An appearance's states, or a graphics state with nothing to follow.
                    for inner in dict.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>() {
                        self.follow(&inner, scope, pruner, depth + 1)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The scope content in this object is searched under: its own `/Resources`, or whatever is using it.
    fn content_scope(
        &mut self, dict: &Dictionary, number: u32, scope: &Rc<Vec<Holder>>, pruner: &mut Pruner,
    ) -> RewriteResult<Rc<Vec<Holder>>> {
        let Ok(resources) = dict.get(b"Resources") else {
            return Ok(Rc::clone(scope));
        };
        let holders = self.scope_within(&resources.clone(), number, pruner)?;
        for holder in &holders {
            if let Some(state) = pruner.holders.get_mut(holder) {
                state.has_user = true;
            }
        }
        Ok(Rc::new(holders))
    }

    /// Search everything queued, and whatever the names it finds lead to.
    fn run_searches(&mut self, pruner: &mut Pruner) -> RewriteResult<()> {
        while let Some(search) = pruner.pending.pop() {
            if !pruner.searched.insert((search.stream, search.scope.as_ref().clone())) {
                continue;
            }
            let Some(bytes) = self.decoded_content(search.stream, pruner)? else {
                for holder in search.scope.iter() {
                    if let Some(state) = pruner.holders.get_mut(holder) {
                        state.whole = true;
                    }
                }
                continue;
            };
            let mut newly_used = Vec::new();
            every_name(&bytes, |name| {
                for holder in search.scope.iter() {
                    let Some(base) = pruner.bases.get(&holder.object()).and_then(Loaded::dictionary) else {
                        continue;
                    };
                    let Some(names) = holder.names(base) else { continue };
                    if names.has(name)
                        && let Some(state) = pruner.holders.get_mut(holder)
                        && state.used.insert(name.to_vec())
                        && let Ok(value) = names.get(name)
                    {
                        newly_used.push(value.clone());
                    }
                }
            });
            drop(bytes);
            for value in newly_used {
                self.follow(&value, &search.scope, pruner, 0)?;
            }
        }
        Ok(())
    }

    /// A stream's decoded bytes, or `None` if they can't be had.
    fn decoded_content(&mut self, number: u32, pruner: &mut Pruner) -> RewriteResult<Option<Vec<u8>>> {
        let limit = usize::try_from(self.view.limits.max_stream_bytes).unwrap_or(usize::MAX);
        let (mut dict, content) = match self.peek(number)? {
            Some(Loaded::Stream(dict, StreamBody::Source { start, length })) => {
                if length > self.view.limits.max_stream_bytes {
                    return Ok(None);
                }
                let content = self
                    .view
                    .source
                    .read_range(start, length, self.view.limits.max_stream_bytes)?;
                (dict, content)
            }
            Some(Loaded::Stream(dict, StreamBody::Given)) => match self.objects.get(&number) {
                Some(Change::Object {
                    object: Object::Stream(stream),
                    ..
                }) => (dict, stream.content.clone()),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        pruner.decoded = pruner.decoded.saturating_add(content.len() as u64);
        for key in [&b"Filter"[..], b"DecodeParms"] {
            if let Ok(Object::Reference(_)) = dict.get(key)
                && let Some(value) = self.deref(&dict.get(key).cloned().unwrap_or(Object::Null))?
            {
                dict.set(key.to_vec(), value);
            }
        }
        let Some(bytes) = decode_all_or_nothing(dict, content, limit) else {
            return Ok(None);
        };
        pruner.decoded = pruner.decoded.saturating_add(bytes.len() as u64);
        if pruner.decoded > pruner.decoded_limit {
            pruner.give_up();
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// What the walk found using a holder that nothing has searched yet: search it. True if there's anything new, and the
    /// walk has to go again, since what's pruned has changed.
    pub(super) fn scan_new_users(&mut self, pruner: &mut Pruner) -> RewriteResult<bool> {
        if pruner.holders.is_empty() {
            return Ok(false);
        }
        let found = std::mem::take(&mut pruner.found);
        let before = pruner.searched.len();
        let mut grew = false;
        for (number, resources) in std::mem::take(&mut pruner.opaque) {
            for holder in self.scope_within(&resources, number, pruner)? {
                if let Some(state) = pruner.holders.get_mut(&holder) {
                    grew |= !state.whole;
                    state.whole = true;
                }
            }
        }
        let mut queue = Vec::new();
        for number in found {
            let Some(loaded) = self.peek(number)? else { continue };
            let Some(dict) = loaded.dictionary().cloned() else {
                continue;
            };
            let Ok(resources) = dict.get(b"Resources").cloned() else {
                continue;
            };
            let scope = self.scope_within(&resources, number, pruner)?;
            if scope.is_empty() {
                continue;
            }
            queue.push((number, loaded, dict, scope));
        }
        for (number, loaded, dict, scope) in queue {
            let scope = Rc::new(scope);
            let was_unused = scope
                .iter()
                .any(|holder| pruner.holders.get(holder).is_some_and(|state| !state.has_user));
            for holder in scope.iter() {
                if let Some(state) = pruner.holders.get_mut(holder) {
                    state.has_user = true;
                }
            }
            grew |= was_unused;
            let type3 = matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Type3");
            match loaded {
                Loaded::Stream(..) => pruner.pending.push(Search { stream: number, scope }),
                Loaded::Plain(_) if type3 => {
                    if let Some(procs) = self.deref_dict(dict.get(b"CharProcs").ok())? {
                        for (_, value) in procs.iter() {
                            if let Object::Reference((stream, _)) = value {
                                pruner.pending.push(Search {
                                    stream: *stream,
                                    scope: Rc::clone(&scope),
                                });
                            }
                        }
                    }
                }
                Loaded::Plain(_) if dict.has_type(b"Page") => self.queue_page_content(&dict, &scope, pruner)?,
                // Something else with /Resources, which isn't known to draw anything. No telling what it might use.
                Loaded::Plain(_) => {
                    for holder in scope.iter() {
                        if let Some(state) = pruner.holders.get_mut(holder) {
                            grew |= !state.whole;
                            state.whole = true;
                        }
                    }
                }
            }
        }
        self.run_searches(pruner)?;
        Ok(grew || pruner.searched.len() != before)
    }

    /// Make the edits that the search so far allows. What's whole, and what nothing kept uses, stays as it was: the second is
    /// out of reach anyway, unless the walk finds a user, and then it's searched.
    pub(super) fn apply_pruner(&mut self, pruner: &mut Pruner) -> RewriteResult<()> {
        if pruner.holders.is_empty() {
            return Ok(());
        }
        for (number, base) in &pruner.bases {
            let mut edited = base.clone();
            let mut changed = false;
            for (holder, state) in pruner.holders.range(..) {
                if holder.object() != *number || !state.has_user || state.whole {
                    continue;
                }
                let names = match &mut edited {
                    Loaded::Plain(Object::Dictionary(dict)) | Loaded::Stream(dict, _) => holder.names_mut(dict),
                    _ => None,
                };
                let Some(names) = names else { continue };
                let before = names.len();
                names.as_hashmap_mut().retain(|name, _| {
                    state.used.contains(name) || NAMES_LOOKED_UP_IMPLICITLY.contains(&name.as_slice())
                });
                changed |= names.len() != before;
            }
            if changed {
                self.edits.insert(*number, edited);
            } else {
                self.edits.remove(number);
                // Back to what it was before this search, which may itself have been an edit.
                if !matches!(base, Loaded::Stream(_, StreamBody::Given)) {
                    self.edits.insert(*number, base.clone());
                }
            }
        }
        Ok(())
    }

    /// Say what the search did. Called when it's settled.
    pub(super) fn finish_pruner(&mut self, pruner: Pruner) {
        for (holder, state) in &pruner.holders {
            if state.whole {
                self.report
                    .resources_left_whole
                    .push((holder.object(), self.generation(holder.object())));
            } else if state.has_user
                && let Some(base) = pruner.bases.get(&holder.object()).and_then(Loaded::dictionary)
                && let Some(names) = holder.names(base)
            {
                let gone = names
                    .iter()
                    .filter(|(name, _)| {
                        !state.used.contains(*name) && !NAMES_LOOKED_UP_IMPLICITLY.contains(&name.as_slice())
                    })
                    .count();
                self.report.resource_names_pruned += gone as u64;
            }
        }
        self.report.resources_left_whole.sort_unstable();
        self.report.resources_left_whole.dedup();
    }
}
