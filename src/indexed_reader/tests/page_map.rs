//! Indexed-reader tests: page_map.

use super::*;

#[test]
fn page_map_propagates_a_direct_malformed_root_object() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Pages 2 0 R",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
    ]);
    let eager = Document::load_mem(&pdf).unwrap();
    assert!(matches!(
        eager.get_object((1, 0)),
        Err(crate::Error::ObjectNotFound((1, 0)))
    ));
    assert!(eager.page_iter().next().is_none());

    let reader = open_reader(&pdf, ResolverLimits::default());
    let direct = reader.resolve_object((1, 0)).unwrap_err();
    assert!(matches!(
        direct,
        IndexedReaderError::InvalidIndirectObject { id: (1, 0), .. }
            | IndexedReaderError::IncompleteObject { id: (1, 0), .. }
    ));
    let page_map = reader.page_map().unwrap_err();
    assert_eq!(page_map.to_string(), direct.to_string());
    assert_page_map_snapshot_error_matches_legacy(&reader);
}

#[test]
fn page_map_uses_physical_kids_order_ignores_count_and_tracks_inheritance() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Pages 2 0 R >>",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Count 999 /Resources 8 0 R /MediaBox [0 0 600 800] /Kids 10 0 R >>",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page /Parent 2 0 R /CropBox [0 0 300 400] >>",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Count 0 /Rotate 90 /Resources << /Nested true >> /Kids [7 0 R] >>",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
        ObjectDef {
            id: 6,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /NotPage >>",
        },
        ObjectDef {
            id: 7,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page /MediaBox [0 0 200 200] >>",
        },
        ObjectDef {
            id: 8,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /RootResource true >>",
        },
        ObjectDef {
            id: 10,
            object_generation: 0,
            xref_generation: 0,
            body: b"[3 0 R << /Type /Page >> 4 0 R 9 0 R 5 1 R 3 0 R 6 0 R]",
        },
    ]);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let page_map = PageMap::from_reader(&reader).unwrap();
    let eager: Vec<_> = Document::load_mem(&pdf).unwrap().page_iter().collect();

    assert_eq!(page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(), eager);
    assert_eq!(eager, vec![(3, 0), (7, 0), (3, 0)]);
    assert_eq!(
        page_map.pages[0].inherited,
        InheritedPageAttributeOwners {
            resources: Some((2, 0)),
            media_box: Some((2, 0)),
            crop_box: Some((3, 0)),
            rotate: None,
        }
    );
    assert_eq!(
        page_map.pages[1].inherited,
        InheritedPageAttributeOwners {
            resources: Some((4, 0)),
            media_box: Some((7, 0)),
            crop_box: None,
            rotate: Some((4, 0)),
        }
    );
    assert_eq!(page_map.pages[2], page_map.pages[0]);
}

#[test]
fn page_map_bounds_cycles_depth_and_page_count_without_trusting_count() {
    let cyclic = cyclic_page_tree_pdf();
    let reader = open_reader(&cyclic, ResolverLimits::default());
    let eager = Document::load_mem(&cyclic).unwrap();
    let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();

    // The fixture's `/Pages` nodes 2 and 3 name each other, and the two walks bound that
    // cycle differently — so both sides are pinned exactly rather than to each other.
    // The eager walk holds the whole document in memory, so it can carry the ancestor set
    // that identifies the back edge, skip it, and reach the pages beside the cycle. The
    // indexed walk streams the tree with no ancestor state and only its work budget,
    // which the cycle consumes before either page is reached. Neither loops; neither
    // trusts `/Count` (which claims 999999).
    assert_eq!(eager.page_iter().collect::<Vec<_>>(), vec![(5, 0), (4, 0)]);
    assert!(page_map.pages.is_empty());
    assert_eq!(work, eager.objects.len());

    assert!(matches!(
        PageMap::from_reader_with_limits(
            &reader,
            PageMapLimits {
                max_depth: 0,
                max_pages: 10,
            },
        ),
        Err(IndexedReaderError::PageTreeDepthLimitExceeded { limit: 0 })
    ));

    let two_pages = generated_page_tree_pdf(2, 1);
    let reader = open_reader(&two_pages, ResolverLimits::default());
    assert!(matches!(
        PageMap::from_reader_with_limits(
            &reader,
            PageMapLimits {
                max_depth: 256,
                max_pages: 1,
            }
        ),
        Err(IndexedReaderError::PageCountLimitExceeded { limit: 1 })
    ));
}

