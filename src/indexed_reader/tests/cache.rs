//! Indexed-reader tests: cache.

use super::*;

#[test]
fn shared_scalar_is_uncached_by_default_and_cached_values_promote_when_configured() {
    let source = Arc::new(TracingBytesSource {
        bytes: classic_pdf(),
        requests: Mutex::new(Vec::new()),
    });
    let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    source.requests.lock().unwrap().clear();
    let first = reader.resolve_object_shared((4, 0)).unwrap();
    let first_reads = source.requests.lock().unwrap().len();
    let second = reader.resolve_object_shared((4, 0)).unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(source.requests.lock().unwrap().len() > first_reads);
    assert_eq!(reader.object_cache_stats(), IndexedObjectCacheStats::default());

    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    source.requests.lock().unwrap().clear();
    let cached_first = reader.resolve_object_shared((4, 0)).unwrap();
    let cached_reads = source.requests.lock().unwrap().len();
    let cached_second = reader.resolve_object_shared((4, 0)).unwrap();
    assert!(Arc::ptr_eq(&cached_first, &cached_second));
    assert_eq!(source.requests.lock().unwrap().len(), cached_reads);
    assert_eq!(cached_first.as_ref(), &reader.resolve_object((4, 0)).unwrap());
    let stats = reader.object_cache_stats();
    assert_eq!(stats.object_misses, 1);
    assert_eq!(stats.object_hits, 1);
    assert_eq!(stats.object_loads, 1);
    assert_eq!(stats.object_promotions, 1);
    assert_eq!(stats.probation_entries, 0);
    assert_eq!(stats.protected_entries, 1);
}

#[test]
fn sharded_object_cache_resolves_every_id_and_respects_the_aggregate_entry_cap() {
    let count = 600u32;
    let pdf = wide_object_pdf(count);
    let mut reader =
        IndexedReader::open_with_limits(Arc::new(BytesSource::from(pdf.clone())), ResolverLimits::default()).unwrap();
    // 4096 total entries => 2048 object entries => more than one shard.
    configure_test_caches(&mut reader, 8 * 1024 * 1024, 4096);
    let uncached = open_reader(&pdf, ResolverLimits::default());
    for index in 0..count {
        let id = (index + 2, 0);
        // Twice, so the second call exercises the shard's hit path.
        let first = reader.resolve_object_shared(id).unwrap();
        let second = reader.resolve_object_shared(id).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "shard lost {id:?} between calls");
        assert_eq!(first.as_ref(), &uncached.resolve_object(id).unwrap());
    }
    let stats = reader.object_cache_stats();
    assert_eq!(stats.object_misses, u64::from(count));
    assert_eq!(stats.object_hits, u64::from(count));
    // Residency is reported across every shard and stays inside the configured cap.
    assert!(stats.probation_entries + stats.protected_entries <= 2048);
    assert_eq!(
        stats.probation_entries + stats.protected_entries,
        usize::try_from(count).unwrap()
    );
}

#[test]
fn bounded_eviction_scan_keeps_resolving_while_the_working_set_is_pinned() {
    let count = 400u32;
    let pdf = wide_object_pdf(count);
    let mut reader =
        IndexedReader::open_with_limits(Arc::new(BytesSource::from(pdf.clone())), ResolverLimits::default()).unwrap();
    // One shard, and an entry cap far below the working set, so every insertion after the
    // 32nd has to run the eviction pass against a queue the caller is still holding.
    configure_test_caches(&mut reader, 64 * 1024, 128);
    let uncached = open_reader(&pdf, ResolverLimits::default());
    // Hold every resolved object, which pins its cache entry for the whole loop.
    let mut held = Vec::new();
    for index in 0..count {
        let id = (index + 2, 0);
        let object = reader.resolve_object_shared(id).unwrap();
        assert_eq!(object.as_ref(), &uncached.resolve_object(id).unwrap());
        held.push(object);
    }
    // A pinned prefix never lets the cache exceed the configured entry cap: an insertion
    // that cannot make room bypasses the cache instead of growing it.
    let stats = reader.object_cache_stats();
    assert!(
        stats.probation_entries + stats.protected_entries <= 64,
        "residency {} exceeded the object entry cap",
        stats.probation_entries + stats.protected_entries
    );
    assert_eq!(held.len(), usize::try_from(count).unwrap());
}

#[test]
fn shared_scalar_negative_caches_fatal_errors_but_retries_transient_sources() {
    let source = Arc::new(SwitchableFailureSource {
        bytes: classic_pdf(),
        mode: AtomicU8::new(0),
    });
    let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);

    let first = reader.resolve_object_shared((99, 0)).unwrap_err();
    let second = reader.resolve_object_shared((99, 0)).unwrap_err();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(matches!(
        first.as_ref(),
        IndexedReaderError::MissingNormalObject { id: (99, 0) }
    ));

    source.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        reader.resolve_object_shared((4, 0)).unwrap_err().as_ref(),
        IndexedReaderError::Source(_)
    ));
    source.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        reader
            .resolve_object_shared((4, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Title")
            .unwrap(),
        &Object::String(b"base".to_vec(), StringFormat::Literal)
    );
    let stats = reader.object_cache_stats();
    assert_eq!(stats.negative_hits, 1);
    assert_eq!(stats.transient_failures, 1);
    assert_eq!(stats.object_misses, 3);
}

