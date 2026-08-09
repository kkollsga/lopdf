//! Object resolution: scalar, shared, batched, and compressed paths.

use super::*;

#[derive(Default)]
pub(super) struct ResolutionState {
    pub(super) active: HashSet<crate::ObjectId>,
    pub(super) depth: usize,
    /// Count of nested indirect resolutions started under this state.
    ///
    /// A resolution that never bumps this consulted neither `active` nor
    /// `depth` beyond its own frame, so its result is a pure function of the
    /// source bytes and the reader limits. [`ContainerReuse`] uses exactly that
    /// to decide whether one decoded object-stream container may stand in for
    /// re-reading it under a different caller's active set.
    pub(super) nested_resolutions: usize,
}

/// Walk-local reuse of decoded object-stream containers.
///
/// A page-tree walk resolves one object per `/Kids` entry. On a document that
/// keeps its page dictionaries in object streams — the common modern shape —
/// every one of those nodes re-read, re-framed and re-inflated the same
/// container: 383 pages over 3 containers meant 383 inflations. This retains
/// the decoded image between nodes so each container is inflated once.
///
/// Reuse is a pure prefetch, never a semantic shortcut. An entry is only
/// retained when resolving its container started no nested indirect resolution
/// (the container's `/Length` was direct), which is what makes the retained
/// image provably identical to the one a fresh read would produce under any
/// other caller's active set. Every other check, in every other order, is the
/// one the unbatched walk applies, so a miss and a hit are indistinguishable
/// apart from time.
pub(super) struct ContainerReuse {
    entries: HashMap<crate::ObjectId, ReusedContainer>,
    order: VecDeque<crate::ObjectId>,
    retained_bytes: usize,
    peak_bytes: usize,
    budget_bytes: usize,
}

struct ReusedContainer {
    dictionary: Dictionary,
    decoded: Vec<u8>,
}

impl ContainerReuse {
    pub(super) fn with_budget(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            peak_bytes: 0,
            budget_bytes,
        }
    }

    fn get(&self, container: crate::ObjectId) -> Option<&ReusedContainer> {
        self.entries.get(&container)
    }

    fn retain(&mut self, container: crate::ObjectId, dictionary: Dictionary, decoded: Vec<u8>) {
        let bytes = decoded.len();
        if bytes > self.budget_bytes || self.entries.contains_key(&container) {
            return;
        }
        // Oldest-first release: a page tree walks its containers in runs, so the
        // container a bounded budget gives up is the one furthest behind the
        // frontier. Releasing one only costs a re-read, never a different answer.
        while self.retained_bytes.saturating_add(bytes) > self.budget_bytes
            && let Some(evicted) = self.order.pop_front()
        {
            if let Some(entry) = self.entries.remove(&evicted) {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.decoded.len());
            }
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.peak_bytes = self.peak_bytes.max(self.retained_bytes);
        self.order.push_back(container);
        self.entries.insert(container, ReusedContainer { dictionary, decoded });
    }

    /// Most decoded container bytes held at once, for residency accounting.
    pub(super) fn peak_retained_bytes(&self) -> usize {
        self.peak_bytes
    }
}

#[derive(Clone, Copy)]
struct CompressedBatchRequest {
    position: usize,
    pub(super) id: crate::ObjectId,
    index: u32,
}

pub(super) struct ParsedObject {
    pub(super) object: Object,
    pub(super) consumed: usize,
    pub(super) stream_prefix: Option<u64>,
}

pub(super) enum FramedStreamMetadata {
    Scalar(Object),
    MissingLength {
        dictionary: Dictionary,
        encoded_start: u64,
    },
    Span {
        dictionary: Dictionary,
        encoded_start: u64,
        encoded_len: u64,
    },
}

