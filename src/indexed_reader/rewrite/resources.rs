//! Shared `/Resources`: taking out what only a removed page used.
//!
//! A kept page and a removed page can share a resource dictionary, by inheriting it, by reference, or through a form or font they both use, and some
//! producers put every image in the file in one. Everything in it stays reachable from the kept page, drawn or not, so reachability alone would keep
//! the removed page's images.
//!
//! So first everything the removed pages could have drawn from is listed, structurally and without decoding anything: every resource dictionary
//! their pages, their annotations' appearances and whatever those name can reach. Those are the dictionaries that may need pruning. Then everything
//! kept that could draw from one of them is searched: each kept page whose own or inherited resources include one, with its annotations'
//! appearances, and anything the walk reaches that has its own `/Resources` including one. A search follows every name it finds, in every category,
//! shared or not, into forms, patterns, Type 3 glyphs, soft masks and appearance states. What it follows is searched under its own resources *and*
//! everything above it, since viewers fall back to the page's, PDFium a category at a time and others a name at a time, and the union covers both.
//! The entries of a listed dictionary that no search found go. A listed dictionary that something reachable holds and nothing kept draws from keeps
//! only what viewers look up by themselves.
//!
//! The search takes every `/Name` in the decoded bytes, wherever it is, with no attempt to tell a string or an inline image from an operator. That
//! can only find too many, which keeps too much, and never too few, which would break a kept page. Anything that can't be decoded to its end, a
//! `/DR` (fields use those by a name in a string), `/Resources` inside something direct, or anything else with `/Resources` that isn't known to
//! draw leaves the dictionaries it touches whole, and they're reported. Pruning makes less reachable, which can only make less kept content need
//! searching, so it's repeated until it settles, and stopping early is safe: each round keeps at least what the next one needs.

use std::collections::BTreeSet;

use super::*;

/// The most content one rewrite decodes to search for names, at the least. Past it the search stops and every listed dictionary stays whole.
const SCAN_ALLOWANCE: u64 = 1_024 * 1_024 * 1_024;
const SCAN_PER_FILE_BYTE: u64 = 8;
/// Dictionaries one rewrite lists. Past it the rest aren't listed, and the report says how many weren't.
const MAX_LISTED_HOLDERS: usize = if cfg!(test) { 64 } else { 100_000 };
/// Objects looked at while listing.
const MAX_LISTING_VISITS: usize = 1_000_000;
/// Distinct names kept from decoded streams, so that each is decoded once. Past it a stream is decoded again when it comes up again.
const MAX_REMEMBERED_NAMES: usize = 1_000_000;
/// Searches one round makes: one stream, each time its context grows.
const MAX_SEARCHES: usize = 1_000_000;
/// Rounds of pruning and walking.
const MAX_ROUNDS: usize = 4;
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
    /// `/Resources << /Category << ... >> >>` directly in this object, such as a page, a `/Pages` node or a form.
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

/// What removed pages could draw from, listed before anything is known about who else uses it.
#[derive(Default)]
pub(super) struct SharedResources {
    holders: BTreeSet<Holder>,
    /// Dictionaries past the limit, which weren't listed.
    unlisted: u64,
    visited: HashSet<u32>,
    /// Each kept page, and the object its resources came from.
    kept_pages: Vec<(u32, Option<u32>)>,
}

impl SharedResources {
    pub(super) fn remember_kept_pages(&mut self, kept: &[&PageMapEntry]) {
        self.kept_pages = kept
            .iter()
            .map(|entry| (entry.id.0, entry.inherited.resources.map(|owner| owner.0)))
            .collect();
        self.visited = HashSet::new();
    }
}

/// Holders a stream's names are looked up in: its own and those of everything that uses it.
type Context = BTreeSet<Holder>;