#[test]
fn shared_scalar_reuses_one_decoded_object_stream_across_members() {
    let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
    let (first, decoded) = object_stream_content(&members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let content = encoder.finish().unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {first} /Filter /FlateDecode"),
        &content,
        &[(10, 0), (11, 1)],
    );
    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf,
        requests: Mutex::new(Vec::new()),
    });
    let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    source.requests.lock().unwrap().clear();

    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
    let first_reads = source.requests.lock().unwrap().len();
    assert_eq!(
        reader.resolve_object_shared((11, 0)).unwrap().as_str().unwrap(),
        b"eleven"
    );
    assert_eq!(source.requests.lock().unwrap().len(), first_reads);
    assert_eq!(
        reader.resolve_object((10, 0)).unwrap(),
        *reader.resolve_object_shared((10, 0)).unwrap()
    );
    let stats = reader.object_stream_cache_stats();
    assert_eq!(stats.loads, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.entries, 1);
    assert!(stats.bytes >= decoded.len());
}

#[test]
fn bounded_scalar_reuses_one_decoded_object_stream_across_215_members() {
    let bodies: Vec<_> = (10..225).map(|id| format!("({id})")).collect();
    let members: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| (u32::try_from(index).unwrap() + 10, body.as_bytes()))
        .collect();
    let (first, decoded) = object_stream_content(&members);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&decoded).unwrap();
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 215 /First {first} /Filter /FlateDecode"),
        &encoder.finish().unwrap(),
        &(10..225).map(|id| (id, id - 10)).collect::<Vec<_>>(),
    );
    let source = Arc::new(TracingBytesSource {
        bytes: fixture.pdf,
        requests: Mutex::new(Vec::new()),
    });
    let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    source.requests.lock().unwrap().clear();

    let mut first_reads = 0;
    for id in 10..225 {
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let scalar = reader.resolve_scalar_with_permit((id, 0), &permit).unwrap();
        assert_eq!(scalar.as_object().as_str().unwrap(), id.to_string().as_bytes());
        drop(scalar);
        permit.close().unwrap();
        if id == 10 {
            first_reads = source.requests.lock().unwrap().len();
        }
    }

    assert_eq!(source.requests.lock().unwrap().len(), first_reads);
    let stats = reader.object_stream_cache_stats();
    assert_eq!(stats.loads, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 214);
    assert_eq!(stats.entries, 1);
    assert!(stats.bytes >= decoded.len());
}

#[test]
fn bounded_object_stream_cache_singleflights_and_cancelled_waiter_exits() {
    let counters = Arc::new(CacheCounters::default());
    let cache = Arc::new(SharedCache::new(
        4096,
        8,
        4096,
        0,
        CacheKind::ObjectStream,
        Arc::clone(&counters),
    ));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let leader_cache = Arc::clone(&cache);
    let leader_entered = Arc::clone(&entered);
    let leader_release = Arc::clone(&release);
    let leader = std::thread::spawn(move || {
        let permit = crate::ScalarResolutionPermit::new(1024);
        let result = leader_cache.resolve_bounded((5, 0), (10, 0), 0, &permit, || {
            leader_entered.wait();
            leader_release.wait();
            let charge = permit.reserve((10, 0), 1, "test-object-stream")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        });
        assert!(matches!(result, Ok(BoundedPreparedObjectStream::Cached(_))));
        drop(result);
        permit.close().unwrap();
    });
    entered.wait();

    let waiter_cache = Arc::clone(&cache);
    let waiter_permit = crate::ScalarResolutionPermit::new(1024);
    let waiter_cancel = waiter_permit.clone();
    let waiter = std::thread::spawn(move || {
        let result = waiter_cache.resolve_bounded((5, 0), (10, 0), 0, &waiter_permit, || {
            panic!("waiter must not become a loader while the leader is live")
        });
        assert!(matches!(
            result,
            Err(IndexedReaderError::ScalarResolutionCancelled {
                phase: "object-stream-cache-wait",
                ..
            })
        ));
        waiter_permit.close().unwrap();
    });
    while counters.objstm_waits.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    waiter_cancel.cancel();
    waiter.join().unwrap();
    release.wait();
    leader.join().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 1);
    assert_eq!(counters.objstm_waits.load(Ordering::Relaxed), 1);
    assert_eq!(cache.residency().0 + cache.residency().2, 1);

    let cancelled = crate::ScalarResolutionPermit::new(1024);
    cancelled.cancel();
    assert!(matches!(
        cache.resolve_bounded((6, 0), (11, 0), 0, &cancelled, || {
            let charge = cancelled.reserve((11, 0), 1, "test-cancelled-leader")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        }),
        Err(IndexedReaderError::ScalarResolutionCancelled { .. })
    ));
    assert_eq!(cancelled.stats().current_bytes, 0);
    cancelled.close().unwrap();
    assert_eq!(cache.residency().0 + cache.residency().2, 1);
}