#[derive(Clone, Copy)]
pub(super) enum StreamSpanPolicy {
    Materialized(u64),
    Encoded(Option<u64>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum FilterProtection {
    Plain,
    Crypt,
}

impl IndexedReader {
    /// Resolve one full object id into an owned value.
    pub fn resolve_object(&self, id: crate::ObjectId) -> IndexedReaderResult<Object> {
        let mut state = ResolutionState::default();
        self.resolve_inner(id, &mut state)
    }

    /// Resolve one scalar object under one process-external, call-local memory
    /// allowance. This seam never returns a stream. The ordinary indexed-reader
    /// APIs retain their existing behavior and do not pay for this accounting.
    pub fn resolve_scalar_with_permit(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        if permit.stats().current_bytes != 0 {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: permit.stats().current_bytes,
                limit: permit.limit_bytes(),
                phase: "permit-not-empty",
            });
        }
        match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal_scalar_limited(id, permit),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed_scalar_limited(id, container, index, permit)
            }
            Some(ObjectLocation64::Free { .. }) | None => Err(IndexedReaderError::MissingNormalObject { id }),
        }
    }

    /// Resolve one scalar or stream into a common bounded owned value.
    ///
    /// The returned owner keeps every retained allocation charged to `permit`
    /// and exposes a shared [`Object`] view without cloning stream payloads.
    pub fn resolve_object_with_permit(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<crate::BoundedObject> {
        match self.resolve_scalar_with_permit(id, permit) {
            Ok(scalar) => Ok(crate::BoundedObject::Scalar(scalar)),
            Err(IndexedReaderError::NotScalarObject { .. }) => self
                .resolve_stream_with_permit(id, permit)
                .map(crate::BoundedObject::Stream),
            Err(error) => Err(error),
        }
    }

    /// Decode and index one object-stream container under a caller-owned
    /// permit, without consulting or populating the reader's private caches.
    ///
    /// The returned cache-agnostic owner may be retained in a downstream cell
    /// keyed by `container`; member parsing reuses this one decoded image.
    pub fn prepare_object_stream_with_permit(
        &self, container: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedObjectStream> {
        if permit.stats().current_bytes != 0 {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id: container,
                requested: permit.stats().current_bytes,
                limit: permit.limit_bytes(),
                phase: "permit-not-empty",
            });
        }
        if container.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch {
                id: container,
                indexed: 0,
            });
        }
        let (prepared, charges) = self.prepare_compressed_object_stream_limited(container, container.0, 0, permit)?;
        let PreparedObjectStream::Selected(selected) = prepared else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream {
                id: container,
                container,
            });
        };
        Ok(BoundedObjectStream {
            container_id: container,
            selected,
            permit: permit.clone(),
            charges: charges.into_boxed_slice(),
        })
    }

    /// Resolve one normal stream under one call-local simultaneous allocation
    /// allowance. The returned dictionary and decrypted content remain charged
    /// until their bounded owner (or its zero-copy content owner) is dropped.
    /// Compressed objects and missing/malformed stream lengths are typed-refused.
    pub fn resolve_stream_with_permit(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<crate::BoundedStream> {
        if permit.stats().current_bytes != 0 {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: permit.stats().current_bytes,
                limit: permit.limit_bytes(),
                phase: "permit-not-empty",
            });
        }
        if !matches!(self.index.locations.get(&id.0), Some(ObjectLocation64::Normal { .. })) {
            return Err(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "stream objects outside a normal xref entry",
            });
        }
        let (body_offset, source_len, parsed, mut dictionary_charge) = self.parse_normal_at_limited(id, permit)?;
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Err(IndexedReaderError::NotStreamObject { id });
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexedReaderError::NotStreamObject { id });
        };
        let mut length_state = ResolutionState::default();
        let encoded_len = self
            .resolve_stream_length_limited(&dictionary, permit, &mut length_state)?
            .and_then(|length| u64::try_from(length).ok())
            .ok_or(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "streams without a bounded nonnegative /Length",
            })?;
        let encoded_limit = self.limits.max_stream_bytes.min(permit.limit_bytes());
        if encoded_len > encoded_limit {
            return Err(IndexedReaderError::StreamLimitExceeded {
                id,
                length: encoded_len,
                limit: encoded_limit,
            });
        }
        let encoded_start = body_offset
            .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?;
        let encoded_end = encoded_start
            .checked_add(encoded_len)
            .filter(|end| *end <= source_len)
            .ok_or(IndexedReaderError::StreamLimitExceeded {
                id,
                length: encoded_len,
                limit: encoded_limit,
            })?;
        let dictionary_bytes = u64::try_from(dictionary_retained_bytes(&dictionary)).unwrap_or(u64::MAX);
        if dictionary_bytes > dictionary_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: dictionary_bytes,
                limit: dictionary_charge.bytes(),
                phase: "measured-stream-dictionary",
            });
        }
        dictionary_charge.shrink_to(dictionary_bytes);

        let tail_bytes = self
            .limits
            .max_endstream_tail_bytes
            .min(source_len.saturating_sub(encoded_end));
        let tail_charge = permit.reserve(id, tail_bytes, "stream-end-marker")?;
        match validate_endstream(
            self.source.as_ref(),
            source_len,
            encoded_end,
            self.limits.max_endstream_tail_bytes,
        )? {
            EndstreamStatus::Found => {}
            EndstreamStatus::Missing | EndstreamStatus::LimitExceeded => {
                return Err(IndexedReaderError::MissingEndstream { id });
            }
        }
        drop(tail_charge);

        let mut content_charge = permit.reserve(id, encoded_len, "stream-encoded-content")?;
        let decrypts = self.index.encrypt_object_id != Some(id) && self.index.encryption_state.is_some();
        let decrypt_overlap = if decrypts {
            encoded_len.saturating_mul(2).saturating_add(64)
        } else {
            0
        };
        let decrypt_charge = permit.reserve(id, decrypt_overlap, "stream-decryption-overlap")?;
        let encoded_usize = usize::try_from(encoded_len).map_err(|_| IndexedReaderError::ScalarResourceLimit {
            id,
            requested: encoded_len,
            limit: permit.limit_bytes(),
            phase: "stream-encoded-content",
        })?;
        let mut content = Vec::new();
        content
            .try_reserve_exact(encoded_usize)
            .map_err(|_| SourceError::AllocationFailed { requested: encoded_len })?;
        content.resize(encoded_usize, 0);
        let mut completed = 0_usize;
        while completed < content.len() {
            let request = (content.len() - completed).min(ENCODED_STREAM_CHUNK_LIMIT);
            let offset = encoded_start
                .checked_add(u64::try_from(completed).unwrap_or(u64::MAX))
                .ok_or(IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: encoded_start,
                })?;
            self.source
                .read_exact_at(offset, &mut content[completed..completed + request])?;
            completed += request;
        }
        let mut object = Object::Stream(Stream::new(dictionary, content));
        if let Some(encryption_state) = &self.index.encryption_state
            && self.index.encrypt_object_id != Some(id)
        {
            encryption::decrypt_object(encryption_state, id, &mut object)
                .map_err(|source| IndexedReaderError::ObjectDecryption { id, source })?;
        }
        drop(decrypt_charge);
        let Object::Stream(stream) = object else {
            unreachable!("bounded stream construction preserves the object variant")
        };
        let content_bytes = u64::try_from(stream.content.capacity()).unwrap_or(u64::MAX);
        if content_bytes > content_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: content_bytes,
                limit: content_charge.bytes(),
                phase: "measured-stream-content",
            });
        }
        content_charge.shrink_to(content_bytes);
        let retained = dictionary_bytes.saturating_add(content_bytes);
        let peak = permit.stats().peak_bytes;
        Ok(crate::BoundedStream::new(
            stream,
            retained,
            peak,
            dictionary_charge,
            content_charge,
        ))
    }

    pub(super) fn resolve_normal_scalar_limited(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        let (body_offset, source_len, parsed, object_charge) = self.parse_normal_at_limited(id, permit)?;
        let ParsedObject {
            mut object,
            consumed,
            stream_prefix,
        } = parsed;
        if let Some(stream_prefix) = stream_prefix {
            let Object::Dictionary(dictionary) = object else {
                return Err(IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                });
            };
            let encoded_start = body_offset
                .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
                .and_then(|offset| offset.checked_add(stream_prefix))
                .ok_or(IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                })?;
            let mut length_state = ResolutionState::default();
            let Some(length) = self.resolve_stream_length_limited(&dictionary, permit, &mut length_state)? else {
                return Err(IndexedReaderError::NotScalarObject { id });
            };
            if length < 0 {
                return Err(IndexedReaderError::NegativeStreamLength { id, length });
            }
            let length = u64::try_from(length).map_err(|_| IndexedReaderError::NegativeStreamLength { id, length })?;
            if length > self.limits.max_stream_bytes {
                return Err(IndexedReaderError::StreamLimitExceeded {
                    id,
                    length,
                    limit: self.limits.max_stream_bytes,
                });
            }
            let encoded_end = checked_stream_end(id, encoded_start, length)?;
            if encoded_end > source_len {
                object = Object::Dictionary(dictionary);
                return self.finish_bounded_scalar(id, object, object_charge, permit);
            }
            let tail_bytes = self
                .limits
                .max_endstream_tail_bytes
                .min(source_len.saturating_sub(encoded_end));
            let tail_charge = permit.reserve(id, tail_bytes, "scalar-stream-end-marker")?;
            let status = validate_endstream(
                self.source.as_ref(),
                source_len,
                encoded_end,
                self.limits.max_endstream_tail_bytes,
            )?;
            drop(tail_charge);
            match status {
                EndstreamStatus::Found => return Err(IndexedReaderError::NotScalarObject { id }),
                EndstreamStatus::Missing => object = Object::Dictionary(dictionary),
                EndstreamStatus::LimitExceeded => return Err(IndexedReaderError::MissingEndstream { id }),
            }
        }
        self.finish_bounded_scalar(id, object, object_charge, permit)
    }

    fn finish_bounded_scalar(
        &self, id: crate::ObjectId, mut object: Object, mut object_charge: ScalarCharge,
        permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        if self.index.encrypt_object_id != Some(id)
            && let Some(encryption_state) = &self.index.encryption_state
        {
            encryption::decrypt_object(encryption_state, id, &mut object)
                .map_err(|source| IndexedReaderError::ObjectDecryption { id, source })?;
        }
        let retained = u64::try_from(scalar_object_retained_bytes(&object)).unwrap_or(u64::MAX);
        if retained > object_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: retained,
                limit: object_charge.bytes(),
                phase: "measured-scalar",
            });
        }
        object_charge.shrink_to(retained);
        let peak = permit.stats().peak_bytes;
        Ok(BoundedScalar::new(object, retained, peak, object_charge))
    }

    pub(super) fn parse_normal_at_limited(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(u64, u64, ParsedObject, ScalarCharge)> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexedReaderError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header_len = (source_len.saturating_sub(physical)).min(INDIRECT_HEADER_LIMIT);
        let header = ChargedBytes::read(
            self.source.as_ref(),
            physical,
            header_len,
            permit,
            id,
            "indirect-header",
        )?;
        let (actual, header_bytes) =
            parse_indirect_header(&header.bytes).ok_or(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit {
                    offset,
                    limit: INDIRECT_HEADER_LIMIT,
                },
            })?;
        if actual.0 != id.0 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderMismatch { expected: id, actual },
            });
        }
        if actual.1 != id.1 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: id,
                    indexed: indexed_generation,
                    actual,
                },
            });
        }
        let body_offset = physical
            .checked_add(
                u64::try_from(header_bytes).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?,
            )
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        drop(header);
        let (parsed, charge) = self.parse_normal_body_limited(id, body_offset, source_len, permit)?;
        Ok((body_offset, source_len, parsed, charge))
    }

    /// Frame an encrypted ObjStm dictionary through its `stream` EOL without
    /// allowing a speculative source request to cross into the ciphertext.
    /// Reservation sizes and error phases mirror the general bounded parser;
    /// only encrypted object-stream preparation uses this byte-exact path.
    fn parse_encrypted_object_stream_prefix_limited(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(u64, u64, ParsedObject, ScalarCharge, bool)> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexedReaderError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header_len = (source_len.saturating_sub(physical)).min(INDIRECT_HEADER_LIMIT);
        let header_charge = permit.reserve(id, header_len, "indirect-header")?;
        let header_capacity =
            usize::try_from(header_len).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let mut header = Vec::new();
        header
            .try_reserve_exact(header_capacity)
            .map_err(|_| SourceError::AllocationFailed { requested: header_len })?;
        let mut parsed_header = None;
        while header.len() < header_capacity {
            let relative =
                u64::try_from(header.len()).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?;
            let read_offset = physical
                .checked_add(relative)
                .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
            let mut byte = [0_u8; 1];
            self.source.read_exact_at(read_offset, &mut byte)?;
            header.push(byte[0]);
            if let Some((actual, header_bytes)) = parse_indirect_header(&header)
                && (header_bytes < header.len() || header.len() == header_capacity)
            {
                parsed_header = Some((actual, header_bytes));
                break;
            }
        }
        let (actual, header_bytes) = parsed_header.ok_or(IndexedReaderError::MissingNormalObjectAtXref {
            id,
            reason: MissingNormalObjectReason::HeaderProbeLimit {
                offset,
                limit: INDIRECT_HEADER_LIMIT,
            },
        })?;
        if actual.0 != id.0 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderMismatch { expected: id, actual },
            });
        }
        if actual.1 != id.1 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: id,
                    indexed: indexed_generation,
                    actual,
                },
            });
        }
        let body_offset = physical
            .checked_add(
                u64::try_from(header_bytes).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?,
            )
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        drop(header);
        drop(header_charge);
        let (parsed, charge, after_stream_cr) =
            self.parse_encrypted_object_stream_prefix_body_limited(id, body_offset, source_len, permit)?;
        Ok((body_offset, source_len, parsed, charge, after_stream_cr))
    }

    fn parse_encrypted_object_stream_prefix_body_limited(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(ParsedObject, ScalarCharge, bool)> {
        let remaining = source_len.checked_sub(body_offset).ok_or(SourceError::OutOfBounds {
            offset: body_offset,
            length: 0,
            source_len,
        })?;
        let maximum = remaining.min(self.limits.max_object_bytes).min(permit.limit_bytes());
        let mut target = maximum.min(INITIAL_OBJECT_WINDOW);
        let mut frame_charge = permit.reserve(id, target, "scalar-frame")?;
        let mut window = Vec::new();
        let target_usize = usize::try_from(target).map_err(|_| object_limit_arithmetic(id, maximum))?;
        window
            .try_reserve_exact(target_usize)
            .map_err(|_| SourceError::AllocationFailed { requested: target })?;
        let mut object_framer = DirectObjectFramer::new();
        loop {
            while u64::try_from(window.len()).unwrap_or(u64::MAX) < target {
                let relative = u64::try_from(window.len()).map_err(|_| object_limit_arithmetic(id, maximum))?;
                let read_offset =
                    body_offset
                        .checked_add(relative)
                        .ok_or(IndexedReaderError::InvalidIndirectObject {
                            id,
                            offset: body_offset,
                        })?;
                let mut byte = [0_u8; 1];
                self.source.read_exact_at(read_offset, &mut byte)?;
                window.push(byte[0]);
                let status = object_framer.advance(&window);
                let after_stream_cr =
                    status == FrameStatus::NeedMore && matches!(object_framer.lex, FrameLex::StreamEolCr);
                if status == FrameStatus::Invalid {
                    return Err(IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: body_offset,
                    });
                }
                if status == FrameStatus::Ready || after_stream_cr {
                    let ast_bound = scalar_ast_preflight(&window)
                        .ok_or(IndexedReaderError::InvalidIndirectObject {
                            id,
                            offset: body_offset,
                        })?
                        .reserved_bytes(self.index.encryption_state.is_some());
                    let ast_charge = permit.reserve(id, ast_bound, "scalar-ast-envelope")?;
                    let parsed = parse_object_body(&window, id, body_offset)?;
                    drop(window);
                    drop(frame_charge);
                    return Ok((parsed, ast_charge, after_stream_cr));
                }
            }
            if target >= maximum {
                return Err(object_frame_at_maximum(id, maximum, remaining));
            }
            let next_target = target
                .saturating_mul(2)
                .min(target.saturating_add(OBJECT_GROWTH_CHUNK))
                .min(maximum);
            let next_charge = permit.reserve(id, next_target, "scalar-frame-growth")?;
            let next_usize = usize::try_from(next_target).map_err(|_| object_limit_arithmetic(id, maximum))?;
            let mut next_window = Vec::new();
            next_window
                .try_reserve_exact(next_usize)
                .map_err(|_| SourceError::AllocationFailed { requested: next_target })?;
            next_window.extend_from_slice(&window);
            drop(window);
            drop(frame_charge);
            window = next_window;
            frame_charge = next_charge;
            target = next_target;
        }
    }

    fn parse_normal_body_limited(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(ParsedObject, ScalarCharge)> {
        let remaining = source_len.checked_sub(body_offset).ok_or(SourceError::OutOfBounds {
            offset: body_offset,
            length: 0,
            source_len,
        })?;
        let maximum = remaining.min(self.limits.max_object_bytes).min(permit.limit_bytes());
        let initial_length = maximum.min(INITIAL_OBJECT_WINDOW);
        let mut window = ChargedBytes::read(
            self.source.as_ref(),
            body_offset,
            initial_length,
            permit,
            id,
            "scalar-frame",
        )?;
        let mut object_framer = DirectObjectFramer::new();
        loop {
            match object_framer.advance(&window.bytes) {
                FrameStatus::Invalid => {
                    return Err(IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: body_offset,
                    });
                }
                FrameStatus::Ready => break,
                FrameStatus::NeedMore => {}
            }
            let current = u64::try_from(window.bytes.len()).map_err(|_| object_limit_arithmetic(id, maximum))?;
            if current >= maximum {
                return Err(object_frame_at_maximum(id, maximum, remaining));
            }
            let target = current
                .saturating_mul(2)
                .min(current.saturating_add(OBJECT_GROWTH_CHUNK))
                .min(maximum);
            window = ChargedBytes::grow_exact(
                window,
                self.source.as_ref(),
                body_offset,
                target,
                permit,
                id,
                "scalar-frame-growth",
            )?;
        }

        let ast_bound = scalar_ast_preflight(&window.bytes)
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?
            .reserved_bytes(self.index.encryption_state.is_some());
        let ast_charge = permit.reserve(id, ast_bound, "scalar-ast-envelope")?;
        let parsed = parse_object_body(&window.bytes, id, body_offset)?;
        drop(window);
        Ok((parsed, ast_charge))
    }

    fn resolve_compressed_scalar_limited(
        &self, id: crate::ObjectId, container: u32, index: u32, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<BoundedScalar> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        let container_id = (container, 0);
        let prepared = if let Some(cache) = self.object_stream_cache.as_ref() {
            cache.resolve_bounded(container_id, id, index, permit, || {
                self.prepare_compressed_object_stream_limited(id, container, index, permit)
            })?
        } else {
            let (prepared, charges) = self.prepare_compressed_object_stream_limited(id, container, index, permit)?;
            BoundedPreparedObjectStream::CallLocal {
                prepared: Arc::new(prepared),
                _charges: charges,
            }
        };

        let selected = match prepared.prepared() {
            PreparedObjectStream::Selected(selected) => selected,
            PreparedObjectStream::Raw(_) => {
                return Err(IndexedReaderError::UnsupportedBoundedScalar {
                    id,
                    reason: "an object-stream cache entry without a selected-member index",
                });
            }
            PreparedObjectStream::NotStream => {
                return Err(IndexedReaderError::ObjectStreamContainerNotStream {
                    id,
                    container: container_id,
                });
            }
        };
        let member = selected
            .member_slice(id, index)
            .map_err(|source| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source,
            })?;
        let ast_bound = scalar_ast_preflight(member)
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source: crate::Error::InvalidObjectStream(
                    "selected object stream member is truncated or invalid".to_string(),
                ),
            })?
            .reserved_bytes(false);
        let mut ast_charge = permit.reserve(id, ast_bound, "object-stream-member-ast")?;
        let object = crate::parser::direct_object(member).ok_or_else(|| IndexedReaderError::ObjectStreamMember {
            id,
            container: container_id,
            index,
            source: crate::Error::InvalidObjectStream(
                "selected object stream member is truncated or invalid".to_string(),
            ),
        })?;
        drop(prepared);
        let retained = u64::try_from(scalar_object_retained_bytes(&object)).unwrap_or(u64::MAX);
        if retained > ast_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: retained,
                limit: ast_charge.bytes(),
                phase: "measured-object-stream-member",
            });
        }
        ast_charge.shrink_to(retained);
        let peak = permit.stats().peak_bytes;
        Ok(BoundedScalar::new(object, retained, peak, ast_charge))
    }

    fn prepare_compressed_object_stream_limited(
        &self, id: crate::ObjectId, container: u32, index: u32, permit: &ScalarResolutionPermit,
    ) -> IndexedReaderResult<(PreparedObjectStream, Vec<ScalarCharge>)> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        let container_id = (container, 0);
        let decrypts = self.index.encrypt_object_id != Some(container_id) && self.index.encryption_state.is_some();
        let (body_offset, source_len, parsed, mut dictionary_charge, after_stream_cr) = if decrypts {
            self.parse_encrypted_object_stream_prefix_limited(container_id, permit)?
        } else {
            let (body_offset, source_len, parsed, dictionary_charge) =
                self.parse_normal_at_limited(container_id, permit)?;
            (body_offset, source_len, parsed, dictionary_charge, false)
        };
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream {
                id,
                container: container_id,
            });
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream {
                id,
                container: container_id,
            });
        };
        let encoding =
            limited_object_stream_encoding(&dictionary).ok_or(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "object-stream filter chains or decode parameters outside the bounded decode envelope",
            })?;
        let mut length_state = ResolutionState::default();
        let encoded_len = self
            .resolve_stream_length_limited(&dictionary, permit, &mut length_state)?
            .and_then(|length| u64::try_from(length).ok())
            .ok_or(IndexedReaderError::UnsupportedBoundedScalar {
                id,
                reason: "object streams without a bounded nonnegative /Length",
            })?;
        let encoded_limit = self.limits.max_stream_bytes.min(permit.limit_bytes());
        if encoded_len > encoded_limit {
            return Err(IndexedReaderError::StreamLimitExceeded {
                id: container_id,
                length: encoded_len,
                limit: encoded_limit,
            });
        }
        let mut encoded_start = body_offset
            .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id: container_id,
                offset: body_offset,
            })?;
        let mut encoded_end = if after_stream_cr {
            None
        } else {
            Some(
                encoded_start
                    .checked_add(encoded_len)
                    .filter(|end| *end <= source_len)
                    .ok_or(IndexedReaderError::StreamLimitExceeded {
                        id: container_id,
                        length: encoded_len,
                        limit: permit.limit_bytes(),
                    })?,
            )
        };
        let decrypt_overlap = if decrypts {
            encoded_len.saturating_mul(2).saturating_add(64)
        } else {
            0
        };
        let mut prefetched_payload_byte = None;
        if after_stream_cr && encoded_len != 0 {
            // A CR stream EOL is ambiguous until the next byte distinguishes
            // lone CR from CRLF. Admit the complete payload/decryption overlap
            // before that one-byte read, then release it so tail validation
            // retains the established admission order and peak semantics.
            let admission = permit.admit_without_peak(
                id,
                &[
                    (encoded_len, "object-stream-encoded"),
                    (decrypt_overlap, "object-stream-decryption-overlap"),
                ],
            )?;
            let mut lookahead = [0_u8; 1];
            self.source.read_exact_at(encoded_start, &mut lookahead)?;
            if lookahead[0] == b'\n' {
                encoded_start = encoded_start
                    .checked_add(1)
                    .ok_or(IndexedReaderError::InvalidIndirectObject {
                        id: container_id,
                        offset: body_offset,
                    })?;
            } else {
                prefetched_payload_byte = Some(lookahead[0]);
            }
            drop(admission);
        }
        if encoded_end.is_none() {
            encoded_end = Some(
                encoded_start
                    .checked_add(encoded_len)
                    .filter(|end| *end <= source_len)
                    .ok_or(IndexedReaderError::StreamLimitExceeded {
                        id: container_id,
                        length: encoded_len,
                        limit: permit.limit_bytes(),
                    })?,
            );
        }
        let encoded_end = encoded_end.unwrap();
        let dictionary_bytes = u64::try_from(dictionary_retained_bytes(&dictionary)).unwrap_or(u64::MAX);
        if dictionary_bytes > dictionary_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: dictionary_bytes,
                limit: dictionary_charge.bytes(),
                phase: "measured-object-stream-dictionary",
            });
        }
        dictionary_charge.shrink_to(dictionary_bytes);
        let first = dictionary
            .get(b"First")
            .and_then(Object::as_i64)
            .ok()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source: crate::Error::InvalidObjectStream("invalid object stream /First".into()),
            })?;
        let declared_members = dictionary
            .get(b"N")
            .and_then(Object::as_i64)
            .ok()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value <= crate::MAX_SELECTED_OBJECT_STREAM_MEMBERS)
            .ok_or_else(|| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source: crate::Error::InvalidObjectStream("invalid object stream /N".into()),
            })?;
        let tail_bytes = self
            .limits
            .max_endstream_tail_bytes
            .min(source_len.saturating_sub(encoded_end));
        let tail_charge = permit.reserve(id, tail_bytes, "object-stream-end-marker")?;
        match validate_endstream(
            self.source.as_ref(),
            source_len,
            encoded_end,
            self.limits.max_endstream_tail_bytes,
        )? {
            EndstreamStatus::Found => {}
            EndstreamStatus::Missing | EndstreamStatus::LimitExceeded => {
                return Err(IndexedReaderError::MissingEndstream { id: container_id });
            }
        }
        drop(tail_charge);
        let mut content_charge = permit.reserve(id, encoded_len, "object-stream-encoded")?;
        // Re-admit the complete ciphertext/plaintext/cipher-work overlap before
        // allocating the payload buffer or reading any remaining payload bytes.
        let mut decrypt_charge = permit.reserve(id, decrypt_overlap, "object-stream-decryption-overlap")?;
        let encoded_usize = usize::try_from(encoded_len).map_err(|_| IndexedReaderError::ScalarResourceLimit {
            id,
            requested: encoded_len,
            limit: permit.limit_bytes(),
            phase: "object-stream-encoded",
        })?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(encoded_usize)
            .map_err(|_| SourceError::AllocationFailed { requested: encoded_len })?;
        encoded.resize(encoded_usize, 0);
        let mut completed = 0_usize;
        if let Some(first) = prefetched_payload_byte {
            encoded[0] = first;
            completed = 1;
        }
        while completed < encoded.len() {
            let request = (encoded.len() - completed).min(ENCODED_STREAM_CHUNK_LIMIT);
            let offset = encoded_start
                .checked_add(u64::try_from(completed).unwrap_or(u64::MAX))
                .ok_or(IndexedReaderError::InvalidIndirectObject {
                    id: container_id,
                    offset: encoded_start,
                })?;
            self.source
                .read_exact_at(offset, &mut encoded[completed..completed + request])?;
            completed += request;
        }
        let mut object = Object::Stream(Stream {
            dict: dictionary,
            content: encoded,
            allows_compression: true,
            start_position: None,
        });
        if let Some(encryption_state) = &self.index.encryption_state
            && self.index.encrypt_object_id != Some(container_id)
        {
            encryption::decrypt_object(encryption_state, container_id, &mut object).map_err(|source| {
                IndexedReaderError::ObjectDecryption {
                    id: container_id,
                    source,
                }
            })?;
        }
        let Object::Stream(stream) = object else {
            unreachable!("object-stream decryption preserves the stream variant")
        };
        let plaintext_bytes = u64::try_from(stream.content.capacity()).unwrap_or(u64::MAX);
        if plaintext_bytes > content_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: plaintext_bytes,
                limit: content_charge.bytes(),
                phase: "measured-object-stream-plaintext",
            });
        }
        content_charge.shrink_to(plaintext_bytes);
        let decrypted_dictionary_bytes = u64::try_from(dictionary_retained_bytes(&stream.dict)).unwrap_or(u64::MAX);
        let dictionary_extra = decrypted_dictionary_bytes.saturating_sub(dictionary_charge.bytes());
        if dictionary_extra > decrypt_charge.bytes() {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: permit
                    .stats()
                    .current_bytes
                    .saturating_add(dictionary_extra.saturating_sub(decrypt_charge.bytes())),
                limit: permit.limit_bytes(),
                phase: "measured-object-stream-decrypted-dictionary",
            });
        }
        decrypt_charge.shrink_to(dictionary_extra);

        let (decoded, decoded_charge) = match encoding {
            LimitedObjectStreamEncoding::Plain => {
                let Stream { content, .. } = stream;
                drop(dictionary_charge);
                drop(decrypt_charge);
                (content, content_charge)
            }
            LimitedObjectStreamEncoding::Decoded => {
                let current = permit.stats().current_bytes;
                let decode_envelope = permit.limit_bytes().saturating_sub(current);
                const DECODER_FIXED_BYTES: u64 = 128 * 1024;
                if decode_envelope <= DECODER_FIXED_BYTES {
                    return Err(IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: current.saturating_add(DECODER_FIXED_BYTES).saturating_add(1),
                        limit: permit.limit_bytes(),
                        phase: "object-stream-decode-envelope",
                    });
                }
                let mut decoded_charge = permit.reserve(id, decode_envelope, "object-stream-decode-envelope")?;
                let decoded_limit = usize::try_from((decode_envelope - DECODER_FIXED_BYTES) / 4).unwrap_or(usize::MAX);
                let decoded = match stream.decompressed_content_with_limit(decoded_limit) {
                    Ok(decoded) => decoded,
                    Err(crate::Error::Decompress(crate::DecompressError::MemoryLimitExceeded { .. })) => {
                        // The decoder deliberately stops at its charged output
                        // cap, so it can prove only that a larger allowance is
                        // required. Doubling is deterministic and monotonic,
                        // bounds retry count logarithmically, and lets the
                        // caller enforce its own oversize ceiling before the
                        // next allocation or source read.
                        let Some(requested) = permit.limit_bytes().checked_mul(2) else {
                            return Err(object_limit_arithmetic(id, permit.limit_bytes()));
                        };
                        return Err(IndexedReaderError::ScalarResourceLimit {
                            id,
                            requested,
                            limit: permit.limit_bytes(),
                            phase: "object-stream-decompressed-growth",
                        });
                    }
                    Err(source) => {
                        return Err(IndexedReaderError::ObjectStreamMember {
                            id,
                            container: container_id,
                            index,
                            source,
                        });
                    }
                };
                let decoded_capacity = u64::try_from(decoded.capacity()).unwrap_or(u64::MAX);
                if decoded_capacity > decode_envelope {
                    return Err(IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: current.saturating_add(decoded_capacity),
                        limit: permit.limit_bytes(),
                        phase: "object-stream-decoded-capacity",
                    });
                }
                drop(stream);
                drop(content_charge);
                drop(dictionary_charge);
                drop(decrypt_charge);
                decoded_charge.shrink_to(decoded_capacity);
                (decoded, decoded_charge)
            }
        };

        let pair_count = crate::object_stream::SelectedObjectStream::index_pair_count_with_first(first, &decoded)
            .map_err(|source| IndexedReaderError::ObjectStreamMember {
                id,
                container: container_id,
                index,
                source,
            })?;
        let pair_bytes = pair_count.saturating_mul(std::mem::size_of::<(Option<u32>, Option<u32>)>());
        let pair_charge = permit.reserve(
            id,
            u64::try_from(pair_bytes).unwrap_or(u64::MAX),
            "object-stream-header-index",
        )?;
        let selected = crate::object_stream::SelectedObjectStream::from_decoded_parts(
            first,
            i64::try_from(declared_members).unwrap_or(i64::MAX),
            decoded,
        )
        .map_err(|source| IndexedReaderError::ObjectStreamMember {
            id,
            container: container_id,
            index,
            source,
        })?;
        let retained = u64::try_from(selected.retained_bytes()).unwrap_or(u64::MAX);
        let charged = decoded_charge.bytes().saturating_add(pair_charge.bytes());
        if retained > charged {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: retained,
                limit: charged,
                phase: "measured-object-stream-cache-entry",
            });
        }
        Ok((
            PreparedObjectStream::Selected(selected),
            vec![decoded_charge, pair_charge],
        ))
    }
}