/// The depth cap is inclusive, and crossing it *reports* rather than truncates.
///
/// Returning a short page map for an over-deep tree is indistinguishable from a genuinely
/// empty document, so a caller that only watches for errors renders it blank. The typed
/// error is the whole signal: at the limit the walk succeeds, one past it the caller is
/// told which limit it hit.
#[test]
fn page_map_depth_limit_is_inclusive_and_reported() {
    let at_limit = generated_deep_page_tree_pdf(DEFAULT_PAGE_TREE_DEPTH_LIMIT);
    let reader = open_reader(&at_limit, ResolverLimits::default());
    assert_eq!(PageMap::from_reader(&reader).unwrap().pages.len(), 1);

    let over_limit = generated_deep_page_tree_pdf(DEFAULT_PAGE_TREE_DEPTH_LIMIT + 1);
    let reader = open_reader(&over_limit, ResolverLimits::default());
    assert!(matches!(
        PageMap::from_reader(&reader),
        Err(IndexedReaderError::PageTreeDepthLimitExceeded {
            limit: DEFAULT_PAGE_TREE_DEPTH_LIMIT
        })
    ));
    // The eager walk reads the same file without complaint, so the refusal is the indexed
    // reader's bound and not a property of the document.
    let eager = Document::load_mem(&over_limit).unwrap();
    assert_eq!(eager.page_iter().count(), 1);
}

/// The public `page_tree_depth` option carries the same refusal through
/// [`IndexedReader::page_map`], which is the entry point product code calls.
#[test]
fn public_page_tree_depth_option_reports_an_over_deep_tree() {
    let pdf = generated_deep_page_tree_pdf(4);
    let options = IndexedReaderOptions {
        page_tree_depth: 4,
        ..IndexedReaderOptions::default()
    };
    let reader = IndexedReader::open_with_options(BytesSource::from(pdf.clone()), options.clone()).unwrap();
    assert_eq!(reader.page_map().unwrap().len(), 1);

    let reader = IndexedReader::open_with_options(
        BytesSource::from(pdf),
        IndexedReaderOptions {
            page_tree_depth: 3,
            ..options
        },
    )
    .unwrap();
    assert!(matches!(
        reader.page_map(),
        Err(IndexedReaderError::PageTreeDepthLimitExceeded { limit: 3 })
    ));
}

#[test]
fn repeated_page_dag_uses_eager_global_work_budget() {
    let pdf = repeated_page_dag_pdf(15);
    let eager = Document::load_mem(&pdf).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();
    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        eager_pages
    );
    assert_eq!(eager_pages.len(), 3);
    assert_eq!(work, eager.objects.len());
    assert!(source.requests.lock().unwrap().len() <= (work + 2) * 4);
}

/// Pending traversal metadata stays inside the work budget even when the tree is wide enough
/// to saturate it: the queue is capped by remaining work and each slot is a compact
/// fixed-size record, never a retained `/Kids` array.
///
/// The chain is deliberately *finite* in depth — a self-cycling node reaches the depth cap,
/// which is a refusal (see below), so it can no longer be walked to budget exhaustion. ~70
/// hops of 14k kids already saturate the budget, so an 80-node chain reaches the same peak.
#[test]
fn wide_page_tree_pending_metadata_is_capped_by_near_maximum_work() {
    const NON_REFERENCE_KIDS: usize = 14_000;
    const DISTINCT_NODES: u32 = 80;
    let pdf = wide_page_tree_chain_pdf(DISTINCT_NODES, NON_REFERENCE_KIDS);
    assert!(pdf.len() > 64 * 1_024);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let work_budget = usize::try_from(MAX_XREF_ENTRIES - 1).unwrap();
    let (page_map, work) =
        PageMap::from_reader_with_work_budget_and_stats(&reader, PageMapLimits::default(), work_budget).unwrap();

    assert!(page_map.pages.is_empty());
    assert_eq!(work.consumed, work_budget);
    assert!(work.peak_pending_items <= work_budget);
    assert!(work.peak_pending_items > work_budget * 9 / 10);
    assert_eq!(
        work.peak_pending_bytes,
        work.peak_pending_items * std::mem::size_of::<PendingKid>()
    );
    assert!(std::mem::size_of::<PendingKid>() <= 32);
}