#[test]
fn bounded_object_stream_cache_evicts_reloads_and_keeps_oversize_call_local() {
    let entry_weight = PreparedObjectStream::NotStream.cache_weight();
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::new(
        entry_weight,
        1,
        entry_weight,
        0,
        CacheKind::ObjectStream,
        Arc::clone(&counters),
    );
    for id in [(5, 0), (6, 0), (5, 0)] {
        let permit = crate::ScalarResolutionPermit::new(1024);
        let result = cache
            .resolve_bounded(id, id, 0, &permit, || {
                let charge = permit.reserve(id, 1, "test-object-stream")?;
                Ok((PreparedObjectStream::NotStream, vec![charge]))
            })
            .unwrap();
        assert!(matches!(result, BoundedPreparedObjectStream::Cached(_)));
        drop(result);
        permit.close().unwrap();
    }
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 3);
    assert_eq!(counters.objstm_evictions.load(Ordering::Relaxed), 2);

    let oversize_counters = Arc::new(CacheCounters::default());
    let oversize_cache = SharedCache::new(1, 1, 1, 0, CacheKind::ObjectStream, Arc::clone(&oversize_counters));
    let permit = crate::ScalarResolutionPermit::new(4096);
    let result = oversize_cache
        .resolve_bounded((7, 0), (7, 0), 0, &permit, || {
            let stream = Stream::new(Dictionary::new(), vec![0; 64]);
            let prepared = PreparedObjectStream::Raw(stream);
            let charge = permit.reserve(
                (7, 0),
                u64::try_from(prepared.retained_bytes()).unwrap(),
                "test-object-stream-oversize",
            )?;
            Ok((prepared, vec![charge]))
        })
        .unwrap();
    assert!(matches!(result, BoundedPreparedObjectStream::CallLocal { .. }));
    assert!(permit.stats().current_bytes > 0);
    assert_eq!(oversize_cache.residency().0 + oversize_cache.residency().2, 0);
    drop(result);
    assert_eq!(permit.stats().current_bytes, 0);
    permit.close().unwrap();
    assert_eq!(oversize_counters.objstm_bypasses.load(Ordering::Relaxed), 1);

    let refusal = crate::ScalarResolutionPermit::new(8);
    assert!(matches!(
        oversize_cache.resolve_bounded((8, 0), (8, 0), 0, &refusal, || {
            let charge = refusal.reserve((8, 0), 9, "test-object-stream-over-b-plus-o")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        }),
        Err(IndexedReaderError::ScalarResourceLimit { .. })
    ));
    assert_eq!(refusal.stats().current_bytes, 0);
    refusal.close().unwrap();
    assert_eq!(oversize_cache.residency().0 + oversize_cache.residency().2, 0);
}

#[test]
fn bounded_object_stream_oversize_bypass_permits_close_independently() {
    let counters = Arc::new(CacheCounters::default());
    let cache = Arc::new(SharedCache::new(
        1,
        8,
        1,
        0,
        CacheKind::ObjectStream,
        Arc::clone(&counters),
    ));
    let first_permit = crate::ScalarResolutionPermit::new(4096);
    let first = cache
        .resolve_bounded((7, 0), (10, 0), 0, &first_permit, || {
            let prepared = PreparedObjectStream::Raw(Stream::new(Dictionary::new(), vec![0; 64]));
            let charge = first_permit.reserve(
                (10, 0),
                u64::try_from(prepared.retained_bytes()).unwrap(),
                "test-object-stream-oversize-first",
            )?;
            Ok((prepared, vec![charge]))
        })
        .unwrap();
    let second_permit = crate::ScalarResolutionPermit::new(4096);
    let second = cache
        .resolve_bounded((7, 0), (11, 0), 1, &second_permit, || {
            let prepared = PreparedObjectStream::Raw(Stream::new(Dictionary::new(), vec![0; 64]));
            let charge = second_permit.reserve(
                (11, 0),
                u64::try_from(prepared.retained_bytes()).unwrap(),
                "test-object-stream-oversize-second",
            )?;
            Ok((prepared, vec![charge]))
        })
        .unwrap();
    assert!(matches!(first, BoundedPreparedObjectStream::CallLocal { .. }));
    assert!(matches!(second, BoundedPreparedObjectStream::CallLocal { .. }));

    // Closing either permit must not wait for, or release, the other call's
    // allocation. Independent O budgets intentionally trade singleflight
    // for unambiguous ownership on non-resident results.
    drop(first);
    first_permit.close().unwrap();
    assert!(second_permit.stats().current_bytes > 0);
    drop(second);
    second_permit.close().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 2);
    assert_eq!(counters.objstm_bypasses.load(Ordering::Relaxed), 2);
}