impl IndexedReader {
    /// Resolve one full object id into a shareable owned value.
    ///
    /// Readers opened by the legacy constructors do not retain this value:
    /// the `Arc` only makes the returned ownership shareable. When a cached
    /// constructor configures the bounded resolver caches, concurrent calls
    /// for the same id are single-flighted and successful or deterministic
    /// failing results remain reusable until eviction. Source I/O failures are
    /// shared by calls already waiting on that cell, then discarded so a later
    /// operation can retry.
    pub fn resolve_object_shared(&self, id: crate::ObjectId) -> SharedIndexedReaderResult<Arc<Object>> {
        let Some(cache) = &self.object_cache else {
            let mut state = ResolutionState::default();
            return self.resolve_inner(id, &mut state).map(Arc::new).map_err(Arc::new);
        };
        cache.resolve(id, || self.resolve_object_shared_uncached(id), object_retained_bytes)
    }

    fn resolve_object_shared_uncached(&self, id: crate::ObjectId) -> SharedIndexedReaderResult<Arc<Object>> {
        let mut state = ResolutionState::default();
        match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Compressed { container, index }) if self.object_stream_cache.is_some() => {
                self.resolve_compressed_shared(id, container, index, &mut state)
            }
            _ => self.resolve_inner(id, &mut state).map(Arc::new).map_err(Arc::new),
        }
    }

    /// Resolve each unique requested id into a shareable owned value.
    ///
    /// Results retain the first-occurrence order of `ids`; duplicate ids are
    /// resolved once and omitted from later positions. Independent ordinary
    /// objects may complete out of order internally. Compressed requests are
    /// grouped by object-stream container so each successful container is
    /// resolved, decoded and header-indexed once for this call.
    pub fn resolve_many_shared(
        &self, ids: &[crate::ObjectId],
    ) -> Vec<(crate::ObjectId, SharedIndexedReaderResult<Arc<Object>>)> {
        let mut seen = HashSet::with_capacity(ids.len());
        let unique: Vec<_> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        let mut normal = Vec::new();
        let mut compressed: BTreeMap<u32, Vec<CompressedBatchRequest>> = BTreeMap::new();

        for (position, id) in unique.iter().copied().enumerate() {
            match self.index.locations.get(&id.0) {
                Some(ObjectLocation64::Compressed { container, index }) => {
                    compressed.entry(*container).or_default().push(CompressedBatchRequest {
                        position,
                        id,
                        index: *index,
                    });
                }
                _ => normal.push((position, id)),
            }
        }

        #[cfg(feature = "rayon")]
        let normal_results: Vec<_> = normal
            .into_par_iter()
            .map(|(position, id)| (position, self.resolve_object_shared(id)))
            .collect();
        #[cfg(not(feature = "rayon"))]
        let normal_results: Vec<_> = normal
            .into_iter()
            .map(|(position, id)| (position, self.resolve_object_shared(id)))
            .collect();

        let mut results: Vec<Option<SharedIndexedReaderResult<Arc<Object>>>> =
            std::iter::repeat_with(|| None).take(unique.len()).collect();
        for (position, result) in normal_results {
            results[position] = Some(result);
        }
        for (container, requests) in compressed {
            for (position, result) in self.resolve_compressed_group(container, &requests) {
                results[position] = Some(result);
            }
        }

        unique
            .into_iter()
            .zip(results)
            .map(|(id, result)| (id, result.expect("every unique batch id is classified")))
            .collect()
    }
}