/// A `/Pages` node that references itself re-enters one level deeper on every hop, so the
/// depth cap is what stops it — and stopping now *reports*. Truncating instead handed back an
/// empty page map indistinguishable from a genuinely page-less document.
#[test]
fn wide_self_cycle_is_refused_by_the_depth_limit() {
    const NON_REFERENCE_KIDS: usize = 14_000;
    let pdf = wide_page_tree_pdf(1, NON_REFERENCE_KIDS, true);
    assert!(pdf.len() > 64 * 1_024);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let work_budget = usize::try_from(MAX_XREF_ENTRIES - 1).unwrap();
    assert!(matches!(
        PageMap::from_reader_with_work_budget_and_stats(&reader, PageMapLimits::default(), work_budget),
        Err(IndexedReaderError::PageTreeDepthLimitExceeded {
            limit: DEFAULT_PAGE_TREE_DEPTH_LIMIT
        })
    ));
}

#[test]
fn repeated_distinct_wide_nodes_keep_only_compact_reachable_work() {
    const DISTINCT_NODES: u32 = 24;
    const NON_REFERENCE_KIDS: usize = 14_000;
    let pdf = wide_page_tree_pdf(DISTINCT_NODES, NON_REFERENCE_KIDS, false);
    assert!(pdf.len() > usize::try_from(DISTINCT_NODES).unwrap() * 64 * 1_024);
    let eager = Document::load_mem(&pdf).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    let reader = open_reader(&pdf, ResolverLimits::default());
    let (page_map, work) = PageMap::from_reader_with_limits_and_stats(&reader, PageMapLimits::default()).unwrap();

    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        eager_pages
    );
    assert_eq!(work.consumed, eager.objects.len());
    assert!(work.peak_pending_items <= work.consumed);
    assert!(work.peak_pending_bytes < 64 * 1_024);
    assert_eq!(
        work.peak_pending_bytes,
        work.peak_pending_items * std::mem::size_of::<PendingKid>()
    );
}

#[test]
fn page_map_work_budget_counts_non_reference_kids_like_eager() {
    let pdf = object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Pages 2 0 R >>",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Kids [null 3 0 R 17 (bad)] /Count 1 >>",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
    ]);
    let eager = Document::load_mem(&pdf).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    let reader = open_reader(&pdf, ResolverLimits::default());
    let (page_map, work) = PageMap::from_reader_with_limits_and_work(&reader, PageMapLimits::default()).unwrap();

    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        eager_pages
    );
    assert_eq!(eager_pages, vec![(3, 0)]);
    assert_eq!(work, eager.objects.len());
}