#[test]
fn bounded_object_stream_negative_cache_rewraps_each_requested_member() {
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::new(4096, 8, 4096, 0, CacheKind::ObjectStream, Arc::clone(&counters));
    let first = crate::ScalarResolutionPermit::new(1024);
    let malformed = match cache.resolve_bounded((5, 0), (10, 0), 0, &first, || {
        Err(IndexedReaderError::ObjectStreamMember {
            id: (10, 0),
            container: (5, 0),
            index: 0,
            source: crate::Error::InvalidObjectStream("stable malformed member".into()),
        })
    }) {
        Err(error) => error,
        Ok(_) => panic!("malformed object stream unexpectedly resolved"),
    };
    first.close().unwrap();
    let second = crate::ScalarResolutionPermit::new(1024);
    let shared = match cache.resolve_bounded((5, 0), (11, 0), 1, &second, || {
        panic!("stable malformed error must be shared")
    }) {
        Err(error) => error,
        Ok(_) => panic!("cached malformed object stream unexpectedly resolved"),
    };
    second.close().unwrap();
    assert!(matches!(
        malformed,
        IndexedReaderError::ObjectStreamMember {
            id: (10, 0),
            container: (5, 0),
            index: 0,
            source: crate::Error::InvalidObjectStream(_),
        }
    ));
    assert!(matches!(
        shared,
        IndexedReaderError::ObjectStreamMember {
            id: (11, 0),
            container: (5, 0),
            index: 1,
            source: crate::Error::InvalidObjectStream(_),
        }
    ));
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 1);

    let transient = crate::ScalarResolutionPermit::new(1024);
    assert!(matches!(
        cache.resolve_bounded((6, 0), (12, 0), 0, &transient, || {
            Err(IndexedReaderError::Source(SourceError::Io(std::io::Error::other(
                "one-shot source failure",
            ))))
        }),
        Err(IndexedReaderError::Source(SourceError::Io(_)))
    ));
    transient.close().unwrap();
    let retry = crate::ScalarResolutionPermit::new(1024);
    let recovered = cache
        .resolve_bounded((6, 0), (12, 0), 0, &retry, || {
            let charge = retry.reserve((6, 0), 1, "transient-retry")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        })
        .unwrap();
    assert!(matches!(recovered, BoundedPreparedObjectStream::Cached(_)));
    drop(recovered);
    retry.close().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 3);
}

#[test]
fn bounded_object_stream_negative_cache_rewraps_nonstream_and_unsupported_members() {
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::new(4096, 8, 4096, 0, CacheKind::ObjectStream, Arc::clone(&counters));

    let first = crate::ScalarResolutionPermit::new(1024);
    assert!(matches!(
        cache.resolve_bounded((5, 0), (10, 0), 0, &first, || {
            Err(IndexedReaderError::ObjectStreamContainerNotStream {
                id: (10, 0),
                container: (5, 0),
            })
        }),
        Err(IndexedReaderError::ObjectStreamContainerNotStream {
            id: (10, 0),
            container: (5, 0),
        })
    ));
    first.close().unwrap();
    let second = crate::ScalarResolutionPermit::new(1024);
    assert!(matches!(
        cache.resolve_bounded((5, 0), (11, 0), 1, &second, || panic!("non-stream must be cached")),
        Err(IndexedReaderError::ObjectStreamContainerNotStream {
            id: (11, 0),
            container: (5, 0),
        })
    ));
    second.close().unwrap();

    const REASON: &str = "object-stream filter chains or decode parameters outside the bounded decode envelope";
    let third = crate::ScalarResolutionPermit::new(1024);
    assert!(matches!(
        cache.resolve_bounded((6, 0), (10, 0), 0, &third, || {
            Err(IndexedReaderError::UnsupportedBoundedScalar {
                id: (10, 0),
                reason: REASON,
            })
        }),
        Err(IndexedReaderError::UnsupportedBoundedScalar {
            id: (10, 0),
            reason: REASON,
        })
    ));
    third.close().unwrap();
    let fourth = crate::ScalarResolutionPermit::new(1024);
    assert!(matches!(
        cache.resolve_bounded((6, 0), (11, 0), 1, &fourth, || panic!("unsupported must be cached")),
        Err(IndexedReaderError::UnsupportedBoundedScalar {
            id: (11, 0),
            reason: REASON,
        })
    ));
    fourth.close().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 2);
    assert_eq!(counters.negative_hits.load(Ordering::Relaxed), 2);
}

