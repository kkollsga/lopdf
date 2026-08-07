use lopdf::{BytesSource, Document, IndexedReader, Object, dictionary};

const FROZEN_BASE_BYTES: u64 = 33_554_432;
const FROZEN_OBJECT_BYTES: u64 = 1_536;
const FROZEN_PAGE_BYTES: u64 = 3_072;

fn frozen_l1_cap(objects: usize, pages: usize) -> u64 {
    FROZEN_BASE_BYTES
        .saturating_add(FROZEN_OBJECT_BYTES.saturating_mul(u64::try_from(objects).unwrap()))
        .saturating_add(FROZEN_PAGE_BYTES.saturating_mul(u64::try_from(pages).unwrap()))
}

fn object_axis_fixture(count: u32) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    for id in 1..=count {
        document.objects.insert((id, 0), Object::Integer(i64::from(id)));
    }
    document.max_id = count;
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn page_axis_fixture(count: u32) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let kids: Vec<_> = (0..count).map(|page| Object::Reference((page + 3, 0))).collect();
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => i64::from(count),
        }),
    );
    for page in 0..count {
        document.objects.insert(
            (page + 3, 0),
            Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => Object::Reference((2, 0)) }),
        );
    }
    document.max_id = count + 2;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn indexed_fixture() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = [0usize; 7];
    for (number, generation, body) in [
        (1usize, 4u16, b"<< /Type /Catalog /Pages 2 0 R >>".as_slice()),
        (2, 0, b"<< /Type /Pages /Kids [3 0 R] /Count 99 >>".as_slice()),
        (3, 0, b"<< /Type /Page /Parent 2 0 R >>".as_slice()),
        (5, 2, b"<< /Producer (indexed-api) >>".as_slice()),
        (6, 0, b"<< /Length 4 >>\nstream\nDATA\nendstream".as_slice()),
    ] {
        offsets[number] = pdf.len();
        pdf.extend_from_slice(format!("{number} {generation} obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    pdf.extend_from_slice(format!("{:010} 00004 n \n", offsets[1]).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[2]).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[3]).as_bytes());
    pdf.extend_from_slice(b"0000000000 00007 f \n");
    pdf.extend_from_slice(format!("{:010} 00002 n \n", offsets[5]).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[6]).as_bytes());
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 7 /Root 1 4 R /Info 5 2 R /Flag true >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

#[test]
fn trailer_entries_are_owned_and_indirect_values_are_resolved() {
    let reader = IndexedReader::open(BytesSource::from(indexed_fixture())).unwrap();

    let info = reader.trailer_entry_owned(b"Info").unwrap().unwrap();
    assert_eq!(
        info.as_dict().unwrap().get(b"Producer").unwrap().as_str().unwrap(),
        b"indexed-api"
    );
    assert_eq!(
        reader.trailer_entry_owned(b"Flag").unwrap(),
        Some(Object::Boolean(true))
    );
    assert_eq!(reader.trailer_entry_owned(b"Missing").unwrap(), None);
}

#[test]
fn object_ids_exclude_free_entries_and_preserve_generations() {
    let reader = IndexedReader::open(BytesSource::from(indexed_fixture())).unwrap();
    assert_eq!(reader.object_ids(), vec![(1, 4), (2, 0), (3, 0), (5, 2), (6, 0)]);
}

#[test]
fn index_stats_count_actual_pages_and_bound_structural_residency() {
    let reader = IndexedReader::open(BytesSource::from(indexed_fixture())).unwrap();
    let stats = reader.index_stats().unwrap();

    assert_eq!(stats.object_count(), 5);
    assert_eq!(stats.page_count(), 1);
    assert!(stats.index_retained_bytes() > 5 * std::mem::size_of::<lopdf::ObjectId>() as u64);
    assert!(stats.page_map_retained_bytes() >= std::mem::size_of::<lopdf::PageMap>() as u64);
    assert_eq!(
        stats.estimated_retained_bytes(),
        stats.index_retained_bytes() + stats.page_map_retained_bytes()
    );
}

#[test]
fn unified_bounded_owner_keeps_scalar_and_stream_charged_without_copying() {
    let reader = IndexedReader::open(BytesSource::from(indexed_fixture())).unwrap();

    let scalar_permit = lopdf::ScalarResolutionPermit::new(1024 * 1024);
    let scalar = reader.resolve_object_with_permit((5, 2), &scalar_permit).unwrap();
    assert!(!scalar.is_stream());
    assert!(scalar.as_object().as_dict().is_ok());
    assert_eq!(scalar_permit.stats().current_bytes, scalar.retained_bytes());
    drop(scalar);
    assert_eq!(scalar_permit.stats().current_bytes, 0);

    let stream_permit = lopdf::ScalarResolutionPermit::new(1024 * 1024);
    let stream = reader.resolve_object_with_permit((6, 0), &stream_permit).unwrap();
    assert!(stream.is_stream());
    let Object::Stream(value) = stream.as_object() else {
        panic!("bounded stream did not expose its object view")
    };
    assert_eq!(value.content, b"DATA");
    assert_eq!(stream_permit.stats().current_bytes, stream.retained_bytes());
    drop(stream);
    assert_eq!(stream_permit.stats().current_bytes, 0);
}

#[test]
fn frozen_l1_cap_bounds_generated_five_and_ten_thousand_object_axes() {
    for count in [5_000, 10_000] {
        let reader = IndexedReader::open(BytesSource::from(object_axis_fixture(count))).unwrap();
        let stats = reader.index_stats().unwrap();
        // Modern save adds one live xref-stream object to the requested axis.
        assert_eq!(stats.object_count(), usize::try_from(count + 1).unwrap());
        assert_eq!(stats.page_count(), 0);
        assert!(
            stats.estimated_retained_bytes() <= frozen_l1_cap(stats.object_count(), stats.page_count()),
            "{count}-object estimate {} exceeded frozen cap {}",
            stats.estimated_retained_bytes(),
            frozen_l1_cap(stats.object_count(), stats.page_count())
        );
    }
}

#[test]
fn frozen_l1_cap_bounds_generated_five_and_ten_thousand_page_axes() {
    for count in [5_000, 10_000] {
        let reader = IndexedReader::open(BytesSource::from(page_axis_fixture(count))).unwrap();
        let stats = reader.index_stats().unwrap();
        // Catalog + Pages + generated leaves + one live xref-stream object.
        assert_eq!(stats.object_count(), usize::try_from(count + 3).unwrap());
        assert_eq!(stats.page_count(), usize::try_from(count).unwrap());
        assert!(
            stats.estimated_retained_bytes() <= frozen_l1_cap(stats.object_count(), stats.page_count()),
            "{count}-page estimate {} exceeded frozen cap {}",
            stats.estimated_retained_bytes(),
            frozen_l1_cap(stats.object_count(), stats.page_count())
        );
    }
}