/// One round's findings.
#[derive(Default)]
struct Round {
    used: HashMap<Holder, BTreeSet<Vec<u8>>>,
    whole: BTreeSet<Holder>,
    /// Every stream found so far, with the union of every context it's used in. A stream is searched again only when that grows, so content
    /// that reaches itself through a shared dictionary, or a form used from a thousand pages, costs a search for each holder it gains, not
    /// for each path to it.
    contexts: HashMap<u32, Context>,
    pending: Vec<u32>,
    queued: HashSet<u32>,
    held: usize,
    /// Each holder's names, looked up once a round.
    names_of: HashMap<Holder, Option<Rc<Dictionary>>>,
    /// Each annotation array, read once a round, with only its references kept.
    annotations: HashMap<u32, Rc<Vec<Object>>>,
    searches: usize,
}

/// Holders in all contexts together, past which the search gives up.
const MAX_CONTEXT_ENTRIES: usize = 4_000_000;

/// The state of the search across rounds.
pub(super) struct Pruner {
    /// What removed pages could draw from.
    listed: BTreeSet<Holder>,
    unlisted: u64,
    kept_pages: Vec<(u32, Option<u32>)>,
    /// The objects holding listed holders, as they were before any pruning.
    bases: BTreeMap<u32, Loaded>,
    /// Names in each stream decoded so far, or `None` for one that can't be decoded.
    names: HashMap<u32, Option<Rc<[Vec<u8>]>>>,
    remembered_names: usize,
    /// Reachable objects with `/Resources`, and `/DR` and nested `/Resources`, that the last walk found.
    found: BTreeSet<u32>,
    opaque: Vec<(u32, Object)>,
    /// What's pruned now: each pruned holder, with the names it keeps.
    applied: BTreeMap<Holder, BTreeSet<Vec<u8>>>,
    /// Holders that stay whole, whatever later rounds find.
    whole: BTreeSet<Holder>,
    decoded: u64,
    decoded_limit: u64,
    gave_up: bool,
    /// One buffer for reading and one for inflating, reused, so that large streams don't leave large freed blocks behind.
    raw: Vec<u8>,
    inflated: Vec<u8>,
}