#[test]
fn bounded_object_stream_all_pinned_pressure_bypasses_and_closes_every_charge() {
    let entry_weight = PreparedObjectStream::NotStream.cache_weight();
    let cache = SharedCache::new(
        entry_weight,
        1,
        entry_weight,
        0,
        CacheKind::ObjectStream,
        Arc::default(),
    );
    let pinned_permit = crate::ScalarResolutionPermit::new(1024);
    let pinned = cache
        .resolve_bounded((5, 0), (5, 0), 0, &pinned_permit, || {
            let charge = pinned_permit.reserve((5, 0), 1, "pinned-entry")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        })
        .unwrap();
    assert!(matches!(pinned, BoundedPreparedObjectStream::Cached(_)));

    let bypass_permit = crate::ScalarResolutionPermit::new(1024);
    let bypass = cache
        .resolve_bounded((6, 0), (6, 0), 0, &bypass_permit, || {
            let charge = bypass_permit.reserve((6, 0), 1, "all-pinned-bypass")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        })
        .unwrap();
    assert!(matches!(bypass, BoundedPreparedObjectStream::CallLocal { .. }));
    assert_eq!(cache.residency().0 + cache.residency().2, 1);
    drop(bypass);
    assert_eq!(bypass_permit.stats().current_bytes, 0);
    bypass_permit.close().unwrap();
    drop(pinned);
    pinned_permit.close().unwrap();
}

#[test]
fn bounded_encrypted_object_stream_caches_are_reader_and_epoch_isolated() {
    let (pdf_r4, _, _) = encrypted_object_stream_pdf(4, true, 0);
    let (pdf_r6, _, _) = encrypted_object_stream_pdf(6, true, 0);
    let mut reader_r4 = open_encrypted(&pdf_r4, Some(b"user")).unwrap();
    let mut reader_r6 = open_encrypted(&pdf_r6, Some(b"user")).unwrap();
    for reader in [&mut reader_r4, &mut reader_r6] {
        reader.configure_resolution_caches(0, 0, 4 * 1024 * 1024, 8);
    }
    for reader in [&reader_r4, &reader_r6] {
        let permit = crate::ScalarResolutionPermit::new(4 * 1024 * 1024);
        let member = reader.resolve_scalar_with_permit((10, 0), &permit).unwrap();
        assert_eq!(
            member
                .as_object()
                .as_dict()
                .unwrap()
                .get(b"Text")
                .unwrap()
                .as_str()
                .unwrap(),
            b"member secret"
        );
        drop(member);
        permit.close().unwrap();
        let stats = reader.object_stream_cache_stats();
        assert_eq!(stats.loads, 1);
        assert_eq!(stats.entries, 1);
    }
}

#[test]
fn cached_constructor_wires_every_partition_into_unified_stats() {
    let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
    let (first, decoded) = object_stream_content(&members);
    let fixture = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {first}"),
        &decoded,
        &[(10, 0), (11, 1)],
    );
    let options = IndexedReaderCacheOptions::new(8 * 1024 * 1024, 1024);
    let reader =
        IndexedReader::open_cached(BytesSource::from(fixture.pdf), IndexedReaderOptions::default(), options).unwrap();

    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
    assert_eq!(
        reader.resolve_object_shared((11, 0)).unwrap().as_str().unwrap(),
        b"eleven"
    );
    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");

    let stats = reader.cache_stats();
    assert!(stats.source().loads() > 0);
    assert_eq!(stats.object().object_loads, 2);
    assert_eq!(stats.object().object_hits, 1);
    assert_eq!(stats.object_stream().loads, 1);
    assert_eq!(stats.object_stream().hits, 1);
    assert_eq!(stats.object_stream().entries, 1);
    assert!(stats.object_stream().bytes >= decoded.len());
    assert_eq!(*stats.object(), reader.object_cache_stats());
    assert_eq!(*stats.object_stream(), reader.object_stream_cache_stats());
    assert!(stats.current_bytes() <= options.max_bytes());
    assert!(stats.peak_bytes() <= options.max_bytes());
    assert!(stats.current_entries() <= options.max_entries());
    assert!(stats.peak_entries() <= options.max_entries());
}

#[test]
fn oversized_streams_bypass_probation_retention() {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Stream(Stream::new(Dictionary::new(), vec![b'x'; 8 * 1024])),
    );
    document.max_id = 1;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    let mut reader = IndexedReader::open(BytesSource::from(pdf)).unwrap();
    // Object half is 4 KiB and probation is one quarter of that (1 KiB).
    configure_test_caches(&mut reader, 8 * 1024, 64);
    assert_eq!(
        reader
            .resolve_object_shared((1, 0))
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .len(),
        8 * 1024
    );
    assert_eq!(
        reader
            .resolve_object_shared((1, 0))
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .len(),
        8 * 1024
    );
    let stats = reader.object_cache_stats();
    assert_eq!(stats.object_bypasses, 2);
    assert_eq!(stats.probation_entries + stats.protected_entries, 0);
}

#[test]
fn segmented_cache_promotes_churns_and_stays_within_a_five_thousand_entry_cap() {
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::new(
        2 * 1024 * 1024,
        5_000,
        512 * 1024,
        75,
        CacheKind::Object,
        Arc::clone(&counters),
    );
    for id in 1..=5_100 {
        assert_eq!(
            *cache
                .resolve((id, 0), || Ok(Arc::new(Object::Integer(i64::from(id)))), |_| 64)
                .unwrap(),
            Object::Integer(i64::from(id))
        );
    }
    let (probation_entries, probation_bytes, protected_entries, protected_bytes) = cache.residency();
    assert_eq!(probation_entries, 1_250);
    assert_eq!(protected_entries, 0);
    assert!(probation_bytes + protected_bytes <= 2 * 1024 * 1024);
    assert_eq!(counters.object_evictions.load(Ordering::Relaxed), 3_850);

    let promoted = cache
        .resolve((5_100, 0), || panic!("resident entry must not reload"), |_| 64)
        .unwrap();
    assert_eq!(*promoted, Object::Integer(5_100));
    let (_, _, protected_entries, _) = cache.residency();
    assert_eq!(protected_entries, 1);
}