impl IndexedReader {
    /// Return an owned trailer value, resolving an indirect entry through the
    /// same object resolver used by [`Self::resolve_object`]. Direct values are
    /// cloned from the immutable index. Missing keys return `Ok(None)`.
    pub fn trailer_entry_owned(&self, key: &[u8]) -> IndexedReaderResult<Option<Object>> {
        let Some(value) = self.index.trailer.get(key).ok() else {
            return Ok(None);
        };
        match value {
            Object::Reference(id) => self.resolve_object(*id).map(Some),
            direct => Ok(Some(direct.clone())),
        }
    }

    /// Return an owned trailer value without dereferencing an indirect entry.
    ///
    /// This is a structural index lookup only: missing keys return `None`,
    /// direct values are cloned as-is, and references remain
    /// [`Object::Reference`] values. No target object or source bytes are read.
    pub fn trailer_entry_raw_owned(&self, key: &[u8]) -> Option<Object> {
        self.index.trailer.get(key).ok().cloned()
    }

    /// Enumerate every live indexed indirect object id in deterministic order.
    ///
    /// Free entries are excluded. Normal entries preserve their xref
    /// generation; compressed entries use generation zero as required by PDF.
    pub fn object_ids(&self) -> Vec<crate::ObjectId> {
        self.index
            .locations
            .iter()
            .filter_map(|(number, location)| match location {
                ObjectLocation64::Free { .. } => None,
                ObjectLocation64::Normal { generation, .. } => Some((*number, *generation)),
                ObjectLocation64::Compressed { .. } => Some((*number, 0)),
            })
            .collect()
    }

