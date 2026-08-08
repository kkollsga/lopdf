//! Opening a document: header, xref chain, recovery rescan, and encryption bootstrap.

use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ObjectLocation64 {
    // The eager classic-xref parser currently drops free entries. The indexed
    // representation deliberately retains them so a later writer/recovery
    // layer does not have to reconstruct the free list.
    Free { next: u64, generation: u16 },
    Normal { offset: u64, generation: u16 },
    Compressed { container: u32, index: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexXrefType {
    Table,
    Stream,
}

pub(crate) struct PdfIndex {
    pub(crate) version: String,
    pub(crate) source_len: u64,
    pub(crate) source_origin: u64,
    pub(crate) xref_start: u64,
    pub(crate) xref_type: IndexXrefType,
    pub(crate) declared_size: u64,
    pub(crate) locations: BTreeMap<u32, ObjectLocation64>,
    pub(crate) trailer: Dictionary,
    pub(crate) encryption_state: Option<EncryptionState>,
    pub(crate) encrypt_object_id: Option<crate::ObjectId>,
    /// Whether this index came from a body rescan rather than a cross-reference
    /// section, so a caller can count and report the provenance of the open.
    pub(crate) recovered: bool,
}

fn authenticate_password(
    algorithm: &PasswordAlgorithm, file_id: Option<&[u8]>, password: &[u8],
) -> IndexedReaderResult<Option<Vec<u8>>> {
    let user = algorithm.authenticate_user_password_with_file_id(file_id, password);
    if user.is_ok() {
        return Ok(Some(password.to_vec()));
    }
    let owner = if (2..=4).contains(&algorithm.revision) {
        algorithm.recover_user_password_with_file_id(file_id, password)
    } else {
        algorithm
            .authenticate_owner_password_with_file_id(file_id, password)
            .map(|()| password.to_vec())
    };
    match owner {
        Ok(password) => Ok(Some(password)),
        Err(owner_error) => match user {
            Err(crate::encryption::DecryptionError::IncorrectPassword)
                if matches!(owner_error, crate::encryption::DecryptionError::IncorrectPassword) =>
            {
                Ok(None)
            }
            Err(crate::encryption::DecryptionError::IncorrectPassword) => {
                Err(IndexedReaderError::Encryption(crate::Error::Decryption(owner_error)))
            }
            Err(user_error) => Err(IndexedReaderError::Encryption(crate::Error::Decryption(user_error))),
            Ok(()) => unreachable!(),
        },
    }
}

impl PdfIndex {
    pub(crate) fn open(source: Arc<dyn RandomAccessSource>) -> IndexedReaderResult<Self> {
        let source_len = source.len()?;
        let (source_origin, version) = read_header(source.as_ref(), source_len)?;
        match Self::open_from_xref(&source, source_len, source_origin, &version) {
            Ok(index) => Ok(index),
            Err(error) if error_admits_xref_recovery(&error) => {
                // The cross-reference machinery is unusable, but the body may be
                // intact. Rebuilding the index from a scan keeps the document on
                // the indexed route rather than surrendering it to a reader that
                // must materialize the whole file. A recovery that cannot prove
                // it found a usable catalog reports the original failure, so a
                // caller's own fallback still sees exactly what it saw before.
                recover_index(source.as_ref(), source_len, source_origin, version).map_err(|_| error)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn open_from_xref(
        source: &Arc<dyn RandomAccessSource>, source_len: u64, source_origin: u64, version: &str,
    ) -> IndexedReaderResult<Self> {
        let version = version.to_owned();
        let xref_start = read_startxref(source.as_ref(), source_len)?;
        let logical_len = source_len
            .checked_sub(source_origin)
            .ok_or(IndexedReaderError::InvalidHeader {
                limit: HEADER_SCAN_LIMIT,
            })?;
        if xref_start > logical_len {
            return Err(IndexedReaderError::StartXrefOutOfBounds {
                offset: xref_start,
                logical_len,
            });
        }
        // The logical bound above makes this addition mathematically safe, but
        // keep it checked so a prefixed source can never wrap before I/O if the
        // surrounding invariants change.
        source_origin
            .checked_add(xref_start)
            .ok_or(IndexedReaderError::StartXrefOutOfBounds {
                offset: xref_start,
                logical_len,
            })?;

        let mut locations = BTreeMap::new();
        let mut seen = HashSet::new();
        let mut next = Some(xref_start);
        let mut newest_trailer = None;
        let mut newest_type = None;
        let mut declared_size = 0_u64;
        let mut revisions = 0_usize;

        while let Some(offset) = next {
            if !seen.insert(offset) {
                break;
            }
            revisions = revisions
                .checked_add(1)
                .ok_or(IndexedReaderError::RevisionLimitExceeded {
                    limit: MAX_XREF_REVISIONS,
                })?;
            if revisions > MAX_XREF_REVISIONS {
                return Err(IndexedReaderError::RevisionLimitExceeded {
                    limit: MAX_XREF_REVISIONS,
                });
            }

            let physical = source_origin
                .checked_add(offset)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let section = read_xref_section(source.as_ref(), source_len, physical)?;
            if newest_trailer.is_none() {
                newest_type = Some(section.kind);
                newest_trailer = Some(section.trailer.clone());
                declared_size =
                    trailer_unsigned(&section.trailer, b"Size").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
            }
            let mut entries = section.entries;

            // A hybrid-reference table supplements its own revision: it lists
            // the compressed objects that the classic section is required to
            // mask as free, so within that revision the supplement takes
            // precedence (ISO 32000-1, 7.5.8.4). The revision as a whole still
            // wins over everything older down the `/Prev` chain.
            if let Some(hybrid) = trailer_offset(&section.trailer, b"XRefStm", "XRefStm")? {
                let hybrid_physical = source_origin
                    .checked_add(hybrid)
                    .ok_or(IndexedReaderError::InvalidTrailerOffset { key: "XRefStm" })?;
                let supplement = read_xref_section(source.as_ref(), source_len, hybrid_physical)?;
                if supplement.kind != IndexXrefType::Stream {
                    return Err(IndexedReaderError::InvalidXref { offset: hybrid });
                }
                entries.extend(supplement.entries);
            }
            merge_newest(&mut locations, entries);

            next = trailer_offset(&section.trailer, b"Prev", "Prev")?;
        }

        let trailer = newest_trailer.ok_or(IndexedReaderError::InvalidXref { offset: xref_start })?;
        Ok(Self {
            version,
            source_len,
            source_origin,
            xref_start,
            xref_type: newest_type.ok_or(IndexedReaderError::InvalidXref { offset: xref_start })?,
            declared_size,
            locations,
            trailer,
            encryption_state: None,
            encrypt_object_id: None,
            recovered: false,
        })
    }
}

struct XrefSection64 {
    kind: IndexXrefType,
    pub(super) entries: BTreeMap<u32, ObjectLocation64>,
    trailer: Dictionary,
}

fn merge_newest(target: &mut BTreeMap<u32, ObjectLocation64>, entries: BTreeMap<u32, ObjectLocation64>) {
    for (id, entry) in entries {
        target.entry(id).or_insert(entry);
    }
}

pub(super) fn read_header(source: &dyn RandomAccessSource, source_len: u64) -> IndexedReaderResult<(u64, String)> {
    let read_limit = HEADER_SCAN_LIMIT
        .checked_add(HEADER_PARSE_OVERLAP)
        .ok_or(IndexedReaderError::InvalidHeader {
            limit: HEADER_SCAN_LIMIT,
        })?;
    let length = source_len.min(read_limit);
    let bytes = source.read_range(0, length, read_limit)?;
    let origin = bytes
        .windows(5)
        .position(|window| window == b"%PDF-")
        .filter(|origin| {
            u64::try_from(*origin)
                .ok()
                .is_some_and(|origin| origin < HEADER_SCAN_LIMIT)
        })
        .ok_or(IndexedReaderError::InvalidHeader {
            limit: HEADER_SCAN_LIMIT,
        })?;
    let version = crate::parser::header(&bytes[origin..], false).ok_or(IndexedReaderError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    let origin = u64::try_from(origin).map_err(|_| IndexedReaderError::InvalidHeader {
        limit: HEADER_SCAN_LIMIT,
    })?;
    Ok((origin, version))
}

/// Whether `error` says the cross-reference machinery is unusable — the only
/// class of failure a body rescan can answer.
///
/// A source failure, a resource limit or a malformed header is deliberately not
/// in the set: rescanning would not fix any of them, and a healthy document
/// never reaches recovery at all.
fn error_admits_xref_recovery(error: &IndexedReaderError) -> bool {
    matches!(
        error,
        IndexedReaderError::InvalidStartXref { .. }
            | IndexedReaderError::StartXrefOutOfBounds { .. }
            | IndexedReaderError::InvalidXref { .. }
    )
}

/// Bytes of the recovery scan's sliding window, one physical read each.
pub(super) const RECOVERY_CHUNK_BYTES: u64 = 64 * 1_024;

/// Bytes retained ahead of a window's scan boundary so a token that straddles
/// two reads is still matched whole.
const RECOVERY_LOOKAHEAD_BYTES: usize = 16;

/// Bytes retained behind a window's scan boundary so the object number and
/// generation preceding a matched `obj` are still in the window.
const RECOVERY_LOOKBEHIND_BYTES: usize = 64;

/// Upper bound on recovered object headers, so a hostile source cannot turn the
/// offsets map into unbounded growth.
const MAX_RECOVERED_OBJECTS: usize = 1 << 21;

/// Newest classic trailers retained as candidates during the scan.
const MAX_RECOVERY_TRAILER_CANDIDATES: usize = 32;

/// Recovered objects probed, newest first, for a trailer when the file carries
/// no classic `trailer` keyword.
const MAX_RECOVERY_ROOT_PROBES: usize = 256;

/// Bytes read to parse one trailer or object dictionary during recovery.
const RECOVERY_DICTIONARY_WINDOW_BYTES: u64 = 8 * 1_024;

/// Rebuild an index by scanning the body for `N G obj` headers.
///
/// The scan is one forward pass through the same chunked physical-read path the
/// resolver uses — 64 KiB at a time into a reused window, never the whole file —
/// so a document recovers without giving up the memory profile that made the
/// indexed route worth taking. Retention is the offsets map itself, which is
/// O(live objects) and capped.
///
/// A later header for an object number wins, which is how an incremental update
/// supersedes the revision it was appended to.
fn recover_index(
    source: &dyn RandomAccessSource, source_len: u64, source_origin: u64, version: String,
) -> IndexedReaderResult<PdfIndex> {
    let scan = scan_for_indirect_objects(source, source_len, source_origin)?;
    if scan.locations.is_empty() {
        return Err(IndexedReaderError::InvalidXref { offset: 0 });
    }
    let (trailer, root) = recover_trailer(source, source_len, source_origin, &scan)?;
    // A trailer is only worth having if the catalog it names was actually found
    // by the scan. Anything else — a catalog inside an object stream the scan
    // cannot expand, a `/Root` into a revision that is gone — would open a
    // document that resolves to nothing, which is strictly worse than reporting
    // the original cross-reference failure.
    if !matches!(scan.locations.get(&root.0), Some(ObjectLocation64::Normal { .. })) {
        return Err(IndexedReaderError::InvalidXref { offset: 0 });
    }
    let declared_size = scan
        .locations
        .keys()
        .next_back()
        .and_then(|highest| u64::from(*highest).checked_add(1))
        .unwrap_or_default();
    Ok(PdfIndex {
        version,
        source_len,
        source_origin,
        // No cross-reference section survived; there is no offset to report.
        xref_start: 0,
        xref_type: IndexXrefType::Table,
        declared_size,
        locations: scan.locations,
        trailer,
        encryption_state: None,
        encrypt_object_id: None,
        recovered: true,
    })
}

struct RecoveryScan {
    locations: BTreeMap<u32, ObjectLocation64>,
    /// Logical offsets just past a classic `trailer` keyword, newest last.
    trailer_offsets: VecDeque<u64>,
    /// Whether the body mentions `/Encrypt` anywhere. A synthesized trailer
    /// cannot carry an encryption dictionary, so seeing this forbids synthesis.
    saw_encrypt: bool,
}

fn scan_for_indirect_objects(
    source: &dyn RandomAccessSource, source_len: u64, source_origin: u64,
) -> IndexedReaderResult<RecoveryScan> {
    let mut scan = RecoveryScan {
        locations: BTreeMap::new(),
        trailer_offsets: VecDeque::new(),
        saw_encrypt: false,
    };
    // `window` holds the bytes physically at [base, base + window.len()); it is
    // reused across reads and never exceeds one chunk plus the straddle margin.
    let mut window: Vec<u8> = Vec::new();
    let mut base = source_origin;
    let mut next_read = source_origin;
    let mut scanned = source_origin;
    while next_read < source_len {
        let chunk = read_window(source, source_len, next_read, RECOVERY_CHUNK_BYTES)?;
        if chunk.is_empty() {
            break;
        }
        next_read = next_read.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        window.extend_from_slice(&chunk);
        drop(chunk);
        let final_chunk = next_read >= source_len;
        let scan_end = if final_chunk {
            window.len()
        } else {
            window.len().saturating_sub(RECOVERY_LOOKAHEAD_BYTES)
        };
        let scan_start = usize::try_from(scanned.saturating_sub(base)).unwrap_or(window.len());
        if scan_start < scan_end {
            scan_window(&window[..scan_end], scan_start, base, &mut scan)?;
        }
        scanned = base.saturating_add(u64::try_from(scan_end).unwrap_or(0));
        let keep = window.len().min(RECOVERY_LOOKAHEAD_BYTES + RECOVERY_LOOKBEHIND_BYTES);
        let drained = window.len() - keep;
        window.drain(..drained);
        base = base.saturating_add(u64::try_from(drained).unwrap_or(0));
    }
    Ok(scan)
}

/// Match every `obj` header and `trailer` keyword whose keyword starts at or
/// after `from` in `window`, whose first byte lies at physical offset `base`.
fn scan_window(window: &[u8], from: usize, base: u64, scan: &mut RecoveryScan) -> IndexedReaderResult<()> {
    for index in from..window.len() {
        if window[index..].starts_with(b"obj") {
            if !is_token_boundary(window.get(index + 3).copied()) {
                continue;
            }
            if let Some((id, header_start)) = indirect_header_before(window, index) {
                let offset = base
                    .checked_add(u64::try_from(header_start).unwrap_or(u64::MAX))
                    .ok_or(IndexedReaderError::InvalidXref { offset: base })?;
                if scan.locations.len() >= MAX_RECOVERED_OBJECTS && !scan.locations.contains_key(&id.0) {
                    return Err(IndexedReaderError::InvalidXref { offset });
                }
                scan.locations.insert(
                    id.0,
                    ObjectLocation64::Normal {
                        offset,
                        generation: id.1,
                    },
                );
            }
        } else if window[index..].starts_with(b"trailer") {
            if !is_token_boundary(window.get(index + 7).copied())
                || !is_token_start_boundary(index.checked_sub(1).map(|before| window[before]))
            {
                continue;
            }
            let offset = base
                .checked_add(u64::try_from(index + 7).unwrap_or(u64::MAX))
                .ok_or(IndexedReaderError::InvalidXref { offset: base })?;
            if scan.trailer_offsets.len() == MAX_RECOVERY_TRAILER_CANDIDATES {
                scan.trailer_offsets.pop_front();
            }
            scan.trailer_offsets.push_back(offset);
        } else if window[index..].starts_with(b"/Encrypt") {
            scan.saw_encrypt = true;
        }
    }
    Ok(())
}

/// Read the `N G` pair immediately before the `obj` keyword at `keyword`,
/// returning the object id and where its header starts.
fn indirect_header_before(window: &[u8], keyword: usize) -> Option<(crate::ObjectId, usize)> {
    let mut cursor = keyword;
    cursor = skip_whitespace_back(window, cursor)?;
    let (generation, cursor) = digits_back(window, cursor)?;
    let cursor = skip_whitespace_back(window, cursor)?;
    let (number, start) = digits_back(window, cursor)?;
    if !is_token_start_boundary(start.checked_sub(1).map(|before| window[before])) {
        return None;
    }
    Some(((u32::try_from(number).ok()?, u16::try_from(generation).ok()?), start))
}

/// Step back over at least one whitespace byte, returning the index just past
/// the run's first byte.
fn skip_whitespace_back(window: &[u8], end: usize) -> Option<usize> {
    let mut cursor = end;
    while cursor > 0 && is_pdf_whitespace(window[cursor - 1]) {
        cursor -= 1;
    }
    (cursor < end).then_some(cursor)
}

/// Read an ASCII digit run ending at `end`, returning its value and start.
fn digits_back(window: &[u8], end: usize) -> Option<(u64, usize)> {
    let mut cursor = end;
    while cursor > 0 && window[cursor - 1].is_ascii_digit() {
        cursor -= 1;
    }
    if cursor == end {
        return None;
    }
    // A run longer than 20 digits cannot be a PDF object number and would only
    // overflow the parse.
    let digits = &window[cursor..end];
    if digits.len() > 20 {
        return None;
    }
    std::str::from_utf8(digits)
        .ok()?
        .parse()
        .ok()
        .map(|value| (value, cursor))
}

fn is_token_start_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| is_pdf_whitespace(byte) || is_pdf_delimiter(byte))
}

/// Find the newest trailer the scan can prove usable, and the `/Root` it names.
fn recover_trailer(
    source: &dyn RandomAccessSource, source_len: u64, source_origin: u64, scan: &RecoveryScan,
) -> IndexedReaderResult<(Dictionary, crate::ObjectId)> {
    for offset in scan.trailer_offsets.iter().rev() {
        let Some(dictionary) = read_dictionary_at(source, source_len, *offset) else {
            continue;
        };
        if let Ok(root) = dictionary.get(b"Root").and_then(Object::as_reference) {
            return Ok((dictionary, root));
        }
    }
    // A cross-reference-stream file has no `trailer` keyword: its trailer *is*
    // an `/Type /XRef` stream dictionary. Probe the recovered objects newest
    // first, which is the revision order an incremental update appends in.
    let mut newest: Vec<(u64, u32)> = scan
        .locations
        .iter()
        .filter_map(|(id, location)| match location {
            ObjectLocation64::Normal { offset, .. } => Some((*offset, *id)),
            _ => None,
        })
        .collect();
    newest.sort_unstable_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    let mut catalog = None;
    for (offset, id) in newest.into_iter().take(MAX_RECOVERY_ROOT_PROBES) {
        let physical = offset;
        let Some(dictionary) = read_object_dictionary_at(source, source_len, physical) else {
            continue;
        };
        let kind = dictionary.get(b"Type").and_then(Object::as_name).ok();
        if kind == Some(b"XRef")
            && let Ok(root) = dictionary.get(b"Root").and_then(Object::as_reference)
        {
            return Ok((dictionary, root));
        }
        if kind == Some(b"Catalog") && catalog.is_none() {
            catalog = Some(id);
        }
    }
    // Last resort: name the catalog the scan found. A synthesized trailer cannot
    // carry `/Encrypt`, so refuse if the body mentions one rather than open a
    // document whose strings and streams would decode to ciphertext.
    let Some(catalog) = catalog.filter(|_| !scan.saw_encrypt) else {
        return Err(IndexedReaderError::InvalidXref { offset: source_origin });
    };
    let mut trailer = Dictionary::new();
    trailer.set("Root", Object::Reference((catalog, 0)));
    Ok((trailer, (catalog, 0)))
}

fn read_dictionary_at(source: &dyn RandomAccessSource, source_len: u64, offset: u64) -> Option<Dictionary> {
    let window = read_window(source, source_len, offset, RECOVERY_DICTIONARY_WINDOW_BYTES).ok()?;
    read_dictionary_slice(&window)
}

fn read_object_dictionary_at(source: &dyn RandomAccessSource, source_len: u64, offset: u64) -> Option<Dictionary> {
    let window = read_window(source, source_len, offset, RECOVERY_DICTIONARY_WINDOW_BYTES).ok()?;
    let (_, consumed) = parse_indirect_header(&window)?;
    read_dictionary_slice(window.get(consumed..)?)
}

/// Parse the first direct object in `input`, which the parser itself will not
/// do across leading whitespace or a comment.
fn read_dictionary_slice(input: &[u8]) -> Option<Dictionary> {
    match TokenCursor::new(input).direct_object()? {
        Object::Dictionary(dictionary) => Some(dictionary),
        _ => None,
    }
}

pub(super) fn read_startxref(source: &dyn RandomAccessSource, source_len: u64) -> IndexedReaderResult<u64> {
    let length = source_len.min(TAIL_SCAN_LIMIT);
    let offset = source_len
        .checked_sub(length)
        .ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let tail = source.read_range(offset, length, TAIL_SCAN_LIMIT)?;
    let eof = rfind(&tail, b"%%EOF").ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let marker =
        rfind(&tail[..eof], b"startxref").ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })?;
    let mut cursor = TokenCursor::new(&tail[marker + b"startxref".len()..]);
    cursor
        .unsigned()
        .ok_or(IndexedReaderError::InvalidStartXref { limit: TAIL_SCAN_LIMIT })
}

fn read_xref_section(
    source: &dyn RandomAccessSource, source_len: u64, physical_offset: u64,
) -> IndexedReaderResult<XrefSection64> {
    if physical_offset >= source_len {
        return Err(IndexedReaderError::InvalidXref {
            offset: physical_offset,
        });
    }
    let remaining = source_len
        .checked_sub(physical_offset)
        .ok_or(IndexedReaderError::InvalidXref {
            offset: physical_offset,
        })?;
    let maximum = remaining.min(XREF_WINDOW_LIMIT);
    let mut length = maximum.min(XREF_INITIAL_WINDOW);
    loop {
        let window = source.read_range(physical_offset, length, length)?;
        let result = if starts_with_token(&window, b"xref") {
            parse_classic_xref(&window, physical_offset)
        } else {
            parse_xref_stream(&window, physical_offset)
        };
        match result {
            Ok(section) => return Ok(section),
            Err(IndexedReaderError::IncompleteXref { .. }) if length < maximum => {
                length = length.saturating_mul(2).min(maximum);
            }
            Err(IndexedReaderError::IncompleteXref { .. }) if remaining > XREF_WINDOW_LIMIT => {
                return Err(IndexedReaderError::StructureLimitExceeded {
                    structure: "cross-reference section",
                    limit: XREF_WINDOW_LIMIT,
                });
            }
            Err(IndexedReaderError::IncompleteXref { .. }) => {
                return Err(IndexedReaderError::InvalidXref {
                    offset: physical_offset,
                });
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_classic_xref(window: &[u8], offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_token(&mut cursor, b"xref", offset)?;
    let mut entries = BTreeMap::new();
    let mut entry_count = 0_u64;

    loop {
        cursor.skip_space();
        if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"trailer".len() && b"trailer".starts_with(cursor.remaining()))
        {
            return Err(IndexedReaderError::IncompleteXref { offset });
        }
        if cursor.consume(b"trailer") {
            let trailer = match cursor.direct_object() {
                Some(Object::Dictionary(dictionary)) => dictionary,
                Some(_) => return Err(IndexedReaderError::InvalidTrailer { offset }),
                None if dictionary_may_be_truncated(cursor.remaining()) => {
                    return Err(IndexedReaderError::IncompleteXref { offset });
                }
                None => return Err(IndexedReaderError::InvalidTrailer { offset }),
            };
            return Ok(XrefSection64 {
                kind: IndexXrefType::Table,
                entries,
                trailer,
            });
        }

        let start = required_xref_unsigned(&mut cursor, offset)?;
        let count = required_xref_unsigned(&mut cursor, offset)?;
        entry_count = entry_count
            .checked_add(count)
            .ok_or(IndexedReaderError::EntryLimitExceeded {
                count: u64::MAX,
                limit: MAX_XREF_ENTRIES,
            })?;
        check_entry_limit(entry_count)?;

        for index in 0..count {
            let object_number = start
                .checked_add(index)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            let field = required_xref_unsigned(&mut cursor, offset)?;
            let generation = required_xref_unsigned(&mut cursor, offset)?;
            let generation = u16::try_from(generation).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            cursor.skip_space();
            if cursor.remaining().is_empty() {
                return Err(IndexedReaderError::IncompleteXref { offset });
            }
            let state = cursor.token().ok_or(IndexedReaderError::InvalidXref { offset })?;
            let location = match state {
                b"n" => ObjectLocation64::Normal {
                    offset: field,
                    generation,
                },
                b"f" => ObjectLocation64::Free {
                    next: field,
                    generation,
                },
                _ => return Err(IndexedReaderError::InvalidXref { offset }),
            };
            entries.insert(object_number, location);
        }
    }
}

fn parse_xref_stream(window: &[u8], offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut cursor = TokenCursor::new(window);
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_unsigned(&mut cursor, offset)?;
    required_xref_token(&mut cursor, b"obj", offset)?;
    let dictionary = match cursor.direct_object() {
        Some(Object::Dictionary(dictionary)) => dictionary,
        Some(_) => return Err(IndexedReaderError::InvalidTrailer { offset }),
        None if dictionary_may_be_truncated(cursor.remaining()) => {
            return Err(IndexedReaderError::IncompleteXref { offset });
        }
        None => return Err(IndexedReaderError::InvalidTrailer { offset }),
    };
    required_xref_token(&mut cursor, b"stream", offset)?;
    if cursor.consume_stream_eol().is_none() {
        return if cursor.remaining().is_empty() {
            Err(IndexedReaderError::IncompleteXref { offset })
        } else {
            Err(IndexedReaderError::InvalidXref { offset })
        };
    }

    let stream_len = trailer_unsigned(&dictionary, b"Length").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
    let stream_len = usize::try_from(stream_len).map_err(|_| IndexedReaderError::StructureLimitExceeded {
        structure: "cross-reference stream",
        limit: XREF_WINDOW_LIMIT,
    })?;
    if stream_len > cursor.remaining().len() {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    let content = cursor
        .take(stream_len)
        .ok_or(IndexedReaderError::IncompleteXref { offset })?
        .to_vec();
    cursor.consume_optional_eol();
    if !cursor.consume_exact(b"endstream") {
        return if cursor.remaining().is_empty()
            || (cursor.remaining().len() < b"endstream".len() && b"endstream".starts_with(cursor.remaining()))
        {
            Err(IndexedReaderError::IncompleteXref { offset })
        } else {
            Err(IndexedReaderError::InvalidXref { offset })
        };
    }
    let mut stream = Stream::new(dictionary.clone(), content);
    if stream.is_compressed() {
        stream
            .decompress_with_limit(XREF_DECOMPRESSED_LIMIT)
            .map_err(IndexedReaderError::XrefDecompression)?;
    }
    decode_xref_stream64(stream, offset)
}

fn decode_xref_stream64(stream: Stream, offset: u64) -> IndexedReaderResult<XrefSection64> {
    let mut trailer = stream.dict;
    let size = trailer_unsigned(&trailer, b"Size").ok_or(IndexedReaderError::InvalidTrailer { offset })?;
    let widths = integer_array(&trailer, b"W").ok_or(IndexedReaderError::InvalidXref { offset })?;
    if widths.len() < 3 || widths[..3].iter().any(|width| *width > MAX_XREF_FIELD_WIDTH) {
        return Err(IndexedReaderError::InvalidXref { offset });
    }
    let indices = integer_array(&trailer, b"Index").unwrap_or_else(|| vec![0, size]);
    if !indices.chunks_exact(2).remainder().is_empty() {
        return Err(IndexedReaderError::InvalidXref { offset });
    }

    let mut total = 0_u64;
    for pair in indices.chunks_exact(2) {
        total = total
            .checked_add(pair[1])
            .ok_or(IndexedReaderError::EntryLimitExceeded {
                count: u64::MAX,
                limit: MAX_XREF_ENTRIES,
            })?;
    }
    check_entry_limit(total)?;

    let entry_width = widths[..3]
        .iter()
        .try_fold(0_u64, |sum, width| sum.checked_add(*width))
        .ok_or(IndexedReaderError::InvalidXref { offset })?;
    let required = total
        .checked_mul(entry_width)
        .ok_or(IndexedReaderError::InvalidXref { offset })?;
    let content_len = u64::try_from(stream.content.len()).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
    if required > content_len {
        return Err(IndexedReaderError::InvalidXref { offset });
    }

    let mut content = stream.content.as_slice();
    let mut entries = BTreeMap::new();
    for pair in indices.chunks_exact(2) {
        let start = pair[0];
        let count = pair[1];
        for index in 0..count {
            let kind = if widths[0] == 0 {
                1
            } else {
                read_be(&mut content, widths[0], offset)?
            };
            let field2 = read_be(&mut content, widths[1], offset)?;
            let field3 = read_be(&mut content, widths[2], offset)?;
            let object_number = start
                .checked_add(index)
                .ok_or(IndexedReaderError::InvalidXref { offset })?;
            let object_number = u32::try_from(object_number).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
            let location = match kind {
                0 => ObjectLocation64::Free {
                    next: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                },
                1 => ObjectLocation64::Normal {
                    offset: field2,
                    generation: u16::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                },
                2 => ObjectLocation64::Compressed {
                    container: u32::try_from(field2).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                    index: u32::try_from(field3).map_err(|_| IndexedReaderError::InvalidXref { offset })?,
                },
                _ => continue,
            };
            entries.insert(object_number, location);
        }
    }

    trailer.remove(b"Length");
    trailer.remove(b"W");
    trailer.remove(b"Index");
    Ok(XrefSection64 {
        kind: IndexXrefType::Stream,
        entries,
        trailer,
    })
}

fn read_be(input: &mut &[u8], width: u64, offset: u64) -> IndexedReaderResult<u64> {
    let width = usize::try_from(width).map_err(|_| IndexedReaderError::InvalidXref { offset })?;
    let bytes = input.get(..width).ok_or(IndexedReaderError::InvalidXref { offset })?;
    *input = input.get(width..).ok_or(IndexedReaderError::InvalidXref { offset })?;
    Ok(bytes.iter().fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

pub(super) fn check_entry_limit(count: u64) -> IndexedReaderResult<()> {
    if count > MAX_XREF_ENTRIES {
        return Err(IndexedReaderError::EntryLimitExceeded {
            count,
            limit: MAX_XREF_ENTRIES,
        });
    }
    Ok(())
}

fn integer_array(dictionary: &Dictionary, key: &[u8]) -> Option<Vec<u64>> {
    dictionary
        .get(key)
        .ok()?
        .as_array()
        .ok()?
        .iter()
        .map(|object| object.as_i64().ok().and_then(|value| u64::try_from(value).ok()))
        .collect()
}

fn trailer_unsigned(dictionary: &Dictionary, key: &[u8]) -> Option<u64> {
    dictionary
        .get(key)
        .ok()?
        .as_i64()
        .ok()
        .and_then(|value| u64::try_from(value).ok())
}

fn trailer_offset(dictionary: &Dictionary, key: &[u8], name: &'static str) -> IndexedReaderResult<Option<u64>> {
    match dictionary.get(key) {
        Ok(object) => object
            .as_i64()
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .map(Some)
            .ok_or(IndexedReaderError::InvalidTrailerOffset { key: name }),
        Err(_) => Ok(None),
    }
}

fn required_xref_token(cursor: &mut TokenCursor<'_>, expected: &[u8], offset: u64) -> IndexedReaderResult<()> {
    cursor.skip_space();
    if cursor.remaining().is_empty()
        || (cursor.remaining().len() < expected.len() && expected.starts_with(cursor.remaining()))
    {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    cursor
        .expect(expected)
        .ok_or(IndexedReaderError::InvalidXref { offset })
}

fn required_xref_unsigned(cursor: &mut TokenCursor<'_>, offset: u64) -> IndexedReaderResult<u64> {
    cursor.skip_space();
    if cursor.remaining().is_empty() {
        return Err(IndexedReaderError::IncompleteXref { offset });
    }
    let token = cursor.token().ok_or(IndexedReaderError::InvalidXref { offset })?;
    if !token.iter().all(u8::is_ascii_digit) {
        return Err(IndexedReaderError::InvalidXref { offset });
    }
    std::str::from_utf8(token)
        .ok()
        .and_then(|token| token.parse().ok())
        .ok_or(IndexedReaderError::InvalidXref { offset })
}

impl IndexedReader {
    /// Open a source with the default bounded options.
    pub fn open<S: RandomAccessSource>(source: S) -> IndexedReaderResult<Self> {
        Self::open_with_options(source, IndexedReaderOptions::default())
    }

    /// Open a source with explicit resource limits and password handling.
    pub fn open_with_options<S: RandomAccessSource>(
        source: S, options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        Self::open_shared(Arc::new(source), options)
    }

    /// Open a shared source without adding another source allocation.
    pub fn open_shared(
        source: Arc<dyn RandomAccessSource>, options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        Self::from_erased_source(source, options)
    }

    fn from_erased_source(
        source: Arc<dyn RandomAccessSource>, mut options: IndexedReaderOptions,
    ) -> IndexedReaderResult<Self> {
        let index = PdfIndex::open(Arc::clone(&source))?;
        let limits = ResolverLimits::from(&options);
        let password = options.password.take();
        let mut reader = Self {
            source,
            cached_source: None,
            index: Arc::new(index),
            limits,
            options,
            object_cache: None,
            object_stream_cache: None,
            cache_counters: None,
        };
        reader.initialize_encryption(password.as_deref())?;
        Ok(reader)
    }

    /// Open a source with explicit reader limits and a bounded cache budget.
    pub fn open_cached<S: RandomAccessSource>(
        source: S, options: IndexedReaderOptions, cache_options: IndexedReaderCacheOptions,
    ) -> IndexedReaderResult<Self> {
        Self::open_shared_cached(Arc::new(source), options, cache_options)
    }

    /// Open a shared source with explicit reader limits and a bounded cache budget.
    pub fn open_shared_cached(
        source: Arc<dyn RandomAccessSource>, options: IndexedReaderOptions, cache_options: IndexedReaderCacheOptions,
    ) -> IndexedReaderResult<Self> {
        let cached_source = Arc::new(CachedSource::new(source, cache_options)?);
        let erased: Arc<dyn RandomAccessSource> = cached_source.clone();
        let mut reader = Self::from_erased_source(erased, options)?;
        reader.cached_source = Some(cached_source);
        reader.configure_resolution_caches(
            usize_from_u64_saturating(cache_options.object_max_bytes()),
            cache_options.object_max_entries(),
            usize_from_u64_saturating(cache_options.object_stream_max_bytes()),
            cache_options.object_stream_max_entries(),
        );
        Ok(reader)
    }
}

impl IndexedReader {
    pub(super) fn open_with_limits(
        source: Arc<dyn RandomAccessSource>, limits: ResolverLimits,
    ) -> IndexedReaderResult<Self> {
        let options = IndexedReaderOptions {
            object_bytes: limits.max_object_bytes,
            stream_bytes: limits.max_stream_bytes,
            encoded_stream_bytes: limits.max_encoded_stream_bytes,
            endstream_tail_bytes: limits.max_endstream_tail_bytes,
            reference_depth: limits.max_length_depth,
            ..IndexedReaderOptions::default()
        };
        Self::from_erased_source(source, options)
    }

    pub(super) fn open_with_password(
        source: Arc<dyn RandomAccessSource>, limits: ResolverLimits, password: Option<&[u8]>,
    ) -> IndexedReaderResult<Self> {
        let options = IndexedReaderOptions {
            object_bytes: limits.max_object_bytes,
            stream_bytes: limits.max_stream_bytes,
            encoded_stream_bytes: limits.max_encoded_stream_bytes,
            endstream_tail_bytes: limits.max_endstream_tail_bytes,
            reference_depth: limits.max_length_depth,
            password: password.map(<[u8]>::to_vec),
            ..IndexedReaderOptions::default()
        };
        Self::from_erased_source(source, options)
    }
}

impl IndexedReader {
    fn initialize_encryption(&mut self, password: Option<&[u8]>) -> IndexedReaderResult<()> {
        let Ok(encrypt) = self.index.trailer.get(b"Encrypt") else {
            return Ok(());
        };
        let (dictionary, encrypt_object_id) = if let Ok(dictionary) = encrypt.as_dict() {
            (dictionary.clone(), None)
        } else if let Ok(id) = encrypt.as_reference() {
            let mut state = ResolutionState::default();
            let object = self.resolve_normal_plain(id, &mut state)?;
            let dictionary = object
                .as_dict()
                .map_err(|_| IndexedReaderError::InvalidEncryptDictionary)?
                .clone();
            (dictionary, Some(id))
        } else {
            return Err(IndexedReaderError::InvalidEncryptDictionary);
        };
        let file_id = self
            .index
            .trailer
            .get(b"ID")
            .ok()
            .and_then(|id| id.as_array().ok())
            .and_then(|ids| ids.first())
            .and_then(|id| id.as_str().ok());
        let algorithm = PasswordAlgorithm::try_from(&dictionary).map_err(IndexedReaderError::Encryption)?;

        let selected = if let Some(selected) = authenticate_password(&algorithm, file_id, b"")? {
            selected
        } else if let Some(password) = password {
            authenticate_password(&algorithm, file_id, password)?.ok_or(IndexedReaderError::InvalidPassword)?
        } else {
            return Err(IndexedReaderError::PasswordRequired);
        };
        let state = EncryptionState::decode_from_dictionary(&dictionary, file_id, &selected)
            .map_err(IndexedReaderError::Encryption)?;
        let index = Arc::get_mut(&mut self.index).expect("index is not shared during reader construction");
        index.encryption_state = Some(state);
        index.encrypt_object_id = encrypt_object_id;
        Ok(())
    }
}