#[test]
fn single_flight_shares_one_load_and_one_arc_with_waiters() {
    let counters = Arc::new(CacheCounters::default());
    let cache = Arc::new(SharedCache::new(
        1024,
        8,
        256,
        75,
        CacheKind::Object,
        Arc::clone(&counters),
    ));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let cache = Arc::clone(&cache);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        threads.push(std::thread::spawn(move || {
            cache
                .resolve(
                    (1, 0),
                    || {
                        entered.wait();
                        release.wait();
                        Ok(Arc::new(Object::Integer(42)))
                    },
                    |_| 64,
                )
                .unwrap()
        }));
    }
    entered.wait();
    while counters.object_waits.load(Ordering::SeqCst) < 3 {
        std::thread::yield_now();
    }
    release.wait();
    let values: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert!(values.iter().all(|value| Arc::ptr_eq(&values[0], value)));
    assert_eq!(counters.object_loads.load(Ordering::Relaxed), 1);
    assert_eq!(counters.object_waits.load(Ordering::Relaxed), 3);
}

#[test]
fn transient_failure_is_shared_only_with_waiters_then_post_publication_retries() {
    let counters = Arc::new(CacheCounters::default());
    let cache = Arc::new(SharedCache::new(
        1024,
        8,
        256,
        75,
        CacheKind::Object,
        Arc::clone(&counters),
    ));
    let load_entered = Arc::new(std::sync::Barrier::new(2));
    let release_load = Arc::new(std::sync::Barrier::new(2));
    let published = Arc::new(std::sync::Barrier::new(2));
    let release_publisher = Arc::new(std::sync::Barrier::new(2));
    {
        let published = Arc::clone(&published);
        let release_publisher = Arc::clone(&release_publisher);
        *cache.after_publish_hook.lock().unwrap() = Some(Arc::new(move || {
            published.wait();
            release_publisher.wait();
        }));
    }

    let leader_cache = Arc::clone(&cache);
    let leader_entered = Arc::clone(&load_entered);
    let leader_release = Arc::clone(&release_load);
    let leader = std::thread::spawn(move || {
        leader_cache.resolve(
            (1, 0),
            || {
                leader_entered.wait();
                leader_release.wait();
                Err(Arc::new(IndexedReaderError::Source(SourceError::Io(
                    std::io::Error::other("transient"),
                ))))
            },
            |_| 64,
        )
    });
    load_entered.wait();

    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                cache.resolve((1, 0), || panic!("waiter must not become a second leader"), |_| 64)
            })
        })
        .collect();
    while counters.object_waits.load(Ordering::SeqCst) < 2 {
        std::thread::yield_now();
    }
    release_load.wait();
    published.wait();

    let waiter_errors: Vec<_> = waiters
        .into_iter()
        .map(|waiter| waiter.join().unwrap().unwrap_err())
        .collect();
    assert!(Arc::ptr_eq(&waiter_errors[0], &waiter_errors[1]));

    let retried = cache
        .resolve((1, 0), || Ok(Arc::new(Object::Integer(42))), |_| 64)
        .unwrap();
    assert_eq!(*retried, Object::Integer(42));

    release_publisher.wait();
    let leader_error = leader.join().unwrap().unwrap_err();
    assert!(Arc::ptr_eq(&leader_error, &waiter_errors[0]));
    assert_eq!(counters.object_loads.load(Ordering::Relaxed), 2);
    assert_eq!(counters.object_misses.load(Ordering::Relaxed), 2);
    assert_eq!(counters.object_hits.load(Ordering::Relaxed), 2);
    assert_eq!(counters.object_waits.load(Ordering::Relaxed), 2);
    assert_eq!(counters.object_transient_failures.load(Ordering::Relaxed), 1);
    assert_eq!(counters.negative_hits.load(Ordering::Relaxed), 0);
}

#[test]
fn fatal_failure_remains_negative_cached_with_shared_identity_and_count() {
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::<Object>::new(1024, 8, 256, 75, CacheKind::Object, Arc::clone(&counters));
    let first = cache
        .resolve(
            (99, 0),
            || Err(Arc::new(IndexedReaderError::MissingNormalObject { id: (99, 0) })),
            |_| 64,
        )
        .unwrap_err();
    let second = cache
        .resolve((99, 0), || panic!("fatal negative entry must not reload"), |_| 64)
        .unwrap_err();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(counters.object_loads.load(Ordering::Relaxed), 1);
    assert_eq!(counters.object_misses.load(Ordering::Relaxed), 1);
    assert_eq!(counters.object_hits.load(Ordering::Relaxed), 1);
    assert_eq!(counters.negative_hits.load(Ordering::Relaxed), 1);
    assert_eq!(counters.object_transient_failures.load(Ordering::Relaxed), 0);
}