    /// Locate one live object without exposing physical source offsets.
    pub fn object_location(&self, id: crate::ObjectId) -> Option<IndexedObjectLocation> {
        match self.index.locations.get(&id.0)? {
            ObjectLocation64::Normal { generation, .. } if *generation == id.1 => Some(IndexedObjectLocation::Normal),
            ObjectLocation64::Compressed { container, index } if id.1 == 0 => Some(IndexedObjectLocation::Compressed {
                container: (*container, 0),
                index: *index,
            }),
            ObjectLocation64::Free { .. } | ObjectLocation64::Normal { .. } | ObjectLocation64::Compressed { .. } => {
                None
            }
        }
    }
}

impl IndexedReader {
    /// PDF header version, for example `"1.7"`.
    pub fn version(&self) -> &str {
        &self.index.version
    }

    /// Stable source length captured while the index was opened.
    pub fn source_len(&self) -> u64 {
        self.index.source_len
    }

    /// Whether the trailer declares an encryption dictionary.
    pub fn is_encrypted(&self) -> bool {
        self.index.trailer.has(b"Encrypt")
    }

    /// Whether an encrypted document has an authenticated decryption state.
    pub fn is_authenticated(&self) -> bool {
        self.index.encryption_state.is_some()
    }

    /// Derive and count actual leaf pages without trusting `/Count`.
    pub fn page_count(&self) -> IndexedReaderResult<usize> {
        self.page_map().map(|pages| pages.len())
    }
}