impl Pruner {
    /// A dictionary the walk reached. If it has `/Resources`, it may use a listed holder, which is looked at after the walk.
    pub(super) fn note_reached(&mut self, number: u32, dict: &Dictionary) {
        if self.listed.is_empty() {
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

    fn give_up(&mut self) {
        self.gave_up = true;
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
/// it and wrong here: names past the damage wouldn't be seen, a viewer might get further, and a page would lose what it uses. Nothing past
/// `limit` is ever produced.
fn decode_all_or_nothing(dict: &Dictionary, content: &[u8], limit: usize, output: &mut Vec<u8>) -> Option<()> {
    output.clear();
    if !dict.has(b"Filter") {
        if content.len() > limit {
            return None;
        }
        output.extend_from_slice(content);
        return Some(());
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
    if let [only] = filters.as_slice()
        && flate(only)
    {
        // The usual case, straight into the reused buffer.
        return inflate_whole_into(content, limit, output);
    }
    let mut bytes = content.to_vec();
    for filter in &filters {
        bytes = if flate(filter) {
            inflate_whole_into(&bytes, limit, output)?;
            std::mem::take(output)
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
    *output = bytes;
    Some(())
}

/// Inflate to the end of the deflate stream, or not at all: truncated data doesn't count.
#[cfg(test)]
pub(super) fn inflate_whole(input: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    inflate_whole_into(input, limit, &mut output)?;
    Some(output)
}

/// Inflate into a buffer of the caller's, cleared first, to the end of the deflate stream or not at all. It never grows past `limit`.
fn inflate_whole_into(input: &[u8], limit: usize, output: &mut Vec<u8>) -> Option<()> {
    let mut decoder = flate2::Decompress::new(true);
    output.clear();
    output.reserve(
        input
            .len()
            .saturating_mul(3)
            .clamp(4_096, limit.max(4_096))
            .min(limit.max(1)),
    );
    loop {
        if output.len() == output.capacity() {
            if output.len() >= limit {
                return None;
            }
            output.reserve_exact(output.len().max(4_096).min(limit - output.len()));
        }
        let (read_before, written_before) = (decoder.total_in(), decoder.total_out());
        let consumed = usize::try_from(read_before).ok()?;
        match decoder
            .decompress_vec(&input[consumed..], output, flate2::FlushDecompress::None)
            .ok()?
        {
            flate2::Status::StreamEnd => return Some(()),
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
    /// The holders a resource dictionary has, as held by `owner` if it's direct there. A reference, or a category, is followed through
    /// references to references to the object that holds it.
    fn holders_of(&mut self, resources: &Object, owner: u32) -> RewriteResult<Vec<Holder>> {
        let (dict, in_resources) = match resources {
            Object::Reference(_) => match self.final_object(resources)? {
                Some((number, Object::Dictionary(dict))) => (Some(dict), Some(number)),
                _ => (None, None),
            },
            Object::Dictionary(dict) => (Some(dict.clone()), None),
            _ => (None, None),
        };
        let Some(dict) = dict else {
            return Ok(Vec::new());
        };
        let mut holders = Vec::new();
        for (category, key) in CATEGORIES.iter().enumerate() {
            match dict.get(key) {
                Ok(value @ Object::Reference(_)) => {
                    if let Some((number, Object::Dictionary(_))) = self.final_object(&value.clone())? {
                        holders.push(Holder::Category(number));
                    }
                }
                Ok(Object::Dictionary(_)) => holders.push(match in_resources {
                    Some(number) => Holder::InResources(number, category),
                    None => Holder::InOwner(owner, category),
                }),
                _ => {}
            }
        }
        Ok(holders)
    }

    /// Follow references to the object at the end, and give its number and value, a stream as its dictionary.
    fn final_object(&mut self, value: &Object) -> RewriteResult<Option<(u32, Object)>> {
        let Object::Reference((first, _)) = value else {
            return Ok(None);
        };
        let mut number = *first;
        for _ in 0..32 {
            match self.peek(number)? {
                Some(Loaded::Plain(Object::Reference((next, _)))) => number = next,
                Some(Loaded::Plain(object)) => return Ok(Some((number, object))),
                Some(Loaded::Stream(dict, _)) => return Ok(Some((number, Object::Dictionary(dict)))),
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// List what a removed page could draw from: the dictionaries of its resources, its annotations' appearances, and whatever those can reach,
    /// whether a removed page draws it or not. Nothing is decoded: listing too much costs a search, and listing too little leaves a removed
    /// page's content behind.
    pub(super) fn note_removed_resources(
        &mut self, page: crate::ObjectId, owner: Option<crate::ObjectId>, shared: &mut SharedResources,
    ) -> RewriteResult<()> {
        let mut queue: Vec<(Object, u32)> = Vec::new();
        if let Some(owner) = owner
            && let Some(dict) = self.peek_dict(owner.0)?
            && let Ok(resources) = dict.get(b"Resources")
        {
            queue.push((resources.clone(), owner.0));
        }
        if let Some(dict) = self.peek_dict(page.0)? {
            let annotations = match dict.get(b"Annots") {
                Ok(Object::Reference((number, _))) if !shared.visited.insert(*number) => Vec::new(),
                other => self.deref_array(other.ok())?,
            };
            for annotation in annotations {
                if let Some(annotation) = self.deref_dict(Some(&annotation))?
                    && let Ok(appearance) = annotation.get(b"AP")
                {
                    self.list_value(appearance.clone(), page.0, shared, &mut queue)?;
                }
            }
        }
        while let Some((resources, owner)) = queue.pop() {
            for holder in self.holders_of(&resources, owner)? {
                // What's directly in a removed page dies with it, though what it names may not.
                let dies = self.dead.get(holder.object()) && matches!(holder, Holder::InOwner(..));
                if !dies && !shared.holders.contains(&holder) {
                    if shared.holders.len() >= MAX_LISTED_HOLDERS {
                        shared.unlisted += 1;
                        continue;
                    }
                    shared.holders.insert(holder);
                }
                let Some(base) = self.peek_dict(holder.object())? else {
                    continue;
                };
                let Some(names) = holder.names(&base) else { continue };
                for (_, value) in names.iter() {
                    self.list_value(value.clone(), owner, shared, &mut queue)?;
                }
            }
        }
        Ok(())
    }

    /// Queue whatever resources something a removed page names has of its own, and what's under it.
    fn list_value(
        &mut self, value: Object, owner: u32, shared: &mut SharedResources, queue: &mut Vec<(Object, u32)>,
    ) -> RewriteResult<()> {
        let mut stack = vec![(value, 0_usize)];
        while let Some((value, depth)) = stack.pop() {
            if depth > 16 {
                continue;
            }
            if shared.visited.len() >= MAX_LISTING_VISITS {
                shared.unlisted += 1;
                return Ok(());
            }
            let (number, object) = match &value {
                Object::Reference((number, _)) => {
                    if !shared.visited.insert(*number) {
                        continue;
                    }
                    match self.final_object(&value)? {
                        Some(found) => found,
                        None => continue,
                    }
                }
                Object::Dictionary(_) | Object::Array(_) => (owner, value.clone()),
                _ => continue,
            };
            match object {
                Object::Dictionary(dict) => {
                    if let Ok(resources) = dict.get(b"Resources") {
                        queue.push((resources.clone(), number));
                    }
                    for key in [&b"CharProcs"[..], b"SMask", b"G", b"N", b"D", b"R", b"Font", b"Pattern"] {
                        if let Ok(inner) = dict.get(key) {
                            stack.push((inner.clone(), depth + 1));
                        }
                    }
                    // An appearance's states, a glyph table.
                    if !dict.has(b"Type") && !dict.has(b"Subtype") {
                        for (_, inner) in dict.iter() {
                            stack.push((inner.clone(), depth + 1));
                        }
                    }
                }
                Object::Array(items) => {
                    for item in items {
                        stack.push((item, depth + 1));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Set up the search from what a removal listed.
    pub(super) fn start_pruner(&mut self, removal: &super::removal::PageRemoval) -> Pruner {
        let shared = &removal.shared;
        let mut pruner = Pruner {
            listed: shared.holders.clone(),
            unlisted: shared.unlisted,
            kept_pages: shared.kept_pages.clone(),
            bases: BTreeMap::new(),
            names: HashMap::new(),
            remembered_names: 0,
            found: BTreeSet::new(),
            opaque: Vec::new(),
            applied: BTreeMap::new(),
            whole: BTreeSet::new(),
            decoded: 0,
            decoded_limit: self
                .view
                .index
                .source_len
                .saturating_mul(SCAN_PER_FILE_BYTE)
                .max(SCAN_ALLOWANCE),
            gave_up: false,
            raw: Vec::new(),
            inflated: Vec::new(),
        };
        // A page the caller added takes its resources from wherever it was put, which isn't known here.
        let new_inheriting_page = self.objects.values().any(|change| {
            matches!(change, Change::Object { object: Object::Dictionary(dict), .. }
                if dict.has_type(b"Page") && !dict.has(b"Resources"))
        });
        if new_inheriting_page {
            pruner.give_up();
        }
        pruner
    }

    /// The walk, with pruning settled around it: walk, see what kept content uses, prune, and walk again until nothing changes. The last walk
    /// is always under the pruning that's in place.
    pub(super) fn settle_pruning(&mut self, pruner: &mut Pruner, layers: &[u8]) -> RewriteResult<()> {
        for round in 0..=MAX_ROUNDS {
            let roots = self.roots()?;
            self.reach(roots, Some(pruner), Some(layers))?;
            if pruner.listed.is_empty() || round == MAX_ROUNDS {
                return Ok(());
            }
            if round == 0 {
                self.capture_bases(pruner)?;
            }
            let wanted = if pruner.gave_up {
                BTreeMap::new()
            } else {
                self.search_round(pruner)?
            };
            // Out of allowance in the middle of a round: nothing it found can be trusted to be all there is.
            let wanted = if pruner.gave_up { BTreeMap::new() } else { wanted };
            if wanted == pruner.applied {
                return Ok(());
            }
            pruner.applied = wanted;
            self.apply_pruning(pruner);
        }
        Ok(())
    }

    /// Keep each listed holder's object as it is before anything is pruned, if it's reachable at all. One that isn't is gone anyway.
    fn capture_bases(&mut self, pruner: &mut Pruner) -> RewriteResult<()> {
        let objects: BTreeSet<u32> = pruner.listed.iter().map(|holder| holder.object()).collect();
        for number in objects {
            if self.live.get(number)
                && let Some(base) = self.load(number)?
            {
                pruner.bases.insert(number, base);
            }
        }
        let bases = &pruner.bases;
        pruner.listed.retain(|holder| bases.contains_key(&holder.object()));
        Ok(())
    }

    /// One round: search everything kept that can draw from a listed holder, and say what each listed holder has to keep.
    fn search_round(&mut self, pruner: &mut Pruner) -> RewriteResult<BTreeMap<Holder, BTreeSet<Vec<u8>>>> {
        let mut round = Round::default();
        // What can't be searched keeps what it touches.
        for (number, resources) in std::mem::take(&mut pruner.opaque) {
            for holder in self.holders_of(&resources, number)? {
                if pruner.listed.contains(&holder) {
                    pruner.whole.insert(holder);
                }
            }
        }
        // Kept pages, under their own or inherited resources.
        for (page, owner) in pruner.kept_pages.clone() {
            let Some(owner) = owner else { continue };
            let Some(dict) = self.peek_dict(owner)? else { continue };
            let Ok(resources) = dict.get(b"Resources") else {
                continue;
            };
            let context: Context = self.holders_of(&resources.clone(), owner)?.into_iter().collect();
            if !context.iter().any(|holder| pruner.listed.contains(holder)) {
                continue;
            }
            let Some(page_dict) = self.peek_dict(page)? else {
                continue;
            };
            self.queue_page(&page_dict, &context, &mut round)?;
        }
        // Whatever else reachable has resources of its own that include one.
        for number in std::mem::take(&mut pruner.found) {
            let Some(loaded) = self.peek(number)? else { continue };
            let Some(dict) = loaded.dictionary().cloned() else {
                continue;
            };
            let Ok(resources) = dict.get(b"Resources").cloned() else {
                continue;
            };
            let context: Context = self.holders_of(&resources, number)?.into_iter().collect();
            if !context.iter().any(|holder| pruner.listed.contains(holder)) {
                continue;
            }
            match loaded {
                Loaded::Stream(..) => self.queue(number, &context, &mut round),
                Loaded::Plain(_) if dict.has_type(b"Page") => self.queue_page(&dict, &context, &mut round)?,
                Loaded::Plain(_) if matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Type3") => {
                    self.queue_glyphs(&dict, &context, &mut round)?;
                }
                // Something with /Resources that isn't known to draw. No telling what it uses.
                Loaded::Plain(_) => {
                    for holder in &context {
                        if pruner.listed.contains(holder) {
                            pruner.whole.insert(*holder);
                        }
                    }
                }
            }
        }
        self.run_searches(pruner, &mut round)?;
        let mut wanted = BTreeMap::new();
        for holder in &pruner.listed {
            if pruner.whole.contains(holder) || round.whole.contains(holder) {
                pruner.whole.insert(*holder);
                continue;
            }
            let mut keep = round.used.remove(holder).unwrap_or_default();
            keep.extend(NAMES_LOOKED_UP_IMPLICITLY.iter().map(|name| name.to_vec()));
            wanted.insert(*holder, keep);
        }
        Ok(wanted)
    }

    /// Add holders to a stream's context, and queue it to be searched again if that grew it.
    fn queue(&mut self, stream: u32, context: &Context, round: &mut Round) {
        let known = round.contexts.entry(stream).or_default();
        let before = known.len();
        known.extend(context.iter().copied());
        let grew = known.len() - before;
        if grew > 0 {
            round.held += grew;
            if round.queued.insert(stream) {
                round.pending.push(stream);
            }
        }
    }

    fn queue_page(&mut self, page: &Dictionary, context: &Context, round: &mut Round) -> RewriteResult<()> {
        let contents = match page.get(b"Contents") {
            Ok(Object::Reference((number, _))) => match self.peek(*number)? {
                Some(Loaded::Plain(Object::Array(items))) => items,
                _ => vec![Object::Reference((*number, 0))],
            },
            Ok(Object::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        for item in contents {
            if let Object::Reference((stream, _)) = item {
                self.queue(stream, context, round);
            }
        }
        // An appearance falls back on the page's resources in PDFium.
        let annotations = match page.get(b"Annots") {
            Ok(Object::Reference((number, _))) => match round.annotations.get(number) {
                Some(known) => Rc::clone(known),
                None => {
                    let list: Vec<Object> = self
                        .deref_array(page.get(b"Annots").ok())?
                        .into_iter()
                        .filter(|item| matches!(item, Object::Reference(_) | Object::Dictionary(_)))
                        .collect();
                    let list = Rc::new(list);
                    round.annotations.insert(*number, Rc::clone(&list));
                    list
                }
            },
            other => Rc::new(self.deref_array(other.ok())?),
        };
        for annotation in annotations.iter() {
            if let Some(annotation) = self.deref_dict(Some(annotation))?
                && let Ok(appearance) = annotation.get(b"AP")
            {
                self.follow(&appearance.clone(), context, round, 0)?;
            }
        }
        Ok(())
    }

    fn queue_glyphs(&mut self, font: &Dictionary, context: &Context, round: &mut Round) -> RewriteResult<()> {
        if let Some(procs) = self.deref_dict(font.get(b"CharProcs").ok())? {
            for (_, value) in procs.iter() {
                if let Object::Reference((stream, _)) = value {
                    self.queue(*stream, context, round);
                }
            }
        }
        Ok(())
    }

    /// The context for something with these resources of its own, used in `context`: its own and everything above, as viewers fall back on.
    fn context_below(&mut self, resources: Option<&Object>, number: u32, context: &Context) -> RewriteResult<Context> {
        let mut below = context.clone();
        if let Some(resources) = resources {
            below.extend(self.holders_of(&resources.clone(), number)?);
        }
        Ok(below)
    }

    /// Something a used name led to: queue whatever content it has, in its own resources and then `context`.
    fn follow(&mut self, value: &Object, context: &Context, round: &mut Round, depth: usize) -> RewriteResult<()> {
        if depth > 8 {
            return Ok(());
        }
        let (number, loaded) = match value {
            Object::Reference(_) => match self.final_object(value)? {
                Some((number, _)) => match self.peek(number)? {
                    Some(loaded) => (Some(number), loaded),
                    None => return Ok(()),
                },
                None => return Ok(()),
            },
            Object::Dictionary(dict) => (None, Loaded::Plain(Object::Dictionary(dict.clone()))),
            Object::Array(items) => {
                // An ExtGState's /Font is [font size].
                if let Some(first) = items.first() {
                    self.follow(&first.clone(), context, round, depth + 1)?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        // A direct dictionary isn't a holder anyone else shares.
        let owner = number.unwrap_or(u32::MAX);
        match loaded {
            Loaded::Stream(dict, _) => {
                let is_form = matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Form");
                let is_pattern = matches!(dict.get(b"PatternType"), Ok(Object::Integer(1)));
                if (is_form || is_pattern || dict.has(b"BBox"))
                    && let Some(number) = number
                {
                    let below = self.context_below(dict.get(b"Resources").ok(), number, context)?;
                    self.queue(number, &below, round);
                }
            }
            Loaded::Plain(Object::Dictionary(dict)) => {
                if matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Type3") {
                    let below = self.context_below(dict.get(b"Resources").ok(), owner, context)?;
                    self.queue_glyphs(&dict, &below, round)?;
                    return Ok(());
                }
                for key in [&b"SMask"[..], b"G", b"Font"] {
                    if let Ok(inner) = dict.get(key) {
                        self.follow(&inner.clone(), context, round, depth + 1)?;
                    }
                }
                if !dict.has(b"Type") && !dict.has(b"Subtype") {
                    // An appearance's states.
                    for inner in dict.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>() {
                        self.follow(&inner, context, round, depth + 1)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Search everything queued, and whatever the names it finds lead to, until no context grows.
    fn run_searches(&mut self, pruner: &mut Pruner, round: &mut Round) -> RewriteResult<()> {
        while let Some(stream) = round.pending.pop() {
            round.queued.remove(&stream);
            if pruner.gave_up {
                return Ok(());
            }
            round.searches += 1;
            if round.searches >= MAX_SEARCHES || round.held >= MAX_CONTEXT_ENTRIES {
                pruner.give_up();
                return Ok(());
            }
            let context = round.contexts.get(&stream).cloned().unwrap_or_default();
            let Some(names) = self.stream_names(stream, pruner)? else {
                round.whole.extend(context.iter().copied());
                continue;
            };
            let mut followed = Vec::new();
            for holder in &context {
                let Some(dictionary) = self.holder_names(*holder, pruner, round)? else {
                    continue;
                };
                for name in names.iter() {
                    let Ok(value) = dictionary.get(name) else { continue };
                    if pruner.listed.contains(holder) {
                        round.used.entry(*holder).or_default().insert(name.clone());
                    }
                    followed.push(value.clone());
                }
            }
            for value in followed {
                self.follow(&value, &context, round, 0)?;
            }
        }
        Ok(())
    }

    /// A holder's dictionary of names, as it was before any pruning, looked up once a round.
    fn holder_names(
        &mut self, holder: Holder, pruner: &Pruner, round: &mut Round,
    ) -> RewriteResult<Option<Rc<Dictionary>>> {
        if let Some(known) = round.names_of.get(&holder) {
            return Ok(known.clone());
        }
        let current;
        let base = match pruner.bases.get(&holder.object()) {
            Some(base) => base.dictionary(),
            None => {
                current = self.peek(holder.object())?;
                current.as_ref().and_then(Loaded::dictionary)
            }
        };
        let names = base
            .and_then(|base| holder.names(base))
            .map(|names| Rc::new(names.clone()));
        round.names_of.insert(holder, names.clone());
        Ok(names)
    }

    /// The names in a stream's decoded bytes, decoded at most once while there's room to remember them. `None` if it can't be decoded to its end.
    fn stream_names(&mut self, number: u32, pruner: &mut Pruner) -> RewriteResult<Option<Rc<[Vec<u8>]>>> {
        if let Some(known) = pruner.names.get(&number) {
            return Ok(known.clone());
        }
        let limit_stream = self.view.limits.max_stream_bytes;
        let (mut dict, content_from) = match self.peek(number)? {
            Some(Loaded::Stream(dict, StreamBody::Source { start, length })) => (dict, Some((start, length))),
            Some(Loaded::Stream(dict, StreamBody::Given)) => (dict, None),
            _ => {
                pruner.names.insert(number, None);
                return Ok(None);
            }
        };
        // What's left of the allowance bounds this one too, so that nothing is decoded past it.
        let left = pruner.decoded_limit.saturating_sub(pruner.decoded);
        let limit = usize::try_from(limit_stream.min(left)).unwrap_or(usize::MAX);
        let mut raw = std::mem::take(&mut pruner.raw);
        let readable = match content_from {
            Some((start, length)) if length <= limit as u64 => {
                raw.clear();
                raw.resize(usize::try_from(length).unwrap_or(0), 0);
                self.view.source.read_exact_at(start, &mut raw)?;
                true
            }
            Some(_) => false,
            None => match self.objects.get(&number) {
                Some(Change::Object {
                    object: Object::Stream(stream),
                    ..
                }) if stream.content.len() <= limit => {
                    raw.clear();
                    raw.extend_from_slice(&stream.content);
                    true
                }
                _ => false,
            },
        };
        for key in [&b"Filter"[..], b"DecodeParms"] {
            if let Ok(Object::Reference(_)) = dict.get(key)
                && let Some(value) = self.deref(&dict.get(key).cloned().unwrap_or(Object::Null))?
            {
                dict.set(key.to_vec(), value);
            }
        }
        let mut decoded = std::mem::take(&mut pruner.inflated);
        let complete = readable && {
            pruner.decoded = pruner.decoded.saturating_add(raw.len() as u64);
            decode_all_or_nothing(&dict, &raw, limit, &mut decoded).is_some()
        };
        pruner.raw = raw;
        if !complete {
            pruner.inflated = decoded;
            if left < limit_stream {
                // Out of allowance, not out of luck: everything stays whole from here.
                pruner.give_up();
            }
            pruner.names.insert(number, None);
            return Ok(None);
        }
        pruner.decoded = pruner.decoded.saturating_add(decoded.len() as u64);
        let mut names = BTreeSet::new();
        every_name(&decoded, |name| {
            names.insert(name.to_vec());
        });
        pruner.inflated = decoded;
        let names: Rc<[Vec<u8>]> = names.into_iter().collect();
        if pruner.remembered_names + names.len() <= MAX_REMEMBERED_NAMES {
            pruner.remembered_names += names.len();
            pruner.names.insert(number, Some(Rc::clone(&names)));
        }
        Ok(Some(names))
    }

    /// Put what's settled in place: each pruned holder keeps only its names. Everything else is as its base was.
    fn apply_pruning(&mut self, pruner: &Pruner) {
        for (number, base) in &pruner.bases {
            let mut edited = base.clone();
            for (holder, keep) in &pruner.applied {
                if holder.object() != *number {
                    continue;
                }
                let names = match &mut edited {
                    Loaded::Plain(Object::Dictionary(dict)) | Loaded::Stream(dict, _) => holder.names_mut(dict),
                    _ => None,
                };
                if let Some(names) = names {
                    names.as_hashmap_mut().retain(|name, _| keep.contains(name));
                }
            }
            self.edits.insert(*number, edited);
        }
    }

    /// Say what the search did, once it's settled.
    pub(super) fn finish_pruner(&mut self, pruner: Pruner) {
        let mut whole: BTreeSet<u32> = BTreeSet::new();
        for holder in &pruner.listed {
            let pruned = pruner.applied.get(holder);
            if pruner.gave_up || pruner.whole.contains(holder) || pruned.is_none() {
                if self.live.get(holder.object()) {
                    whole.insert(holder.object());
                }
                continue;
            }
            if let (Some(keep), Some(base)) = (pruned, pruner.bases.get(&holder.object()).and_then(Loaded::dictionary))
                && let Some(names) = holder.names(base)
            {
                let gone = names.iter().filter(|(name, _)| !keep.contains(*name)).count();
                self.report.resource_names_pruned += gone as u64;
            }
        }
        self.report.resources_left_whole = whole
            .into_iter()
            .map(|number| (number, self.generation(number)))
            .collect();
        self.report.resource_dictionaries_unlisted = pruner.unlisted;
    }
}