#[test]
fn public_cache_counters_saturate_instead_of_wrapping() {
    let counter = AtomicU64::new(u64::MAX - 1);
    atomic_saturating_increment(&counter);
    atomic_saturating_increment(&counter);
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn sharded_peaks_aggregate_across_shards_rather_than_maxing_one() {
    // Two shards' worth of entry budget, one object per shard, all resident at once.
    let counters = Arc::new(CacheCounters::default());
    let cache = ShardedCache::<Object>::new(
        1024 * 1024,
        SHARED_OBJECT_MIN_ENTRIES_PER_SHARD * 4,
        1024 * 1024,
        75,
        CacheKind::Object,
        Arc::clone(&counters),
    );
    assert!(cache.shards.len() > 1, "this test needs a sharded cache");
    let mut retained = Vec::new();
    for id in 0..cache.shards.len() as u32 * 4 {
        retained.push(
            cache
                .resolve((id, 0), || Ok(Arc::new(Object::Integer(i64::from(id)))), |_| 64)
                .unwrap(),
        );
    }

    let (probation_entries, probation_bytes, protected_entries, protected_bytes) = cache.residency();
    let live_entries = probation_entries + protected_entries;
    let live_bytes = probation_bytes + protected_bytes;
    let peak_entries = counters.object_peak_entries.load(Ordering::Relaxed);
    let peak_bytes = counters.object_peak_bytes.load(Ordering::Relaxed);
    // The peak is over the *sum* of the shards, so it bounds the summed residency —
    // not just the largest single shard's slice of it.
    assert_eq!(peak_entries, live_entries);
    assert_eq!(peak_bytes, live_bytes);
    assert!(
        live_entries > cache.shards.len(),
        "every shard must hold more than one entry"
    );

    // Dropping every entry and admitting one more keeps the peak at the high-water mark
    // rather than letting a later single-shard report pull it down.
    drop(retained);
    for id in 0..cache.shards.len() as u32 * 4 {
        let _ = cache.resolve((id, 0), || Ok(Arc::new(Object::Integer(i64::from(id)))), |_| 64);
    }
    assert_eq!(counters.object_peak_entries.load(Ordering::Relaxed), peak_entries);
    assert_eq!(counters.object_peak_bytes.load(Ordering::Relaxed), peak_bytes);
}

#[test]
fn loading_cells_are_not_evicted_while_pinned() {
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::<Object>::new(64, 1, 64, 75, CacheKind::Object, counters);
    let pinned = Arc::new(SharedCell::loading());
    {
        let mut inner = cache.inner.lock().unwrap();
        inner.probation.push_back((1, 0));
        inner.entries.insert(
            (1, 0),
            CacheEntry {
                cell: Arc::clone(&pinned),
                segment: CacheSegment::Probation,
                bytes: 0,
            },
        );
        cache.enforce_caps(&mut inner);
        assert!(inner.entries.contains_key(&(1, 0)));
    }
}

#[test]
fn loading_entry_saturation_bypasses_without_exceeding_the_entry_cap() {
    let counters = Arc::new(CacheCounters::default());
    let cache = Arc::new(SharedCache::<Object>::new(
        1024,
        2,
        512,
        75,
        CacheKind::Object,
        Arc::clone(&counters),
    ));
    let entered = Arc::new(std::sync::Barrier::new(5));
    let release = Arc::new(std::sync::Barrier::new(5));
    let threads: Vec<_> = (1..=4)
        .map(|id| {
            let cache = Arc::clone(&cache);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                cache.resolve(
                    (id, 0),
                    || {
                        entered.wait();
                        release.wait();
                        Ok(Arc::new(Object::Integer(i64::from(id))))
                    },
                    |_| 64,
                )
            })
        })
        .collect();
    entered.wait();
    let (probation_entries, _, protected_entries, _) = cache.residency();
    assert_eq!(probation_entries + protected_entries, 2);
    assert_eq!(counters.object_peak_entries.load(Ordering::Relaxed), 2);
    assert_eq!(counters.object_bypasses.load(Ordering::Relaxed), 2);
    release.wait();
    for thread in threads {
        assert!(thread.join().unwrap().is_ok());
    }
    assert_eq!(counters.object_loads.load(Ordering::Relaxed), 4);
}

#[test]
fn cached_shared_resolution_preserves_encryption_revisions_two_through_six() {
    for revision in 2..=6 {
        let pdf = encrypted_pdf(revision, "owner", "user");
        let mut reader = open_encrypted(&pdf, Some(b"user")).unwrap();
        configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
        let legacy = reader.resolve_object((1, 0)).unwrap();
        let first = reader.resolve_object_shared((1, 0)).unwrap();
        let second = reader.resolve_object_shared((1, 0)).unwrap();
        assert_eq!(legacy, *first, "revision {revision}");
        assert!(Arc::ptr_eq(&first, &second), "revision {revision}");
    }

    let (pdf, _, _) = encrypted_object_stream_pdf(4, false, 0);
    let mut reader = open_encrypted(&pdf, Some(b"user")).unwrap();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    for id in [(10, 0), (20, 0), (21, 0)] {
        assert_eq!(
            reader.resolve_object(id).unwrap(),
            *reader.resolve_object_shared(id).unwrap()
        );
    }
    assert_eq!(reader.object_stream_cache_stats().loads, 1);
}

