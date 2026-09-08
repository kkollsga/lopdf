//! Fork-owned eager-reader behavior kept outside upstream's hot reader module.

use std::collections::{BTreeMap, HashSet};

use log::warn;

use crate::error::{ParseError, XrefError};
use crate::reader::Reader;
use crate::xref::Xref;
use crate::{Dictionary, Error, Object, ObjectId, Result};

pub(crate) type ObjectStreamBatch = (u32, BTreeMap<ObjectId, Object>);

/// Merge eagerly parsed object-stream members in the same order as the serial
/// outer xref walk, independently of worker completion order.
pub(crate) fn merge_object_stream_batches(
    objects: &mut BTreeMap<ObjectId, Object>, mut batches: Vec<ObjectStreamBatch>,
) {
    batches.sort_by_key(|(xref_key, _)| *xref_key);
    for (_, batch) in batches {
        for (id, object) in batch {
            objects.entry(id).or_insert(object);
        }
    }
}

/// Resolve all xref revisions while preserving the fork's free-entry and
/// per-section hybrid-reference semantics.
pub(crate) fn resolve_xref_and_trailer(reader: &mut Reader<'_>) -> Result<(Xref, Dictionary)> {
    let xref_start = Reader::get_xref_start(reader.buffer)?;
    if xref_start > reader.buffer.len() {
        return Err(Error::Xref(XrefError::Start));
    }
    let xref_start = reader.correct_xref_offset(xref_start);
    reader.document.xref_start = xref_start;

    let (mut xref, mut trailer) = reader.xref_and_trailer_at(xref_start)?;

    // `/XRefStm` describes its own revision. Apply each supplement before
    // merging that section so compressed rows supersede compatibility rows.
    let mut already_seen = HashSet::new();
    let mut already_seen_supplements = HashSet::new();
    let xref_stream_start = trailer.remove(b"XRefStm");
    apply_hybrid_supplement(reader, &mut xref, xref_stream_start, &mut already_seen_supplements);

    let mut prev_xref_start = trailer.remove(b"Prev");
    while let Some(prev) = prev_xref_start.take().and_then(|offset| offset.as_i64().ok()) {
        if !already_seen.insert(prev) {
            break;
        }
        if prev < 0 || prev as usize > reader.buffer.len() {
            return Err(Error::Xref(XrefError::PrevStart));
        }

        let (mut prev_xref, mut prev_trailer) = reader.xref_and_trailer_at(prev as usize)?;
        let prev_xref_stream_start = prev_trailer.remove(b"XRefStm");
        apply_hybrid_supplement(
            reader,
            &mut prev_xref,
            prev_xref_stream_start,
            &mut already_seen_supplements,
        );
        xref.merge(prev_xref);
        prev_xref_start = prev_trailer.remove(b"Prev");
    }
    normalize_declared_size(&mut xref)?;

    Ok((xref, trailer))
}

/// Reconcile `/Size` with both the highest live definition and the highest
/// retained xref row. A free highest-numbered row may validly separate them.
fn normalize_declared_size(reference_table: &mut Xref) -> Result<()> {
    let lowest = reference_table.max_id().checked_add(1).ok_or(ParseError::InvalidXref)?;
    let highest = reference_table
        .max_entry_id()
        .checked_add(1)
        .ok_or(ParseError::InvalidXref)?;
    let corrected = reference_table.size.clamp(lowest, highest);
    if reference_table.size != corrected {
        warn!(
            "Size entry of trailer dictionary is {}, correct value is {}.",
            reference_table.size, corrected
        );
        reference_table.size = corrected;
    }
    Ok(())
}

/// Overlay one hybrid-reference supplement on the classic rows for the same
/// revision. A damaged supplement is optional and degrades to the classic
/// section, which remains a complete table for readers without xref streams.
fn apply_hybrid_supplement(
    reader: &Reader<'_>, section: &mut Xref, start: Option<Object>, already_seen: &mut HashSet<i64>,
) {
    let Some(start) = start.and_then(|offset| offset.as_i64().ok()) else {
        return;
    };
    if start < 0 || start as usize > reader.buffer.len() {
        warn!("XRefStm {start} is outside the file; the hybrid-reference supplement is ignored.");
        return;
    }
    if !already_seen.insert(start) {
        return;
    }

    match reader.xref_and_trailer_at(start as usize) {
        Ok((supplement, _)) => section.supersede(supplement),
        Err(error) => {
            warn!("XRefStm {start} could not be read ({error}); the hybrid-reference supplement is ignored.")
        }
    }
}