impl IndexedReader {
    fn resolve_inner(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        state.nested_resolutions += 1;
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal(id, state),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed(id, container, index, state)
            }
            // A slot the index says is free reads as null, the same value the
            // eager `Document::dereference` hands back for it (ISO 32000-1,
            // 7.3.10). An id the index does not mention at all keeps the typed
            // refusal: that is a file disagreeing with itself, not a deletion.
            Some(ObjectLocation64::Free { .. }) => Ok(Object::Null),
            None => Err(IndexedReaderError::MissingNormalObject { id }),
        };
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed(
        &self, id: crate::ObjectId, container: u32, index: u32, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Object> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        let container = (container, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(container) {
            return Err(IndexedReaderError::ResolutionCycle { id: container });
        }
        state.depth += 1;
        // Object streams must themselves be ordinary, generation-zero indirect
        // objects. Do not recursively accept a compressed container here.
        let resolved = self.resolve_normal(container, state);
        state.depth -= 1;
        state.active.remove(&container);

        let object = resolved?;
        let Object::Stream(stream) = object else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream { id, container });
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        ObjectStream::parse_selected_member_with_limit(&stream, id, index, Some(limit)).map_err(|source| {
            IndexedReaderError::ObjectStreamMember {
                id,
                container,
                index,
                source,
            }
        })
    }

    /// [`Self::resolve_object`] over a caller-owned decoded-container prefetch.
    ///
    /// Every branch, check and error below is [`Self::resolve_inner`]'s; the
    /// only difference is that a compressed object may read its container out
    /// of `reuse` instead of off the source again.
    pub(super) fn resolve_object_reusing(
        &self, id: crate::ObjectId, reuse: &mut ContainerReuse,
    ) -> IndexedReaderResult<Object> {
        let mut state = ResolutionState::default();
        self.resolve_inner_reusing(id, &mut state, reuse)
    }

    fn resolve_inner_reusing(
        &self, id: crate::ObjectId, state: &mut ResolutionState, reuse: &mut ContainerReuse,
    ) -> IndexedReaderResult<Object> {
        state.nested_resolutions += 1;
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = match self.index.locations.get(&id.0).cloned() {
            Some(ObjectLocation64::Normal { .. }) => self.resolve_normal(id, state),
            Some(ObjectLocation64::Compressed { container, index }) => {
                self.resolve_compressed_reusing(id, container, index, state, reuse)
            }
            // Same freed-slot rule as `resolve_inner`: null, not an error.
            Some(ObjectLocation64::Free { .. }) => Ok(Object::Null),
            None => Err(IndexedReaderError::MissingNormalObject { id }),
        };
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed_reusing(
        &self, id: crate::ObjectId, container: u32, index: u32, state: &mut ResolutionState, reuse: &mut ContainerReuse,
    ) -> IndexedReaderResult<Object> {
        if id.1 != 0 {
            return Err(IndexedReaderError::GenerationMismatch { id, indexed: 0 });
        }
        let container = (container, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        // The cycle guard runs before the prefetch is consulted: a container
        // that is its own member must still refuse, even once a sibling member
        // has put its decoded image in reach.
        if !state.active.insert(container) {
            return Err(IndexedReaderError::ResolutionCycle { id: container });
        }
        state.depth += 1;
        let reused = reuse.get(container).is_some();
        // Object streams must themselves be ordinary, generation-zero indirect
        // objects. Do not recursively accept a compressed container here.
        let fetched = (!reused).then(|| {
            let nested_before = state.nested_resolutions;
            let resolved = self.resolve_normal(container, state);
            (resolved, state.nested_resolutions == nested_before)
        });
        state.depth -= 1;
        state.active.remove(&container);

        let member_error = |source| IndexedReaderError::ObjectStreamMember {
            id,
            container,
            index,
            source,
        };
        let Some((resolved, self_contained)) = fetched else {
            let entry = reuse.get(container).expect("a hit stays retained for this call");
            return ObjectStream::parse_selected_member_from_decoded(&entry.dictionary, &entry.decoded, id, index)
                .map_err(member_error);
        };

        let object = resolved?;
        let Object::Stream(stream) = object else {
            return Err(IndexedReaderError::ObjectStreamContainerNotStream { id, container });
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        let decoded = ObjectStream::decode_selected_member_source(&stream, Some(limit)).map_err(member_error)?;
        let member =
            ObjectStream::parse_selected_member_from_decoded(&stream.dict, &decoded, id, index).map_err(member_error);
        if self_contained {
            let decoded = decoded.into_owned();
            reuse.retain(container, stream.dict, decoded);
        }
        member
    }

    fn resolve_compressed_shared(
        &self, id: crate::ObjectId, container_number: u32, index: u32, state: &mut ResolutionState,
    ) -> SharedIndexedReaderResult<Arc<Object>> {
        if state.depth >= self.limits.max_length_depth {
            return Err(Arc::new(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            }));
        }
        if !state.active.insert(id) {
            return Err(Arc::new(IndexedReaderError::ResolutionCycle { id }));
        }
        state.depth += 1;
        let result = self.resolve_compressed_shared_inner(id, container_number, index, state);
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_compressed_shared_inner(
        &self, id: crate::ObjectId, container_number: u32, index: u32, state: &mut ResolutionState,
    ) -> SharedIndexedReaderResult<Arc<Object>> {
        if id.1 != 0 {
            return Err(Arc::new(IndexedReaderError::GenerationMismatch { id, indexed: 0 }));
        }
        let container = (container_number, 0);
        if state.depth >= self.limits.max_length_depth {
            return Err(Arc::new(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            }));
        }
        if !state.active.insert(container) {
            return Err(Arc::new(IndexedReaderError::ResolutionCycle { id: container }));
        }
        state.depth += 1;
        let cache = self
            .object_stream_cache
            .as_ref()
            .expect("shared compressed resolution requires its configured cache");
        let prepared = cache.resolve(
            container,
            || {
                let prepared = match self.resolve_normal(container, state) {
                    Ok(Object::Stream(stream)) => {
                        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
                        match ObjectStream::selected_members_with_limit(&stream, Some(limit)) {
                            Ok(selected) => PreparedObjectStream::Selected(selected),
                            Err(_) => PreparedObjectStream::Raw(stream),
                        }
                    }
                    Ok(_) => PreparedObjectStream::NotStream,
                    Err(error) => return Err(Arc::new(error)),
                };
                Ok(Arc::new(prepared))
            },
            PreparedObjectStream::cache_weight,
        )?;
        state.depth -= 1;
        state.active.remove(&container);

        match prepared.as_ref() {
            PreparedObjectStream::Selected(selected) => {
                selected.parse_member(id, index).map(Arc::new).map_err(|source| {
                    Arc::new(IndexedReaderError::ObjectStreamMember {
                        id,
                        container,
                        index,
                        source,
                    })
                })
            }
            PreparedObjectStream::Raw(stream) => {
                let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
                ObjectStream::parse_selected_member_with_limit(stream, id, index, Some(limit))
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id,
                            container,
                            index,
                            source,
                        })
                    })
            }
            PreparedObjectStream::NotStream => Err(Arc::new(IndexedReaderError::ObjectStreamContainerNotStream {
                id,
                container,
            })),
        }
    }

    fn resolve_compressed_group(
        &self, container_number: u32, requests: &[CompressedBatchRequest],
    ) -> Vec<(usize, SharedIndexedReaderResult<Arc<Object>>)> {
        // Very small depth limits make the active root id observable. Preserve
        // exact scalar behavior rather than sharing container setup there.
        if self.limits.max_length_depth < 2 {
            return requests
                .iter()
                .map(|request| (request.position, self.resolve_object_shared(request.id)))
                .collect();
        }

        let mut results = Vec::with_capacity(requests.len());
        let valid: Vec<_> = requests
            .iter()
            .filter(|request| {
                if request.id.1 == 0 {
                    true
                } else {
                    results.push((
                        request.position,
                        Err(Arc::new(IndexedReaderError::GenerationMismatch {
                            id: request.id,
                            indexed: 0,
                        })),
                    ));
                    false
                }
            })
            .collect();
        if valid.is_empty() {
            return results;
        }

        let container = (container_number, 0);
        let mut state = ResolutionState {
            active: HashSet::from([container]),
            // Reserve the same target-object and container depths used by the
            // scalar compressed path.
            depth: 2,
            nested_resolutions: 0,
        };
        let object = match self.resolve_normal(container, &mut state) {
            Ok(object) => object,
            Err(error) => {
                let error = Arc::new(error);
                results.extend(
                    valid
                        .into_iter()
                        .map(|request| (request.position, Err(Arc::clone(&error)))),
                );
                return results;
            }
        };
        let Object::Stream(stream) = object else {
            results.extend(valid.into_iter().map(|request| {
                (
                    request.position,
                    Err(Arc::new(IndexedReaderError::ObjectStreamContainerNotStream {
                        id: request.id,
                        container,
                    })),
                )
            }));
            return results;
        };
        let limit = usize::try_from(self.limits.max_stream_bytes).unwrap_or(usize::MAX);
        let selected = match ObjectStream::selected_members_with_limit(&stream, Some(limit)) {
            Ok(selected) => selected,
            Err(source) => {
                let error = Arc::new(IndexedReaderError::ObjectStreamBatchSetup { container, source });
                results.extend(
                    valid
                        .into_iter()
                        .map(|request| (request.position, Err(Arc::clone(&error)))),
                );
                return results;
            }
        };

        #[cfg(feature = "rayon")]
        let parsed: Vec<_> = valid
            .into_par_iter()
            .map(|request| {
                let result = selected
                    .parse_member(request.id, request.index)
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id: request.id,
                            container,
                            index: request.index,
                            source,
                        })
                    });
                (request.position, result)
            })
            .collect();
        #[cfg(not(feature = "rayon"))]
        let parsed: Vec<_> = valid
            .into_iter()
            .map(|request| {
                let result = selected
                    .parse_member(request.id, request.index)
                    .map(Arc::new)
                    .map_err(|source| {
                        Arc::new(IndexedReaderError::ObjectStreamMember {
                            id: request.id,
                            container,
                            index: request.index,
                            source,
                        })
                    });
                (request.position, result)
            })
            .collect();
        results.extend(parsed);
        results
    }

    fn resolve_normal(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<Object> {
        let mut object = self.resolve_normal_plain(id, state)?;
        if self.index.encrypt_object_id != Some(id)
            && let Some(encryption_state) = &self.index.encryption_state
        {
            encryption::decrypt_object(encryption_state, id, &mut object)
                .map_err(|source| IndexedReaderError::ObjectDecryption { id, source })?;
        }
        Ok(object)
    }

    pub(super) fn resolve_normal_plain(
        &self, id: crate::ObjectId, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Object> {
        let (body_offset, source_len, parsed) = self.resolve_normal_framed(id)?;
        self.finish_object(id, body_offset, source_len, parsed, state)
    }

    pub(super) fn resolve_normal_framed(&self, id: crate::ObjectId) -> IndexedReaderResult<(u64, u64, ParsedObject)> {
        let location = self
            .index
            .locations
            .get(&id.0)
            .ok_or(IndexedReaderError::MissingNormalObject { id })?;
        let (offset, indexed_generation) = match location {
            ObjectLocation64::Normal { offset, generation } => (*offset, *generation),
            _ => return Err(IndexedReaderError::MissingNormalObject { id }),
        };
        let physical = self
            .index
            .source_origin
            .checked_add(offset)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let source_len = self.source.len()?;
        let header = read_window(self.source.as_ref(), source_len, physical, INDIRECT_HEADER_LIMIT)?;
        let (actual, header_bytes) =
            parse_indirect_header(&header).ok_or(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit {
                    offset,
                    limit: INDIRECT_HEADER_LIMIT,
                },
            })?;
        if actual.0 != id.0 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderMismatch { expected: id, actual },
            });
        }
        if actual.1 != id.1 {
            return Err(IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::GenerationMismatch {
                    requested: id,
                    indexed: indexed_generation,
                    actual,
                },
            });
        }
        let header_bytes =
            u64::try_from(header_bytes).map_err(|_| IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let body_offset = physical
            .checked_add(header_bytes)
            .ok_or(IndexedReaderError::InvalidIndirectObject { id, offset })?;
        let parsed = self.resolve_body_frame(id, body_offset, source_len)?;
        Ok((body_offset, source_len, parsed))
    }
}