#[test]
fn cached_object_streams_preserve_duplicate_mismatch_and_malformed_errors() {
    let duplicate_header = b"10 0 10 6 ";
    let mut duplicate_content = duplicate_header.to_vec();
    duplicate_content.extend_from_slice(b"(one) (two)");
    let duplicate = object_stream_fixture(
        &format!("/Type /ObjStm /N 2 /First {}", duplicate_header.len()),
        &duplicate_content,
        &[(10, 1)],
    );
    let mut reader = open_reader(&duplicate.pdf, ResolverLimits::default());
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"two");

    let members = [(10, b"(ten)".as_slice()), (11, b"(eleven)".as_slice())];
    let (first, content) = object_stream_content(&members);
    let mismatch = object_stream_fixture(&format!("/Type /ObjStm /N 2 /First {first}"), &content, &[(10, 1)]);
    let mut reader = open_reader(&mismatch.pdf, ResolverLimits::default());
    let legacy = reader.resolve_object((10, 0)).unwrap_err().to_string();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    let first_error = reader.resolve_object_shared((10, 0)).unwrap_err();
    let second_error = reader.resolve_object_shared((10, 0)).unwrap_err();
    assert_eq!(first_error.to_string(), legacy);
    assert!(Arc::ptr_eq(&first_error, &second_error));

    let malformed = object_stream_fixture("/Type /ObjStm /N 1 /First 999", b"10 0 (ten)", &[(10, 0)]);
    let mut reader = open_reader(&malformed.pdf, ResolverLimits::default());
    let legacy = reader.resolve_object((10, 0)).unwrap_err().to_string();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap_err().to_string(), legacy);
}

#[test]
fn cached_object_stream_transient_source_failure_is_retried() {
    let members = [(10, b"(ten)".as_slice())];
    let (first, content) = object_stream_content(&members);
    let fixture = object_stream_fixture(&format!("/Type /ObjStm /N 1 /First {first}"), &content, &[(10, 0)]);
    let source = Arc::new(FailOnceSource {
        bytes: fixture.pdf,
        armed: AtomicBool::new(false),
        armed_reads: AtomicUsize::new(0),
    });
    let mut reader = IndexedReader::open_with_limits(source.clone(), ResolverLimits::default()).unwrap();
    configure_test_caches(&mut reader, 4 * 1024 * 1024, 256);
    source.armed.store(true, Ordering::SeqCst);
    assert!(matches!(
        reader.resolve_object_shared((10, 0)).unwrap_err().as_ref(),
        IndexedReaderError::Source(_)
    ));
    assert_eq!(reader.resolve_object_shared((10, 0)).unwrap().as_str().unwrap(), b"ten");
    assert_eq!(reader.object_stream_cache_stats().transient_failures, 1);
    assert_eq!(reader.object_stream_cache_stats().loads, 2);
}

#[test]
fn bounded_object_stream_encoded_length_failure_is_not_negative_cached() {
    const ENCODED_LEN: u64 = 8192;
    let counters = Arc::new(CacheCounters::default());
    let cache = SharedCache::new(4096, 8, 4096, 0, CacheKind::ObjectStream, Arc::clone(&counters));
    let small = crate::ScalarResolutionPermit::new(4096);
    assert!(matches!(
        cache.resolve_bounded((5, 0), (10, 0), 0, &small, || {
            Err(IndexedReaderError::StreamLimitExceeded {
                id: (5, 0),
                length: ENCODED_LEN,
                limit: small.limit_bytes(),
            })
        }),
        Err(IndexedReaderError::StreamLimitExceeded {
            length: ENCODED_LEN,
            limit: 4096,
            ..
        })
    ));
    assert_eq!(small.stats().current_bytes, 0);
    small.close().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 1);
    assert_eq!(cache.residency().0 + cache.residency().2, 0);

    let large = crate::ScalarResolutionPermit::new(16 * 1024);
    let prepared = cache
        .resolve_bounded((5, 0), (10, 0), 0, &large, || {
            assert!(ENCODED_LEN <= large.limit_bytes());
            let charge = large.reserve((10, 0), 1, "encoded-length-retry")?;
            Ok((PreparedObjectStream::NotStream, vec![charge]))
        })
        .unwrap();
    assert!(matches!(prepared, BoundedPreparedObjectStream::Cached(_)));
    drop(prepared);
    large.close().unwrap();
    assert_eq!(counters.objstm_loads.load(Ordering::Relaxed), 2);
    assert_eq!(cache.residency().0 + cache.residency().2, 1);
}
