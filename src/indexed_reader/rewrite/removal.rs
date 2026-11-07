//! What removing pages does to everything that refers to them. See the module docs of `rewrite` for the rules.
//!
//! Nothing here is written out as it's found. It marks what's dead, which every later reference obeys, and keeps a replacement
//! only for an object whose content has to change in another way: a `/Count`, a bookmark's links, a name tree's `/Limits`.

use super::resources::SharedResources;
use super::*;

/// Depth followed in a name tree, a number tree or a form's fields before a branch is left as it is. The stack each level costs
/// is the limit. Real ones go a few levels deep.
const MAX_TREE_DEPTH: usize = 64;
/// Bookmarks one document can have here. Each costs about 100 bytes while they're looked at.
const MAX_OUTLINE_ITEMS: u64 = 500_000;
/// Page-label ranges one document can have here. Each is a small dictionary, and a real document has a few.
const MAX_PAGE_LABEL_RANGES: u64 = 100_000;
/// Beads one article thread can have here.
const MAX_THREAD_BEADS: usize = 1_000_000;

/// What the later steps need to know about a removal.
pub(super) struct PageRemoval {
    kept_count: usize,
    pub(super) shared: SharedResources,
}

impl PageRemoval {
    pub(super) fn kept_count(&self) -> usize {
        self.kept_count
    }
}

/// Which kind of name or number tree is being pruned.
#[derive(Clone, Copy)]
enum TreeKind<'k> {
    /// `/Dests`: a value that goes to a removed page goes, and its name goes on the list.
    Destinations,
    /// Values that are references: one to something gone goes.
    References,
    /// `/ParentTree`: also goes by the key.
    ParentTree(&'k HashSet<i64>),
}

/// What's left of a node: whether anything is, and its first and last keys for its parent's `/Limits`.
struct TreeOutcome {
    emptied: bool,
    first: Option<Object>,
    last: Option<Object>,
}

struct OutlineNode {
    number: u32,
    parent: Option<u32>,
    previous: Option<u32>,
    next: Option<u32>,
    first: Option<u32>,
    last: Option<u32>,
    count: Option<i64>,
    has_target: bool,
    target_gone: bool,
    children: Vec<u32>,
}

fn reference(dict: &Dictionary, key: &[u8]) -> Option<u32> {
    match dict.get(key) {
        Ok(Object::Reference((number, _))) => Some(*number),
        _ => None,
    }
}

