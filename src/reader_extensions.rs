//! Fork-owned eager-reader behavior kept outside upstream's hot reader module.

use std::collections::{BTreeMap, HashSet};

use log::warn;

use crate::error::{ParseError, XrefError};
use crate::reader::{Reader, free_ids};
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
    let (xref_start, mut xref, mut trailer) = reader.xref_and_trailer_at(xref_start)?;
    reader.document.xref_start = xref_start;
    reader.free_without_xref_stream.clear();

    // `/XRefStm` describes its own revision. Apply each supplement before
    // merging that section so compressed rows supersede compatibility rows.
    let mut already_seen_supplements = HashSet::new();
    let xref_stream_start = trailer.remove(b"XRefStm");
    if apply_hybrid_supplement(reader, &mut xref, xref_stream_start, &mut already_seen_supplements)? {
        reader.free_without_xref_stream.extend(free_ids(&xref));
    }
    let mut sections = 1;

    // A `/Prev` that leads back to the section `startxref` names ends the chain
    // like any other cycle.
    let mut already_seen = HashSet::new();
    already_seen.insert(xref_start as i64);
    let mut prev_xref_start = trailer.remove(b"Prev");
    while let Some(prev) = prev_xref_start.take().and_then(|offset| offset.as_i64().ok()) {
        if !already_seen.insert(prev) {
            break;
        }
        if prev < 0 || prev as usize > reader.buffer.len() {
            return Err(Error::Xref(XrefError::PrevStart));
        }

        let (_, mut prev_xref, mut prev_trailer) = reader.xref_and_trailer_at(prev as usize)?;
        sections += 1;
        let prev_xref_stream_start = prev_trailer.remove(b"XRefStm");
        if apply_hybrid_supplement(
            reader,
            &mut prev_xref,
            prev_xref_stream_start,
            &mut already_seen_supplements,
        )? {
            // Only the free entries that stay: a newer section's entry wins the merge below.
            let hidden: Vec<u32> = free_ids(&prev_xref)
                .filter(|id| !xref.entries.contains_key(id))
                .collect();
            reader.free_without_xref_stream.extend(hidden);
        }
        xref.merge(prev_xref);
        prev_xref_start = prev_trailer.remove(b"Prev");
    }
    normalize_declared_size(&mut xref)?;
    reader.document.xref_section_count = sections;

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
/// revision. A damaged supplement is optional in lenient mode and degrades to the
/// classic section, which remains a complete table for readers without xref
/// streams; strict mode refuses it. Returns whether the section names a
/// supplement that could not be read and was ignored, in which case its free
/// rows may be placeholders for objects that only the supplement located.
fn apply_hybrid_supplement(
    reader: &mut Reader<'_>, section: &mut Xref, start: Option<Object>, already_seen: &mut HashSet<i64>,
) -> Result<bool> {
    let Some(start) = start.and_then(|offset| offset.as_i64().ok()) else {
        return Ok(false);
    };
    if start < 0 || start as usize > reader.buffer.len() {
        if reader.strict {
            return Err(Error::Xref(XrefError::StreamStart));
        }
        warn!("XRefStm {start} is outside the file; the hybrid-reference supplement is ignored.");
        return Ok(true);
    }
    if !already_seen.insert(start) {
        return Ok(false);
    }

    match reader.xref_and_trailer_at(start as usize) {
        Ok((_, supplement, _)) => {
            section.supersede(supplement);
            Ok(false)
        }
        Err(error) if reader.strict => Err(error),
        Err(error) => {
            warn!("XRefStm {start} could not be read ({error}); the hybrid-reference supplement is ignored.");
            Ok(true)
        }
    }
}