#[test]
fn page_map_propagates_malformed_and_decompression_object_stream_failures() {
    let malformed_members = [
        (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
        (11, b"<< /Type /Pages /Kids [12 0 R]".as_slice()),
        (12, b"<< /Type /Page >>".as_slice()),
    ];
    let (first, malformed_content) = object_stream_content(&malformed_members);
    let malformed = object_stream_fixture(
        &format!("/Type /ObjStm /N 3 /First {first}"),
        &malformed_content,
        &[(10, 0), (11, 1), (12, 2)],
    );
    let eager = Document::load_mem(&malformed.pdf).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    let reader = open_reader(&malformed.pdf, ResolverLimits::default());
    assert!(eager_pages.is_empty());
    assert!(matches!(
        reader.resolve_object((11, 0)),
        Err(IndexedReaderError::ObjectStreamMember { .. })
    ));
    assert!(matches!(
        PageMap::from_reader(&reader),
        Err(IndexedReaderError::ObjectStreamMember { .. })
    ));

    let invalid_filter = object_stream_fixture(
        "/Type /ObjStm /N 1 /First 0 /Filter /ASCII85Decode",
        b"uuuuu",
        &[(10, 0)],
    );
    let eager = Document::load_mem(&invalid_filter.pdf).unwrap();
    assert!(eager.page_iter().next().is_none());
    let reader = open_reader(&invalid_filter.pdf, ResolverLimits::default());
    let direct_error = reader.resolve_object((10, 0)).unwrap_err();
    assert!(
        matches!(
            &direct_error,
            IndexedReaderError::ObjectStreamMember {
                source: crate::Error::Decompress(_),
                ..
            }
        ),
        "unexpected direct error: {direct_error:?}"
    );
    assert!(matches!(
        PageMap::from_reader(&reader),
        Err(IndexedReaderError::ObjectStreamMember {
            source: crate::Error::Decompress(_),
            ..
        })
    ));

    let valid_members = [
        (10, b"<< /Type /Catalog /Pages 11 0 R >>".as_slice()),
        (11, b"<< /Type /Pages /Kids [12 0 R] /Count 1 >>".as_slice()),
        (12, b"<< /Type /Page >>".as_slice()),
    ];
    let (first, decoded) = object_stream_content(&valid_members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < decoded.len());
    let limited = object_stream_fixture(
        &format!("/Type /ObjStm /N 3 /First {first} /Filter /FlateDecode"),
        &compressed,
        &[(10, 0), (11, 1), (12, 2)],
    );
    let limit = compressed.len();
    let reader = open_reader(
        &limited.pdf,
        ResolverLimits {
            max_stream_bytes: u64::try_from(limit).unwrap(),
            ..ResolverLimits::default()
        },
    );
    assert!(matches!(
        PageMap::from_reader(&reader),
        Err(IndexedReaderError::ObjectStreamMember {
            source: crate::Error::Decompress(crate::DecompressError::MemoryLimitExceeded {
                limit: actual
            }),
            ..
        }) if actual == limit
    ));
}

/// A page tree that lives inside one object stream decodes that container once
/// for the whole walk, not once per page node.
///
/// The reuse is a prefetch, so what it may not change is the answer: the map,
/// its order and its inherited owners stay the eager page order, and the walk
/// still consumes one work unit per node. What it does change is the source
/// traffic — before this, every one of the 32 page dictionaries re-read and
/// re-inflated the same container body.
#[test]
fn page_tree_inside_one_object_stream_decodes_the_container_once() {
    const PAGES: u32 = 32;

    let mut bodies: Vec<(u32, Vec<u8>)> = vec![
        (10, b"<< /Type /Catalog /Pages 11 0 R >>".to_vec()),
        (
            11,
            format!(
                "<< /Type /Pages /Count {PAGES} /Resources 9 0 R /Kids [{}] >>",
                (0..PAGES)
                    .map(|page| format!("{} 0 R", 12 + page))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .into_bytes(),
        ),
    ];
    for page in 0..PAGES {
        bodies.push((12 + page, b"<< /Type /Page /MediaBox [0 0 612 792] >>".to_vec()));
    }
    let members: Vec<(u32, &[u8])> = bodies.iter().map(|(id, body)| (*id, body.as_slice())).collect();
    let (first, decoded) = object_stream_content(&members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let compressed = encoder.finish().unwrap();
    let entries: Vec<(u32, u32)> = members
        .iter()
        .enumerate()
        .map(|(index, (id, _))| (*id, u32::try_from(index).unwrap()))
        .collect();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N {} /First {first} /Filter /FlateDecode", members.len()),
        &compressed,
        &entries,
    );

    let eager = Document::load_mem(&fixture.pdf).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    assert_eq!(eager_pages.len(), usize::try_from(PAGES).unwrap());

    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf.clone(),
        requests: Mutex::new(Vec::new()),
    });
    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();

    let (page_map, work) = PageMap::from_reader_with_limits_and_stats(&reader, PageMapLimits::default()).unwrap();
    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        eager_pages
    );
    // Every page inherits `/Resources` from the one `/Pages` node and owns its
    // own `/MediaBox`, so the projection is not vacuously equal either.
    assert!(
        page_map
            .pages
            .iter()
            .all(|page| page.inherited.resources == Some((11, 0)) && page.inherited.media_box == Some(page.id))
    );
    assert_eq!(work.consumed, usize::try_from(PAGES).unwrap());

    // One decode means one pass over the container body. Each source request is
    // bounded, so count the reads that touch the payload rather than assuming a
    // single call covers it: reading it 32 times cannot fit in one body's worth
    // of bytes plus one window.
    let payload_bytes: usize = source
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(offset, _)| *offset >= fixture.container_stream_start)
        .map(|(_, length)| *length)
        .sum();
    assert!(
        payload_bytes < 2 * (compressed.len() + INITIAL_OBJECT_WINDOW as usize),
        "container payload re-read {payload_bytes} bytes for a {} byte body",
        compressed.len()
    );
    // The whole residency the reuse adds is that one decoded image.
    assert_eq!(work.peak_container_bytes, decoded.len());
}

