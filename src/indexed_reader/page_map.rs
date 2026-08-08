//! Bounded page-tree walk and the page map it produces.

use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InheritedPageAttributeOwners {
    pub(super) resources: Option<crate::ObjectId>,
    pub(super) media_box: Option<crate::ObjectId>,
    pub(super) crop_box: Option<crate::ObjectId>,
    pub(super) rotate: Option<crate::ObjectId>,
}

impl InheritedPageAttributeOwners {
    pub fn resources(&self) -> Option<crate::ObjectId> {
        self.resources
    }

    pub fn media_box(&self) -> Option<crate::ObjectId> {
        self.media_box
    }

    pub fn crop_box(&self) -> Option<crate::ObjectId> {
        self.crop_box
    }

    pub fn rotate(&self) -> Option<crate::ObjectId> {
        self.rotate
    }

    fn updated(mut self, owner: crate::ObjectId, dictionary: &Dictionary) -> Self {
        if dictionary.has(b"Resources") {
            self.resources = Some(owner);
        }
        if dictionary.has(b"MediaBox") {
            self.media_box = Some(owner);
        }
        if dictionary.has(b"CropBox") {
            self.crop_box = Some(owner);
        }
        if dictionary.has(b"Rotate") {
            self.rotate = Some(owner);
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageMapEntry {
    pub(super) id: crate::ObjectId,
    pub(super) inherited: InheritedPageAttributeOwners,
}

impl PageMapEntry {
    pub fn id(&self) -> crate::ObjectId {
        self.id
    }

    pub fn inherited(&self) -> &InheritedPageAttributeOwners {
        &self.inherited
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageMap {
    pub(super) pages: Vec<PageMapEntry>,
}

impl PageMap {
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&PageMapEntry> {
        self.pages.get(index)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &PageMapEntry> + DoubleEndedIterator + '_ {
        self.pages.iter()
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PageMapLimits {
    pub(super) max_depth: usize,
    pub(super) max_pages: usize,
}

impl Default for PageMapLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_PAGE_TREE_DEPTH_LIMIT,
            max_pages: DEFAULT_PAGE_COUNT_LIMIT,
        }
    }
}

struct PageMapBuilder<'a> {
    reader: &'a IndexedReader,
    limits: PageMapLimits,
    remaining_work: usize,
    consumed_work: usize,
    pub(super) peak_pending_items: usize,
}

#[derive(Clone)]
pub(super) struct PendingKid {
    pub(super) id: Option<crate::ObjectId>,
    pub(super) inherited: Rc<InheritedPageAttributeOwners>,
    pub(super) depth: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PageMapWork {
    pub(super) consumed: usize,
    pub(super) peak_pending_items: usize,
    pub(super) peak_pending_bytes: usize,
}

impl PageMap {
    pub(super) fn from_reader(reader: &IndexedReader) -> IndexedReaderResult<Self> {
        Self::from_reader_with_limits(
            reader,
            PageMapLimits {
                max_depth: reader.options.page_tree_depth,
                max_pages: reader.options.max_pages,
            },
        )
    }

    pub(super) fn from_reader_with_limits(reader: &IndexedReader, limits: PageMapLimits) -> IndexedReaderResult<Self> {
        Self::from_reader_with_limits_and_work(reader, limits).map(|(page_map, _)| page_map)
    }

    pub(super) fn from_reader_with_limits_and_work(
        reader: &IndexedReader, limits: PageMapLimits,
    ) -> IndexedReaderResult<(Self, usize)> {
        Self::from_reader_with_limits_and_stats(reader, limits).map(|(page_map, work)| (page_map, work.consumed))
    }

    pub(super) fn from_reader_with_limits_and_stats(
        reader: &IndexedReader, limits: PageMapLimits,
    ) -> IndexedReaderResult<(Self, PageMapWork)> {
        let work_budget = reader
            .index
            .locations
            .values()
            .filter(|location| !matches!(location, ObjectLocation64::Free { .. }))
            .count();
        Self::from_reader_with_work_budget_and_stats(reader, limits, work_budget)
    }

    pub(super) fn from_reader_with_work_budget_and_stats(
        reader: &IndexedReader, limits: PageMapLimits, work_budget: usize,
    ) -> IndexedReaderResult<(Self, PageMapWork)> {
        let Some(root_id) = reader
            .index
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| root.as_reference().ok())
        else {
            return Ok((Self::default(), PageMapWork::default()));
        };
        let Some(catalog) = reader.resolve_dictionary_deref(root_id)? else {
            return Ok((Self::default(), PageMapWork::default()));
        };
        let Some(pages_id) = catalog.get(b"Pages").ok().and_then(|pages| pages.as_reference().ok()) else {
            return Ok((Self::default(), PageMapWork::default()));
        };

        let mut page_map = Self::default();
        let mut builder = PageMapBuilder {
            reader,
            limits,
            remaining_work: work_budget,
            consumed_work: 0,
            peak_pending_items: 0,
        };
        builder.walk_page_tree(&mut page_map, pages_id)?;
        Ok((
            page_map,
            PageMapWork {
                consumed: builder.consumed_work,
                peak_pending_items: builder.peak_pending_items,
                peak_pending_bytes: builder
                    .peak_pending_items
                    .saturating_mul(std::mem::size_of::<PendingKid>()),
            },
        ))
    }
}

impl PageMapBuilder<'_> {
    fn walk_page_tree(&mut self, page_map: &mut PageMap, root_id: crate::ObjectId) -> IndexedReaderResult<()> {
        #[cfg(test)]
        PAGE_TREE_WALK_CALLS.with(|calls| calls.set(calls.get() + 1));
        let Some(mut root) = self.reader.resolve_dictionary_deref(root_id)? else {
            return Ok(());
        };
        let inherited = InheritedPageAttributeOwners::default().updated(root_id, &root);
        let kids_value = root.remove(b"Kids");
        // Do not retain the resolved dictionary alongside its potentially wide
        // `/Kids`; only compact pending slots survive into traversal.
        drop(root);
        let Some(kids) = self.reader.resolve_array_value(kids_value)? else {
            return Ok(());
        };
        let mut pending = VecDeque::new();
        self.prepend_kids(&mut pending, kids, Rc::new(inherited), 1);

        while let Some(kid) = pending.pop_front() {
            self.remaining_work -= 1;
            self.consumed_work += 1;

            let Some(id) = kid.id else {
                continue;
            };
            // A tree nested past the cap is a *refusal*, not a truncation. Skipping the
            // subtree here used to hand back a short — often empty — page map with no error,
            // which reads downstream as a successfully opened blank document. Reporting it
            // the way `PageCountLimitExceeded` reports the sibling `max_pages` cap lets the
            // caller fall back to an unbounded walk instead of trusting a partial answer.
            if usize::try_from(kid.depth).unwrap_or(usize::MAX) > self.limits.max_depth {
                return Err(IndexedReaderError::PageTreeDepthLimitExceeded {
                    limit: self.limits.max_depth,
                });
            }
            let Some(mut dictionary) = self.reader.resolve_dictionary_deref(id)? else {
                continue;
            };
            let inherited = (*kid.inherited).updated(id, &dictionary);
            match dictionary.get_type() {
                Ok(b"Page") => {
                    if page_map.pages.len() >= self.limits.max_pages {
                        return Err(IndexedReaderError::PageCountLimitExceeded {
                            limit: self.limits.max_pages,
                        });
                    }
                    page_map.pages.push(PageMapEntry { id, inherited });
                }
                Ok(b"Pages") => {
                    let kids_value = dictionary.remove(b"Kids");
                    drop(dictionary);
                    if let Some(kids) = self.reader.resolve_array_value(kids_value)? {
                        self.prepend_kids(&mut pending, kids, Rc::new(inherited), kid.depth.saturating_add(1));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn prepend_kids(
        &mut self, pending: &mut VecDeque<PendingKid>, mut kids: Vec<Object>,
        inherited: Rc<InheritedPageAttributeOwners>, depth: u32,
    ) {
        // Only the first `remaining_work` DFS slots can ever be observed. Drop
        // later siblings before prepending children, then convert every owned
        // Object into a fixed-size slot as it leaves the temporary Kids array.
        kids.truncate(self.remaining_work);
        pending.truncate(self.remaining_work - kids.len());
        for kid in kids.into_iter().rev() {
            pending.push_front(PendingKid {
                id: kid.as_reference().ok(),
                inherited: Rc::clone(&inherited),
                depth,
            });
        }
        debug_assert!(pending.len() <= self.remaining_work);
        self.peak_pending_items = self.peak_pending_items.max(pending.len());
    }
}

impl IndexedReader {
    /// Derive the actual ordered leaf-page map by walking `/Kids`.
    pub fn page_map(&self) -> IndexedReaderResult<PageMap> {
        PageMap::from_reader(self)
    }

    /// Derive one owned page map and its conservative index residency snapshot.
    ///
    /// The stats are estimated from the returned map's exact allocation, so
    /// callers that need both values perform only one bounded page-tree walk.
    pub fn page_map_with_stats(&self) -> IndexedReaderResult<(PageMap, IndexedReaderIndexStats)> {
        let page_map = self.page_map()?;
        let stats = self.index_stats_for_page_map(&page_map);
        Ok((page_map, stats))
    }
}

impl IndexedReader {
    fn resolve_dictionary_deref(&self, id: crate::ObjectId) -> IndexedReaderResult<Option<Dictionary>> {
        let Some(object) = self.resolve_page_tree_object(id)? else {
            return Ok(None);
        };
        Ok(match self.resolve_deref_value(object)? {
            Some(Object::Dictionary(dictionary)) => Some(dictionary),
            _ => None,
        })
    }

    fn resolve_array_value(&self, value: Option<Object>) -> IndexedReaderResult<Option<Vec<Object>>> {
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(match self.resolve_deref_value(value)? {
            Some(Object::Array(array)) => Some(array),
            _ => None,
        })
    }

    fn resolve_deref_value(&self, mut object: Object) -> IndexedReaderResult<Option<Object>> {
        let mut seen = HashSet::new();
        let mut dereferences = 0;
        while let Object::Reference(id) = object {
            if dereferences >= PAGE_TREE_DEREFERENCE_LIMIT || !seen.insert(id) {
                return Ok(None);
            }
            let Some(resolved) = self.resolve_page_tree_object(id)? else {
                return Ok(None);
            };
            object = resolved;
            dereferences += 1;
        }
        Ok(Some(object))
    }

    fn resolve_page_tree_object(&self, id: crate::ObjectId) -> IndexedReaderResult<Option<Object>> {
        match self.resolve_object(id) {
            Ok(object) => Ok(Some(object)),
            Err(
                IndexedReaderError::MissingNormalObject { .. } | IndexedReaderError::MissingNormalObjectAtXref { .. },
            ) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