impl IndexedReader {
    fn resolve_body_frame(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64,
    ) -> IndexedReaderResult<ParsedObject> {
        let remaining = source_len.checked_sub(body_offset).ok_or(SourceError::OutOfBounds {
            offset: body_offset,
            length: 0,
            source_len,
        })?;
        let maximum = remaining.min(self.limits.max_object_bytes);
        let initial_length = maximum.min(INITIAL_OBJECT_WINDOW);
        let mut window = self.source.read_range(body_offset, initial_length, initial_length)?;
        let mut object_framer = DirectObjectFramer::new();
        loop {
            let frame_status = object_framer.advance(&window);
            if frame_status == FrameStatus::Invalid {
                return Err(IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                });
            }
            if frame_status == FrameStatus::Ready {
                return parse_object_body(&window, id, body_offset);
            }

            let current =
                u64::try_from(window.len()).map_err(|_| object_limit_arithmetic(id, self.limits.max_object_bytes))?;
            if current >= maximum {
                return Err(object_frame_need_more(id, body_offset, maximum, remaining));
            }
            // Extend the retained prefix instead of rereading it. A fixed
            // upper growth step caps speculative stream-payload reads while
            // still doubling small object probes.
            let target = current
                .saturating_mul(2)
                .min(current.saturating_add(OBJECT_GROWTH_CHUNK))
                .min(maximum);
            let extension_length = target - current;
            let extension_offset =
                body_offset
                    .checked_add(current)
                    .ok_or(IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: body_offset,
                    })?;
            let extension = self
                .source
                .read_range(extension_offset, extension_length, extension_length)?;
            window.extend_from_slice(&extension);
        }
    }

    fn finish_object(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, parsed: ParsedObject,
        state: &mut ResolutionState,
    ) -> IndexedReaderResult<Object> {
        match self
            .finish_stream_metadata(
                id,
                body_offset,
                source_len,
                parsed,
                state,
                StreamSpanPolicy::Materialized(self.limits.max_stream_bytes),
            )
            .map_err(StreamMetadataError::into_reader_error)?
        {
            FramedStreamMetadata::Scalar(object) => Ok(object),
            FramedStreamMetadata::MissingLength {
                dictionary,
                encoded_start,
            } => {
                let relative_stream_start = encoded_start.checked_sub(self.index.source_origin).ok_or(
                    IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: encoded_start,
                    },
                )?;
                let stream_start =
                    usize::try_from(relative_stream_start).map_err(|_| IndexedReaderError::InvalidIndirectObject {
                        id,
                        offset: encoded_start,
                    })?;
                Ok(Object::Stream(Stream::with_position(dictionary, stream_start)))
            }
            FramedStreamMetadata::Span {
                dictionary,
                encoded_start,
                encoded_len,
            } => {
                let content = self
                    .source
                    .read_range(encoded_start, encoded_len, self.limits.max_stream_bytes)?;
                Ok(Object::Stream(Stream::new(dictionary, content)))
            }
        }
    }

    pub(super) fn finish_stream_metadata(
        &self, id: crate::ObjectId, body_offset: u64, source_len: u64, parsed: ParsedObject,
        state: &mut ResolutionState, span_policy: StreamSpanPolicy,
    ) -> Result<FramedStreamMetadata, StreamMetadataError> {
        let ParsedObject {
            object,
            consumed,
            stream_prefix,
        } = parsed;
        let Some(stream_prefix) = stream_prefix else {
            return Ok(FramedStreamMetadata::Scalar(object));
        };
        let Object::Dictionary(dictionary) = object else {
            return Err(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            }
            .into());
        };

        let stream_start = body_offset
            .checked_add(
                u64::try_from(consumed).map_err(|_| IndexedReaderError::InvalidIndirectObject {
                    id,
                    offset: body_offset,
                })?,
            )
            .and_then(|offset| offset.checked_add(stream_prefix))
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: body_offset,
            })?;
        let Some(length) = self.resolve_stream_length(&dictionary, state)? else {
            // This matches the eager loader's degradation for a missing,
            // malformed, dangling, cyclic, or over-depth /Length: retain the
            // stream dictionary and expose empty owned content.
            return Ok(FramedStreamMetadata::MissingLength {
                dictionary,
                encoded_start: stream_start,
            });
        };
        if length < 0 {
            return Err(IndexedReaderError::NegativeStreamLength { id, length }.into());
        }
        let length = u64::try_from(length).map_err(|_| IndexedReaderError::NegativeStreamLength { id, length })?;
        if let StreamSpanPolicy::Materialized(limit) = span_policy
            && length > limit
        {
            return Err(IndexedReaderError::StreamLimitExceeded { id, length, limit }.into());
        }

        let stream_end = stream_start
            .checked_add(length)
            .ok_or(IndexedReaderError::InvalidIndirectObject {
                id,
                offset: stream_start,
            })?;
        if stream_end > source_len {
            return Ok(FramedStreamMetadata::Scalar(Object::Dictionary(dictionary)));
        }
        if let StreamSpanPolicy::Encoded(Some(limit)) = span_policy
            && length > limit
        {
            return Err(StreamMetadataError::EncodedLimit { id, length, limit });
        }
        match validate_endstream(
            self.source.as_ref(),
            source_len,
            stream_end,
            self.limits.max_endstream_tail_bytes,
        )? {
            EndstreamStatus::Found => {}
            EndstreamStatus::Missing => {
                return Ok(FramedStreamMetadata::Scalar(Object::Dictionary(dictionary)));
            }
            EndstreamStatus::LimitExceeded => return Err(IndexedReaderError::MissingEndstream { id }.into()),
        }
        Ok(FramedStreamMetadata::Span {
            dictionary,
            encoded_start: stream_start,
            encoded_len: length,
        })
    }

    fn resolve_stream_length(
        &self, dictionary: &Dictionary, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Option<i64>> {
        let Ok(length) = dictionary.get(b"Length") else {
            return Ok(None);
        };
        if let Ok(value) = length.as_i64() {
            return Ok(Some(value));
        }
        let Ok(reference) = length.as_reference() else {
            return Ok(None);
        };
        // Eager loading treats every failure to dereference /Length as a
        // missing length and retains an empty stream. Keep that degradation,
        // while the bounded state guarantees cycles and deep chains terminate.
        Ok(self.resolve_length_reference(reference, state).ok())
    }

    pub(super) fn resolve_stream_length_limited(
        &self, dictionary: &Dictionary, permit: &ScalarResolutionPermit, state: &mut ResolutionState,
    ) -> IndexedReaderResult<Option<i64>> {
        let Ok(length) = dictionary.get(b"Length") else {
            return Ok(None);
        };
        if let Ok(value) = length.as_i64() {
            return Ok(Some(value));
        }
        let Ok(reference) = length.as_reference() else {
            return Ok(None);
        };
        // Match eager metadata degradation, but resolve through the same
        // call-local ledger so an indirect /Length cannot escape the bound.
        match self.resolve_length_reference_limited(reference, permit, state) {
            Ok(value) => Ok(Some(value)),
            Err(error @ IndexedReaderError::ScalarResourceLimit { .. })
            | Err(error @ IndexedReaderError::ScalarResolutionCancelled { .. })
            | Err(error @ IndexedReaderError::ScalarResolutionClosed { .. }) => Err(error),
            Err(_) => Ok(None),
        }
    }

    fn resolve_length_reference_limited(
        &self, id: crate::ObjectId, permit: &ScalarResolutionPermit, state: &mut ResolutionState,
    ) -> IndexedReaderResult<i64> {
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = self.resolve_normal_scalar_limited(id, permit).and_then(|object| {
            let next = match object.as_object() {
                Object::Integer(value) => return Ok(*value),
                Object::Reference(next) => *next,
                _ => return Err(IndexedReaderError::InvalidIndirectObject { id, offset: 0 }),
            };
            drop(object);
            self.resolve_length_reference_limited(next, permit, state)
        });
        state.depth -= 1;
        state.active.remove(&id);
        result
    }

    fn resolve_length_reference(&self, id: crate::ObjectId, state: &mut ResolutionState) -> IndexedReaderResult<i64> {
        state.nested_resolutions += 1;
        if state.depth >= self.limits.max_length_depth {
            return Err(IndexedReaderError::ResolutionDepthExceeded {
                limit: self.limits.max_length_depth,
            });
        }
        if !state.active.insert(id) {
            return Err(IndexedReaderError::ResolutionCycle { id });
        }
        state.depth += 1;
        let result = self.resolve_normal(id, state).and_then(|object| match object {
            Object::Integer(value) => Ok(value),
            Object::Reference(next) => self.resolve_length_reference(next, state),
            _ => Err(IndexedReaderError::InvalidIndirectObject { id, offset: 0 }),
        });
        state.depth -= 1;
        state.active.remove(&id);
        result
    }
}