#[test]
fn encrypted_page_map_matches_authenticated_eager_order() {
    let pdf = encrypted_page_tree_pdf();
    assert!(matches!(
        open_encrypted(&pdf, None),
        Err(IndexedReaderError::PasswordRequired)
    ));
    let reader = open_encrypted(&pdf, Some(b"user")).unwrap();
    let page_map = PageMap::from_reader(&reader).unwrap();
    let eager = Document::load_mem_with_options(&pdf, crate::LoadOptions::with_password("user")).unwrap();
    let eager_pages: Vec<_> = eager.page_iter().collect();
    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        eager_pages
    );
    assert_eq!(eager_pages, vec![(3, 0), (4, 0)]);
    assert_eq!(page_map.pages[0].inherited.rotate, Some((2, 0)));
}

#[test]
fn page_map_enumerates_five_thousand_unique_pages() {
    let pdf = generated_page_tree_pdf(5_000, 1);
    let reader = open_reader(&pdf, ResolverLimits::default());
    let page_map = PageMap::from_reader(&reader).unwrap();
    assert_eq!(page_map.pages.len(), 5_000);
    assert_eq!(page_map.pages.first().unwrap().id, (3, 0));
    assert_eq!(page_map.pages.last().unwrap().id, (5_002, 0));
    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<HashSet<_>>().len(),
        5_000
    );
    assert!(
        page_map
            .pages
            .iter()
            .all(|page| { page.inherited.resources == Some((2, 0)) && page.inherited.media_box == Some((2, 0)) })
    );
}

#[test]
fn page_map_propagates_source_and_resource_failures() {
    for mode in [1, 2] {
        let source = Arc::new(SwitchableFailureSource {
            bytes: classic_pdf(),
            mode: AtomicU8::new(0),
        });
        let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
        source.mode.store(mode, Ordering::SeqCst);
        assert!(matches!(
            PageMap::from_reader(&reader),
            Err(IndexedReaderError::Source(_))
        ));
        assert_page_map_snapshot_error_matches_legacy(&reader);
    }

    let reader = IndexedReader::open_with_limits(
        Arc::new(BytesSource::from(classic_pdf())),
        ResolverLimits {
            max_object_bytes: 8,
            ..ResolverLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        PageMap::from_reader(&reader),
        Err(IndexedReaderError::ObjectLimitExceeded { limit: 8, .. })
    ));
    assert_page_map_snapshot_error_matches_legacy(&reader);
}

#[test]
fn page_map_walk_is_bounded_on_a_sparse_hundred_megabyte_source() {
    let pdf = generated_page_tree_pdf(3, 999_999);
    let pdf_source = BytesSource::from(pdf.clone());
    let pdf_len = u64::try_from(pdf.len()).unwrap();
    let xref = read_startxref(&pdf_source, pdf_len).unwrap();
    let len = 100_u64 * 1_024 * 1_024;
    let tail = format!("startxref\n{xref}\n%%EOF\n").into_bytes();
    let tail_offset = len - u64::try_from(tail.len()).unwrap();
    let source = Arc::new(OverlaySource {
        len,
        regions: vec![(0, pdf), (tail_offset, tail)],
        requests: Mutex::new(Vec::new()),
    });

    let reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    let page_map = PageMap::from_reader(&reader).unwrap();
    assert_eq!(
        page_map.pages.iter().map(|page| page.id).collect::<Vec<_>>(),
        vec![(3, 0), (4, 0), (5, 0)]
    );

    let requests = source.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|(_, length)| u64::try_from(*length).unwrap() <= TAIL_SCAN_LIMIT)
    );
    assert!(
        !requests
            .iter()
            .any(|(offset, length)| { *offset == 0 && u64::try_from(*length).unwrap_or(u64::MAX) == len })
    );
    let total: usize = requests.iter().map(|(_, length)| *length).sum();
    assert!(u64::try_from(total).unwrap() < 1_024 * 1_024);
}