impl Plan<'_> {
    pub(super) fn plan_page_removal(&mut self, indices: &[usize]) -> RewriteResult<PageRemoval> {
        let map = self.view.page_map_with_container_budget(RETAINED_CONTAINER_BYTES)?;
        let pages = map.len();
        if let Some(index) = indices.iter().find(|index| **index >= pages) {
            return Err(RewriteError::PageOutOfRange { index: *index, pages });
        }
        for index in indices {
            let id = map.pages[*index].id;
            if self.dead.set(id.0) {
                self.report.pages.push(id);
            }
        }
        let kept: Vec<&PageMapEntry> = map.pages.iter().filter(|entry| !self.dead.get(entry.id.0)).collect();
        if kept.is_empty() {
            return Err(RewriteError::NoPagesLeft);
        }
        let mut catalog = self.peek_dict(self.root)?.ok_or(RewriteError::InvalidRoot)?;

        self.prune_destinations(&mut catalog)?;

        // Kept pages first: what they list can't die with a removed page, and what they inherit from stays.
        let capacity = self.next_number.min(MAX_OBJECT_NUMBER).saturating_add(1);
        let mut kept_annotations = Bits::new(capacity);
        let mut held_up = Bits::new(capacity);
        let mut dead_links = Vec::new();
        for entry in &kept {
            let Some(page) = self.peek_dict(entry.id.0)? else {
                continue;
            };
            let mut parent = reference(&page, b"Parent");
            while let Some(number) = parent {
                if !held_up.set(number) {
                    break;
                }
                parent = self.peek_dict(number)?.and_then(|dict| reference(&dict, b"Parent"));
            }
            for annotation in self.deref_array(page.get(b"Annots").ok())? {
                let Object::Reference((number, _)) = annotation else {
                    continue;
                };
                kept_annotations.set(number);
                if let Some(dict) = self.peek_dict(number)?
                    && matches!(dict.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Link")
                    && self.target_is_gone(&dict)?
                {
                    dead_links.push(number);
                }
            }
        }
        for number in dead_links {
            if self.dead.set(number) {
                self.report.links += 1;
            }
        }

        // Removed pages: their annotations, and the numbers they hold in the structure tree.
        let mut parent_keys = HashSet::new();
        let mut shared = SharedResources::default();
        let removed: Vec<crate::ObjectId> = self.report.pages.clone();
        for id in &removed {
            let Some(page) = self.peek_dict(id.0)? else {
                continue;
            };
            if let Ok(Object::Integer(key)) = page.get(b"StructParents") {
                parent_keys.insert(*key);
            }
            for annotation in self.deref_array(page.get(b"Annots").ok())? {
                let Object::Reference((number, _)) = annotation else {
                    continue;
                };
                if kept_annotations.get(number) {
                    continue;
                }
                if let Some(dict) = self.peek_dict(number)?
                    && let Ok(Object::Integer(key)) = dict.get(b"StructParent")
                {
                    parent_keys.insert(*key);
                }
                if self.dead.set(number) {
                    self.report.annotations += 1;
                }
            }
        }
        for entry in &map.pages {
            if self.dead.get(entry.id.0)
                && let Some(owner) = entry.inherited.resources
            {
                self.note_removed_resources(owner, &mut shared)?;
            }
        }

        self.prune_page_tree(&catalog, &held_up)?;
        self.prune_outlines(&mut catalog)?;
        self.prune_form(&mut catalog, &kept_annotations)?;
        self.prune_structure(&mut catalog, &parent_keys)?;
        self.rebuild_page_labels(&mut catalog, &map)?;
        self.prune_threads(&mut catalog)?;

        let kept_count = kept.len();
        shared.remember_kept_pages(&kept);
        self.edits.insert(self.root, Loaded::Plain(Object::Dictionary(catalog)));
        Ok(PageRemoval { kept_count, shared })
    }

    /// Whether a destination ends at a removed page. A destination that names nothing known isn't this.
    fn destination_is_gone(&mut self, value: &Object, depth: usize) -> RewriteResult<bool> {
        Ok(match value {
            Object::Array(items) => {
                matches!(items.first(), Some(Object::Reference((number, _))) if self.dead.get(*number))
            }
            Object::Reference(_) if depth < 8 => match self.deref(value)? {
                Some(target) => self.destination_is_gone(&target, depth + 1)?,
                None => false,
            },
            Object::Dictionary(dict) if depth < 8 => match dict.get(b"D") {
                Ok(target) => self.destination_is_gone(&target.clone(), depth + 1)?,
                Err(_) => false,
            },
            Object::String(name, _) | Object::Name(name) => self.dead_names.contains(name),
            _ => false,
        })
    }

    /// Whether a link or a bookmark goes to a removed page, directly or by a `/GoTo`.
    fn target_is_gone(&mut self, dict: &Dictionary) -> RewriteResult<bool> {
        if let Ok(destination) = dict.get(b"Dest")
            && self.destination_is_gone(&destination.clone(), 0)?
        {
            return Ok(true);
        }
        let action = self.deref_dict(dict.get(b"A").ok())?;
        if let Some(action) = action
            && matches!(action.get(b"S"), Ok(Object::Name(name)) if name == b"GoTo")
            && let Ok(destination) = action.get(b"D")
        {
            return self.destination_is_gone(&destination.clone(), 0);
        }
        Ok(false)
    }

    fn prune_destinations(&mut self, catalog: &mut Dictionary) -> RewriteResult<()> {
        // The old dictionary of names.
        if let Ok(value) = catalog.get(b"Dests").cloned() {
            let holder = reference(catalog, b"Dests");
            if let Some(mut dests) = self.deref_dict(Some(&value))? {
                let mut gone = Vec::new();
                let entries: Vec<(Vec<u8>, Object)> =
                    dests.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
                for (name, value) in entries {
                    if self.destination_is_gone(&value, 0)? {
                        gone.push(name);
                    }
                }
                if !gone.is_empty() {
                    for name in gone {
                        dests.remove(&name);
                        self.report.named_destinations += 1;
                        self.dead_names.insert(name);
                    }
                    if dests.is_empty() {
                        catalog.remove(b"Dests");
                        if let Some(number) = holder {
                            self.dead.set(number);
                        }
                    } else {
                        match holder {
                            Some(number) => {
                                self.edits.insert(number, Loaded::Plain(Object::Dictionary(dests)));
                            }
                            None => catalog.set("Dests", Object::Dictionary(dests)),
                        }
                    }
                }
            }
        }
        // The name trees.
        let Ok(names_value) = catalog.get(b"Names").cloned() else {
            return Ok(());
        };
        let holder = reference(catalog, b"Names");
        let Some(mut names) = self.deref_dict(Some(&names_value))? else {
            return Ok(());
        };
        let mut changed = false;
        for (key, kind) in [
            (&b"Dests"[..], TreeKind::Destinations),
            (&b"Pages"[..], TreeKind::References),
            (&b"Templates"[..], TreeKind::References),
        ] {
            let Ok(mut tree) = names.get(key).cloned() else {
                continue;
            };
            let mut seen = HashSet::new();
            let was_direct = matches!(tree, Object::Dictionary(_));
            let outcome = self.prune_tree(&mut tree, b"Names", kind, 0, &mut seen)?;
            if outcome.emptied {
                names.remove(key);
                changed = true;
            } else if was_direct {
                names.set(key.to_vec(), tree);
                changed = true;
            }
        }
        if changed {
            if names.is_empty() {
                catalog.remove(b"Names");
                if let Some(number) = holder {
                    self.dead.set(number);
                }
            } else {
                match holder {
                    Some(number) => {
                        self.edits.insert(number, Loaded::Plain(Object::Dictionary(names)));
                    }
                    None => catalog.set("Names", Object::Dictionary(names)),
                }
            }
        }
        Ok(())
    }

    fn tree_keeps(&mut self, kind: TreeKind<'_>, key: &Object, value: &Object) -> RewriteResult<bool> {
        Ok(match kind {
            TreeKind::Destinations => {
                let gone = self.destination_is_gone(value, 0)?;
                if gone {
                    self.report.named_destinations += 1;
                    if let Object::String(name, _) = key {
                        self.dead_names.insert(name.clone());
                    }
                }
                !gone
            }
            TreeKind::References => !matches!(value, Object::Reference((number, _)) if self.dead.get(*number)),
            TreeKind::ParentTree(keys) => {
                let gone = matches!(key, Object::Integer(key) if keys.contains(key))
                    || matches!(value, Object::Reference((number, _)) if self.dead.get(*number));
                if gone {
                    self.report.parent_tree_entries += 1;
                }
                !gone
            }
        })
    }

    /// Prune one node of a name or number tree, in place if it's direct, by replacing it if it's not. Only what changed is touched.
    fn prune_tree(
        &mut self, value: &mut Object, pairs_key: &[u8], kind: TreeKind<'_>, depth: usize, seen: &mut HashSet<u32>,
    ) -> RewriteResult<TreeOutcome> {
        let untouched = TreeOutcome {
            emptied: false,
            first: None,
            last: None,
        };
        if depth > MAX_TREE_DEPTH {
            return Ok(untouched);
        }
        let number = match value {
            Object::Reference((number, _)) => {
                if !seen.insert(*number) {
                    return Ok(untouched);
                }
                Some(*number)
            }
            _ => None,
        };
        let Some(mut node) = self.deref_dict(Some(value))? else {
            return Ok(untouched);
        };
        let mut changed = false;
        let mut emptied = false;
        let mut first = None;
        let mut last = None;

        if let Ok(pairs_value) = node.get(pairs_key).cloned() {
            let pairs = self.deref_array(Some(&pairs_value))?;
            let was_empty = pairs.is_empty();
            let mut kept = Vec::with_capacity(pairs.len());
            let mut items = pairs.into_iter();
            while let (Some(key), Some(pair_value)) = (items.next(), items.next()) {
                if self.tree_keeps(kind, &key, &pair_value)? {
                    kept.push(key);
                    kept.push(pair_value);
                } else {
                    changed = true;
                }
            }
            first = kept.first().cloned();
            last = kept.len().checked_sub(2).and_then(|at| kept.get(at)).cloned();
            if changed {
                emptied = kept.is_empty() && !was_empty;
                node.set(pairs_key.to_vec(), Object::Array(kept));
            }
        }
        if let Ok(kids_value) = node.get(b"Kids").cloned() {
            let kids = self.deref_array(Some(&kids_value))?;
            let had_kids = !kids.is_empty();
            let mut kept = Vec::with_capacity(kids.len());
            let mut kid_changed = false;
            for mut kid in kids {
                let outcome = self.prune_tree(&mut kid, pairs_key, kind, depth + 1, seen)?;
                if outcome.emptied {
                    kid_changed = true;
                    continue;
                }
                if first.is_none() {
                    first = outcome.first.clone();
                }
                if outcome.last.is_some() {
                    last = outcome.last.clone();
                }
                kept.push(kid);
            }
            if kid_changed {
                changed = true;
                emptied = kept.is_empty() && had_kids;
                node.set("Kids", Object::Array(kept));
            }
        }
        // A limit is a key, and the keys that are gone may have been ones to hide. So limits that are stale go, however harmless they'd be.
        if node.has(b"Limits")
            && let (Some(first), Some(last)) = (&first, &last)
        {
            let limits = Object::Array(vec![first.clone(), last.clone()]);
            if node.get(b"Limits").ok() != Some(&limits) {
                node.set("Limits", limits);
                changed = true;
            }
        }
        if emptied {
            if let Some(number) = number {
                self.dead.set(number);
            }
        } else if changed {
            match number {
                Some(number) => {
                    self.edits.insert(number, Loaded::Plain(Object::Dictionary(node)));
                }
                None => *value = Object::Dictionary(node),
            }
        }
        Ok(TreeOutcome { emptied, first, last })
    }

    /// Correct `/Count` up the tree, and mark nodes with nothing left dead. With a stack of its own, since a tree can be as deep as a file
    /// has objects to spend on it, and the reader takes it up to its limit however small this thread's stack is.
    fn prune_page_tree(&mut self, catalog: &Dictionary, held_up: &Bits) -> RewriteResult<()> {
        struct Frame {
            number: u32,
            node: Dictionary,
            kids: Vec<Object>,
            count: i64,
        }
        let Some(root) = reference(catalog, b"Pages") else {
            return Ok(());
        };
        let limit = self.view.options.page_tree_depth;
        let mut known: HashMap<u32, i64> = HashMap::new();
        let mut on_path = HashSet::new();
        let mut stack: Vec<Frame> = Vec::new();
        let mut next = Some(root);
        loop {
            // Enter the next node, or count it as it is if it's a leaf or already known.
            let mut finished = None;
            if let Some(number) = next.take() {
                if self.dead.get(number) || stack.len() > limit || on_path.contains(&number) {
                    finished = Some(0);
                } else if let Some(count) = known.get(&number) {
                    finished = Some(*count);
                } else {
                    match self.peek_dict(number)? {
                        None => finished = Some(0),
                        Some(node) if !node.has_type(b"Pages") && !(node.has(b"Kids") && !node.has_type(b"Page")) => {
                            // A page, or something with no business in a page tree.
                            finished = Some(i64::from(node.has_type(b"Page") || !node.has(b"Type")));
                        }
                        Some(node) => {
                            let mut kids = self.deref_array(node.get(b"Kids").ok())?;
                            kids.reverse();
                            on_path.insert(number);
                            stack.push(Frame {
                                number,
                                node,
                                kids,
                                count: 0,
                            });
                        }
                    }
                }
            }
            if let (Some(count), Some(parent)) = (finished, stack.last_mut()) {
                parent.count += count;
            } else if finished.is_some() {
                return Ok(());
            }
            let Some(frame) = stack.last_mut() else {
                return Ok(());
            };
            if let Some(kid) = frame.kids.pop() {
                if let Object::Reference((kid, _)) = kid {
                    next = Some(kid);
                }
                continue;
            }
            let Some(mut frame) = stack.pop() else {
                return Ok(());
            };
            on_path.remove(&frame.number);
            known.insert(frame.number, frame.count);
            if frame.count == 0 && frame.number != root && !held_up.get(frame.number) {
                self.dead.set(frame.number);
                self.report.page_tree_nodes += 1;
            } else if !matches!(frame.node.get(b"Count"), Ok(Object::Integer(stated)) if *stated == frame.count) {
                frame.node.set("Count", frame.count);
                self.store_edit(frame.number, frame.node)?;
            }
            match stack.last_mut() {
                Some(parent) => parent.count += frame.count,
                None => return Ok(()),
            }
        }
    }

    /// Keep a changed dictionary in place of an object, as a stream if it is one.
    pub(super) fn store_edit(&mut self, number: u32, dict: Dictionary) -> RewriteResult<()> {
        let edit = match self.load(number) {
            Ok(Some(Loaded::Stream(_, body))) => Loaded::Stream(dict, body),
            _ => Loaded::Plain(Object::Dictionary(dict)),
        };
        self.edits.insert(number, edit);
        Ok(())
    }

    fn prune_outlines(&mut self, catalog: &mut Dictionary) -> RewriteResult<()> {
        let Some(root) = reference(catalog, b"Outlines") else {
            return Ok(());
        };
        let Some(root_dict) = self.peek_dict(root)? else {
            return Ok(());
        };
        let mut arena = vec![self.outline_node(root, &root_dict)?];
        let mut stack = vec![0_u32];
        let mut seen = HashSet::new();
        seen.insert(root);
        while let Some(at) = stack.pop() {
            let mut current = arena[at as usize].first;
            let mut children = Vec::new();
            while let Some(number) = current {
                if !seen.insert(number) {
                    break;
                }
                if arena.len() as u64 >= MAX_OUTLINE_ITEMS {
                    return Err(RewriteError::StructureTooLarge {
                        what: "outline",
                        limit: MAX_OUTLINE_ITEMS,
                    });
                }
                let Some(dict) = self.peek_dict(number)? else {
                    break;
                };
                let node = self.outline_node(number, &dict)?;
                current = node.next;
                let child = u32::try_from(arena.len()).unwrap_or(u32::MAX);
                arena.push(node);
                children.push(child);
                stack.push(child);
            }
            arena[at as usize].children = children;
        }
        if arena.iter().all(|node| !node.target_gone) {
            return Ok(());
        }

        // A child is always after its parent, so going backwards settles children first.
        let mut placed: Vec<Vec<u32>> = vec![Vec::new(); arena.len()];
        let mut stays = vec![false; arena.len()];
        let mut promoted = vec![false; arena.len()];
        for at in (0..arena.len()).rev() {
            let mut children = Vec::new();
            for child in &arena[at].children {
                let child = *child as usize;
                if stays[child] {
                    children.push(child as u32);
                } else {
                    children.append(&mut placed[child]);
                }
            }
            let node = &arena[at];
            let lost_everything = !node.children.is_empty() && children.is_empty() && !node.has_target;
            stays[at] = at == 0 || !(node.target_gone || lost_everything);
            if !stays[at] {
                for child in &children {
                    promoted[*child as usize] = true;
                }
            }
            placed[at] = children;
        }
        let mut visible = vec![0_i64; arena.len()];
        for at in (0..arena.len()).rev() {
            if stays[at] {
                let mut total = 0;
                for child in &placed[at] {
                    let child = *child as usize;
                    let open = arena[child].count.is_some_and(|count| count > 0);
                    total += 1 + if open { visible[child] } else { 0 };
                }
                visible[at] = total;
            }
        }
        for at in 0..arena.len() {
            if !stays[at] {
                self.dead.set(arena[at].number);
                self.report.outline_items += 1;
            }
            if promoted[at] && stays[at] {
                self.report.outline_items_promoted += 1;
            }
        }
        if placed[0].is_empty() {
            catalog.remove(b"Outlines");
            self.dead.set(root);
            return Ok(());
        }
        for at in 0..arena.len() {
            if !stays[at] {
                continue;
            }
            let children = placed[at].clone();
            let parent_number = arena[at].number;
            let want_first = children.first().map(|child| arena[*child as usize].number);
            let want_last = children.last().map(|child| arena[*child as usize].number);
            let want_count = match arena[at].count {
                _ if children.is_empty() => None,
                Some(count) if count < 0 => Some(-visible[at]),
                None => None,
                Some(_) => Some(visible[at]),
            };
            if arena[at].first != want_first || arena[at].last != want_last || arena[at].count != want_count {
                self.edit_outline_links(parent_number, |dict| {
                    set_link(dict, b"First", want_first);
                    set_link(dict, b"Last", want_last);
                    match want_count {
                        Some(count) => dict.set("Count", count),
                        None => {
                            dict.remove(b"Count");
                        }
                    }
                })?;
            }
            for (position, child) in children.iter().enumerate() {
                let child = *child as usize;
                let want_previous = position.checked_sub(1).map(|at| arena[children[at] as usize].number);
                let want_next = children.get(position + 1).map(|at| arena[*at as usize].number);
                let node = &arena[child];
                if node.parent != Some(parent_number) || node.previous != want_previous || node.next != want_next {
                    let number = node.number;
                    self.edit_outline_links(number, |dict| {
                        set_link(dict, b"Parent", Some(parent_number));
                        set_link(dict, b"Prev", want_previous);
                        set_link(dict, b"Next", want_next);
                    })?;
                }
            }
        }
        Ok(())
    }

    fn edit_outline_links(&mut self, number: u32, change: impl FnOnce(&mut Dictionary)) -> RewriteResult<()> {
        if let Some(mut dict) = self.peek_dict(number)? {
            change(&mut dict);
            self.store_edit(number, dict)?;
        }
        Ok(())
    }

    fn outline_node(&mut self, number: u32, dict: &Dictionary) -> RewriteResult<OutlineNode> {
        Ok(OutlineNode {
            number,
            parent: reference(dict, b"Parent"),
            previous: reference(dict, b"Prev"),
            next: reference(dict, b"Next"),
            first: reference(dict, b"First"),
            last: reference(dict, b"Last"),
            count: dict.get(b"Count").and_then(Object::as_i64).ok(),
            has_target: dict.has(b"Dest") || dict.has(b"A"),
            target_gone: self.target_is_gone(dict)?,
            children: Vec::new(),
        })
    }

    fn prune_form(&mut self, catalog: &mut Dictionary, kept_annotations: &Bits) -> RewriteResult<()> {
        let holder = reference(catalog, b"AcroForm");
        let Some(mut form) = self.deref_dict(catalog.get(b"AcroForm").ok())? else {
            return Ok(());
        };
        let fields = self.deref_array(form.get(b"Fields").ok())?;
        let had_fields = !fields.is_empty();
        let mut seen = HashSet::new();
        let mut any_stays = false;
        for field in fields {
            if let Object::Reference((number, _)) = field {
                any_stays |= self.field_stays(number, kept_annotations, 0, &mut seen)?;
            } else {
                any_stays = true;
            }
        }
        // A whole second copy of the form and everything typed into it, which can't be taken apart by page.
        let had_xfa = form.remove(b"XFA").is_some();
        self.report.xfa_dropped |= had_xfa;
        if had_fields && !any_stays {
            catalog.remove(b"AcroForm");
            if let Some(number) = holder {
                self.dead.set(number);
            }
        } else if had_xfa {
            match holder {
                Some(number) => self.store_edit(number, form)?,
                None => catalog.set("AcroForm", Object::Dictionary(form)),
            }
        }
        Ok(())
    }

    fn field_stays(
        &mut self, number: u32, kept_annotations: &Bits, depth: usize, seen: &mut HashSet<u32>,
    ) -> RewriteResult<bool> {
        if self.dead.get(number) {
            self.report.form_fields += 1;
            return Ok(false);
        }
        if depth > MAX_TREE_DEPTH || !seen.insert(number) {
            return Ok(true);
        }
        let Some(field) = self.peek_dict(number)? else {
            return Ok(true);
        };
        let kids = self.deref_array(field.get(b"Kids").ok())?;
        let stays = if kids.is_empty() {
            // A widget a removed page holds by /P alone, whichever /Annots it's missing from.
            !(matches!(field.get(b"P"), Ok(Object::Reference((page, _))) if self.dead.get(*page))
                && !kept_annotations.get(number))
        } else {
            let mut any = false;
            for kid in kids {
                if let Object::Reference((kid, _)) = kid {
                    any |= self.field_stays(kid, kept_annotations, depth + 1, seen)?;
                } else {
                    any = true;
                }
            }
            any
        };
        if !stays && self.dead.set(number) {
            self.report.form_fields += 1;
        }
        Ok(stays)
    }

    fn prune_structure(&mut self, catalog: &mut Dictionary, parent_keys: &HashSet<i64>) -> RewriteResult<()> {
        let holder = reference(catalog, b"StructTreeRoot");
        let Some(mut root) = self.deref_dict(catalog.get(b"StructTreeRoot").ok())? else {
            return Ok(());
        };
        let kids = match root.get(b"K") {
            Ok(kids) => self.deref(&kids.clone())?,
            Err(_) => None,
        };
        let had_kids = !matches!(&kids, None | Some(Object::Null))
            && !matches!(&kids, Some(Object::Array(items)) if items.is_empty());
        let any_stays = match kids {
            Some(kids) => self.structure_alive(kids)?,
            None => false,
        };
        if had_kids && !any_stays {
            catalog.remove(b"StructTreeRoot");
            if let Some(number) = holder {
                self.dead.set(number);
            }
            return Ok(());
        }
        let mut changed = false;
        for (key, pairs_key, kind) in [
            (&b"ParentTree"[..], &b"Nums"[..], TreeKind::ParentTree(parent_keys)),
            (&b"IDTree"[..], &b"Names"[..], TreeKind::References),
        ] {
            let Ok(mut tree) = root.get(key).cloned() else {
                continue;
            };
            let mut seen = HashSet::new();
            let was_direct = matches!(tree, Object::Dictionary(_));
            let outcome = self.prune_tree(&mut tree, pairs_key, kind, 0, &mut seen)?;
            if outcome.emptied {
                root.remove(key);
                changed = true;
            } else if was_direct {
                root.set(key.to_vec(), tree);
                changed = true;
            }
        }
        if changed {
            match holder {
                Some(number) => self.store_edit(number, root)?,
                None => catalog.set("StructTreeRoot", Object::Dictionary(root)),
            }
        }
        Ok(())
    }

    /// Whether anything under these structure kids is on a kept page, marking every element with nothing left dead. Kids are a marked-content
    /// number, a reference to marked content or an annotation, an element, or a list of those. `page_gone` is about the element's own
    /// `/Pg`, which is what a plain number and a reference without its own `/Pg` are on. With a stack of its own: a structure tree is as deep as
    /// the file likes, and a recursive walk dies on a small hostile file.
    fn structure_alive(&mut self, top: Object) -> RewriteResult<bool> {
        struct Frame {
            number: Option<u32>,
            items: Vec<Object>,
            page_gone: bool,
            any: bool,
        }
        let capacity = self.next_number.min(MAX_OBJECT_NUMBER).saturating_add(1);
        let mut seen = Bits::new(capacity);
        let mut stack = vec![Frame {
            number: None,
            items: vec![top],
            page_gone: false,
            any: false,
        }];
        loop {
            let Some(frame) = stack.last_mut() else {
                return Ok(false);
            };
            let Some(item) = frame.items.pop() else {
                let Some(done) = stack.pop() else {
                    return Ok(false);
                };
                if let Some(number) = done.number
                    && !done.any
                    && self.dead.set(number)
                {
                    self.report.structure_elements += 1;
                }
                match stack.last_mut() {
                    Some(parent) => parent.any |= done.any,
                    None => return Ok(done.any),
                }
                continue;
            };
            let page_gone = frame.page_gone;
            let (number, element) = match item {
                Object::Integer(_) => {
                    frame.any |= !page_gone;
                    continue;
                }
                Object::Array(items) => {
                    stack.push(Frame {
                        number: None,
                        items,
                        page_gone,
                        any: false,
                    });
                    continue;
                }
                Object::Reference((number, _)) => {
                    if self.dead.get(number) {
                        continue;
                    }
                    if !seen.set(number) {
                        frame.any = true;
                        continue;
                    }
                    match self.peek_dict(number)? {
                        Some(dict) => (Some(number), dict),
                        None => continue,
                    }
                }
                Object::Dictionary(dict) => (None, dict),
                _ => {
                    frame.any = true;
                    continue;
                }
            };
            if element.has_type(b"MCR") || element.has_type(b"OBJR") {
                let dead_reference = |key: &[u8]| matches!(element.get(key), Ok(Object::Reference((number, _))) if self.dead.get(*number));
                let alive = !(dead_reference(b"Pg") || dead_reference(b"Obj") || (page_gone && !element.has(b"Pg")));
                if let Some(frame) = stack.last_mut() {
                    frame.any |= alive;
                }
                continue;
            }
            let own_page_gone = matches!(element.get(b"Pg"), Ok(Object::Reference((page, _))) if self.dead.get(*page));
            let kids = match element.get(b"K") {
                Ok(kids) => self.deref(&kids.clone())?,
                Err(_) => None,
            };
            let items = match kids {
                None | Some(Object::Null) => Vec::new(),
                Some(Object::Array(items)) => items,
                Some(kid) => vec![kid],
            };
            stack.push(Frame {
                number,
                any: items.is_empty() && !own_page_gone,
                items,
                page_gone: own_page_gone,
            });
        }
    }

    fn rebuild_page_labels(&mut self, catalog: &mut Dictionary, map: &PageMap) -> RewriteResult<()> {
        let Ok(labels) = catalog.get(b"PageLabels").cloned() else {
            return Ok(());
        };
        let mut ranges = Vec::new();
        let mut seen = HashSet::new();
        self.collect_label_ranges(&labels, 0, &mut seen, &mut ranges)?;
        ranges.sort_by_key(|(start, _)| *start);
        let mut nums = Vec::new();
        let mut previous: Option<(usize, usize)> = None;
        let mut new_index = 0_i64;
        for (old_index, entry) in map.pages.iter().enumerate() {
            if self.dead.get(entry.id.0) {
                continue;
            }
            let position = i64::try_from(old_index).unwrap_or(i64::MAX);
            let range_at = ranges.partition_point(|(start, _)| *start <= position);
            let continues = previous.is_some_and(|(previous_old, previous_range)| {
                previous_old + 1 == old_index && previous_range == range_at
            });
            if !continues {
                let mut label = Dictionary::new();
                if let Some((start, dict)) = range_at.checked_sub(1).and_then(|at| ranges.get(at)) {
                    for key in [&b"S"[..], &b"P"[..]] {
                        if let Ok(value) = dict.get(key) {
                            label.set(key.to_vec(), value.clone());
                        }
                    }
                    let first = dict.get(b"St").and_then(Object::as_i64).unwrap_or(1);
                    let number = first.saturating_add(position - start);
                    if number != 1 {
                        label.set("St", number);
                    }
                } else {
                    // Before any range: a viewer shows the position, plainly numbered.
                    label.set("S", Object::Name(b"D".to_vec()));
                    label.set("St", position.saturating_add(1));
                }
                nums.push(Object::Integer(new_index));
                nums.push(Object::Dictionary(label));
            }
            previous = Some((old_index, range_at));
            new_index += 1;
        }
        let mut tree = Dictionary::new();
        tree.set("Nums", Object::Array(nums));
        catalog.set("PageLabels", Object::Dictionary(tree));
        self.report.page_labels_rebuilt = true;
        Ok(())
    }

    fn collect_label_ranges(
        &mut self, node: &Object, depth: usize, seen: &mut HashSet<u32>, ranges: &mut Vec<(i64, Dictionary)>,
    ) -> RewriteResult<()> {
        if depth > MAX_TREE_DEPTH {
            return Ok(());
        }
        if let Object::Reference((number, _)) = node
            && !seen.insert(*number)
        {
            return Ok(());
        }
        let Some(node) = self.deref_dict(Some(node))? else {
            return Ok(());
        };
        let pairs = self.deref_array(node.get(b"Nums").ok())?;
        let mut items = pairs.into_iter();
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            if ranges.len() as u64 >= MAX_PAGE_LABEL_RANGES {
                return Err(RewriteError::StructureTooLarge {
                    what: "/PageLabels",
                    limit: MAX_PAGE_LABEL_RANGES,
                });
            }
            if let (Ok(key), Some(value)) = (key.as_i64(), self.deref_dict(Some(&value))?) {
                ranges.push((key, value));
            }
        }
        for kid in self.deref_array(node.get(b"Kids").ok())? {
            self.collect_label_ranges(&kid, depth + 1, seen, ranges)?;
        }
        Ok(())
    }

    fn prune_threads(&mut self, catalog: &mut Dictionary) -> RewriteResult<()> {
        let holder = reference(catalog, b"Threads");
        let Ok(value) = catalog.get(b"Threads").cloned() else {
            return Ok(());
        };
        let threads = self.deref_array(Some(&value))?;
        let had_threads = !threads.is_empty();
        let mut kept = 0;
        for thread in threads {
            let Object::Reference((thread, _)) = thread else {
                kept += 1;
                continue;
            };
            if self.prune_thread(thread)? {
                kept += 1;
            }
        }
        if had_threads && kept == 0 {
            catalog.remove(b"Threads");
            if let Some(number) = holder {
                self.dead.set(number);
            }
        }
        Ok(())
    }

    /// Take beads on removed pages out of one thread's ring. False if the thread is left with none.
    fn prune_thread(&mut self, thread: u32) -> RewriteResult<bool> {
        let Some(mut thread_dict) = self.peek_dict(thread)? else {
            return Ok(true);
        };
        let Some(first) = reference(&thread_dict, b"F") else {
            return Ok(true);
        };
        let mut ring = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(first);
        while let Some(number) = current {
            if !seen.insert(number) || ring.len() >= MAX_THREAD_BEADS {
                break;
            }
            let Some(bead) = self.peek_dict(number)? else {
                break;
            };
            let on_gone_page = matches!(bead.get(b"P"), Ok(Object::Reference((page, _))) if self.dead.get(*page));
            current = reference(&bead, b"N");
            ring.push((number, on_gone_page));
        }
        if ring.iter().all(|(_, gone)| !gone) {
            return Ok(true);
        }
        let kept: Vec<u32> = ring
            .iter()
            .filter(|(_, gone)| !gone)
            .map(|(number, _)| *number)
            .collect();
        for (number, gone) in &ring {
            if *gone && self.dead.set(*number) {
                self.report.thread_beads += 1;
            }
        }
        if kept.is_empty() {
            self.dead.set(thread);
            self.report.threads += 1;
            return Ok(false);
        }
        for (position, number) in kept.iter().enumerate() {
            let next = kept[(position + 1) % kept.len()];
            let previous = kept[(position + kept.len() - 1) % kept.len()];
            if let Some(mut bead) = self.peek_dict(*number)? {
                set_link(&mut bead, b"N", Some(next));
                set_link(&mut bead, b"V", Some(previous));
                if position == 0 {
                    set_link(&mut bead, b"T", Some(thread));
                }
                self.store_edit(*number, bead)?;
            }
        }
        if kept[0] != first {
            set_link(&mut thread_dict, b"F", Some(kept[0]));
            self.store_edit(thread, thread_dict)?;
        }
        Ok(true)
    }
}

fn set_link(dict: &mut Dictionary, key: &[u8], number: Option<u32>) {
    match number {
        Some(number) => dict.set(key.to_vec(), Object::Reference((number, 0))),
        None => {
            dict.remove(key);
        }
    }
}
