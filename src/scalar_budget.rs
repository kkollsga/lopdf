use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{IndexedReaderError, IndexedReaderResult, Object, ObjectId, Stream};

/// A snapshot of one call-local scalar-resolution allowance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ScalarResolutionStats {
    pub limit_bytes: u64,
    pub current_bytes: u64,
    pub peak_bytes: u64,
    pub reservations: u64,
    pub cancelled: bool,
    pub closed: bool,
}

/// Process-external admission token used by the indexed reader's limited
/// scalar seam. Every call-local owned allocation is represented by a charge
/// on this single ledger, including allocations which overlap during a phase
/// transition.
#[derive(Clone, Debug)]
pub struct ScalarResolutionPermit {
    inner: Arc<PermitInner>,
}

#[derive(Debug)]
struct PermitInner {
    state: Mutex<PermitState>,
}

#[derive(Debug)]
struct PermitState {
    limit_bytes: u64,
    current_bytes: u64,
    peak_bytes: u64,
    reservations: u64,
    cancelled: bool,
    closed: bool,
}

pub(crate) const PERMIT_INNER_STRUCTURAL_BYTES: usize = std::mem::size_of::<PermitInner>();
pub(crate) const SCALAR_CHARGE_STRUCTURAL_BYTES: usize = std::mem::size_of::<ScalarCharge>();

pub(crate) struct ScalarCharge {
    inner: Arc<PermitInner>,
    bytes: u64,
}

/// One scalar object whose retained allocation remains charged to its permit.
pub struct BoundedScalar {
    object: Object,
    retained_bytes: u64,
    peak_bytes: u64,
    _charge: ScalarCharge,
}

/// One fully materialized stream whose dictionary and content allocations
/// remain charged to the permit which admitted the resolution.
pub struct BoundedStream {
    object: Object,
    retained_bytes: u64,
    peak_bytes: u64,
    _dictionary_charge: ScalarCharge,
    content_charge: ScalarCharge,
}

/// One bounded owned object, scalar or stream, whose retained allocations
/// remain charged to the permit which admitted its resolution.
pub enum BoundedObject {
    Scalar(BoundedScalar),
    Stream(BoundedStream),
}

/// A materialized stream payload moved out without copying. Its allocation
/// remains charged until this owner is dropped.
pub struct BoundedStreamContent {
    bytes: Vec<u8>,
    peak_bytes: u64,
    _charge: ScalarCharge,
}

impl std::fmt::Debug for BoundedStreamContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedStreamContent")
            .field("bytes", &self.bytes.len())
            .field("allocation_bytes", &self.bytes.capacity())
            .field("peak_bytes", &self.peak_bytes)
            .finish()
    }
}

impl std::fmt::Debug for BoundedScalar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedScalar")
            .field("object", &self.object)
            .field("retained_bytes", &self.retained_bytes)
            .field("peak_bytes", &self.peak_bytes)
            .finish()
    }
}

impl Deref for BoundedScalar {
    type Target = Object;

    fn deref(&self) -> &Self::Target {
        &self.object
    }
}

impl BoundedScalar {
    pub const fn as_object(&self) -> &Object {
        &self.object
    }

    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub const fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    pub(crate) fn new(object: Object, retained_bytes: u64, peak_bytes: u64, charge: ScalarCharge) -> Self {
        Self {
            object,
            retained_bytes,
            peak_bytes,
            _charge: charge,
        }
    }
}

impl std::fmt::Debug for BoundedStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedStream")
            .field("id", &self.as_stream().start_position)
            .field("content_bytes", &self.as_stream().content.len())
            .field("retained_bytes", &self.retained_bytes)
            .field("peak_bytes", &self.peak_bytes)
            .finish_non_exhaustive()
    }
}

impl BoundedStream {
    pub const fn as_stream(&self) -> &Stream {
        match &self.object {
            Object::Stream(stream) => stream,
            _ => panic!("bounded stream invariant violated"),
        }
    }

    /// Inspect this stream through the common PDF object representation
    /// without cloning its payload.
    pub const fn as_object(&self) -> &Object {
        &self.object
    }

    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub const fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    pub fn into_content(self) -> BoundedStreamContent {
        let Self {
            object,
            peak_bytes,
            _dictionary_charge,
            content_charge,
            ..
        } = self;
        let Object::Stream(stream) = object else {
            unreachable!("bounded stream always owns an Object::Stream")
        };
        let Stream { content, .. } = stream;
        drop(_dictionary_charge);
        BoundedStreamContent {
            bytes: content,
            peak_bytes,
            _charge: content_charge,
        }
    }

    pub(crate) fn new(
        stream: Stream, retained_bytes: u64, peak_bytes: u64, dictionary_charge: ScalarCharge,
        content_charge: ScalarCharge,
    ) -> Self {
        Self {
            object: Object::Stream(stream),
            retained_bytes,
            peak_bytes,
            _dictionary_charge: dictionary_charge,
            content_charge,
        }
    }
}

impl std::fmt::Debug for BoundedObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scalar(object) => formatter.debug_tuple("Scalar").field(object).finish(),
            Self::Stream(object) => formatter.debug_tuple("Stream").field(object).finish(),
        }
    }
}

impl Deref for BoundedObject {
    type Target = Object;

    fn deref(&self) -> &Self::Target {
        self.as_object()
    }
}

impl BoundedObject {
    /// Inspect the owned scalar or stream without cloning stream content.
    pub fn as_object(&self) -> &Object {
        match self {
            Self::Scalar(object) => object.as_object(),
            Self::Stream(object) => object.as_object(),
        }
    }

    pub const fn is_stream(&self) -> bool {
        matches!(self, Self::Stream(_))
    }

    pub const fn retained_bytes(&self) -> u64 {
        match self {
            Self::Scalar(object) => object.retained_bytes(),
            Self::Stream(object) => object.retained_bytes(),
        }
    }

    pub const fn peak_bytes(&self) -> u64 {
        match self {
            Self::Scalar(object) => object.peak_bytes(),
            Self::Stream(object) => object.peak_bytes(),
        }
    }
}

impl BoundedStreamContent {
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn allocation_bytes(&self) -> usize {
        self.bytes.capacity()
    }

    pub const fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }
}

impl ScalarResolutionPermit {
    pub fn new(limit_bytes: u64) -> Self {
        Self {
            inner: Arc::new(PermitInner {
                state: Mutex::new(PermitState {
                    limit_bytes,
                    current_bytes: 0,
                    peak_bytes: 0,
                    reservations: 0,
                    cancelled: false,
                    closed: false,
                }),
            }),
        }
    }

    pub fn cancel(&self) {
        lock_unpoisoned(&self.inner.state).cancelled = true;
    }

    pub fn stats(&self) -> ScalarResolutionStats {
        stats(&lock_unpoisoned(&self.inner.state))
    }

    pub fn limit_bytes(&self) -> u64 {
        lock_unpoisoned(&self.inner.state).limit_bytes
    }

    pub fn close(&self) -> Result<ScalarResolutionStats, String> {
        let mut state = lock_unpoisoned(&self.inner.state);
        if state.current_bytes != 0 {
            return Err(format!(
                "cannot close scalar-resolution permit with {} charged bytes",
                state.current_bytes
            ));
        }
        state.closed = true;
        Ok(stats(&state))
    }

    pub(crate) fn reserve(&self, id: ObjectId, bytes: u64, phase: &'static str) -> IndexedReaderResult<ScalarCharge> {
        let mut state = lock_unpoisoned(&self.inner.state);
        if state.cancelled {
            return Err(IndexedReaderError::ScalarResolutionCancelled { id, phase });
        }
        if state.closed {
            return Err(IndexedReaderError::ScalarResolutionClosed { id, phase });
        }
        let proposed = state
            .current_bytes
            .checked_add(bytes)
            .ok_or(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: u64::MAX,
                limit: state.limit_bytes,
                phase,
            })?;
        if proposed > state.limit_bytes {
            return Err(IndexedReaderError::ScalarResourceLimit {
                id,
                requested: proposed,
                limit: state.limit_bytes,
                phase,
            });
        }
        state.current_bytes = proposed;
        state.peak_bytes = state.peak_bytes.max(proposed);
        state.reservations = state.reservations.saturating_add(1);
        Ok(ScalarCharge {
            inner: Arc::clone(&self.inner),
            bytes,
        })
    }
}

impl ScalarCharge {
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn shrink_to(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.bytes);
        let released = self.bytes.saturating_sub(bytes);
        self.bytes = bytes;
        let mut state = lock_unpoisoned(&self.inner.state);
        state.current_bytes = state.current_bytes.saturating_sub(released);
    }
}

impl Drop for ScalarCharge {
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.inner.state);
        state.current_bytes = state.current_bytes.saturating_sub(self.bytes);
    }
}

fn stats(state: &PermitState) -> ScalarResolutionStats {
    ScalarResolutionStats {
        limit_bytes: state.limit_bytes,
        current_bytes: state.current_bytes,
        peak_bytes: state.peak_bytes,
        reservations: state.reservations,
        cancelled: state.cancelled,
        closed: state.closed,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_charges_peak_and_release_to_zero() {
        let permit = ScalarResolutionPermit::new(10);
        let first = permit.reserve((1, 0), 6, "raw").unwrap();
        let second = permit.reserve((1, 0), 4, "ast").unwrap();
        assert_eq!(permit.stats().peak_bytes, 10);
        assert!(permit.reserve((1, 0), 1, "overflow").is_err());
        drop(first);
        assert_eq!(permit.stats().current_bytes, 4);
        drop(second);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
    }

    #[test]
    fn cancel_and_close_reject_new_charges() {
        let cancelled = ScalarResolutionPermit::new(10);
        cancelled.cancel();
        assert!(cancelled.reserve((1, 0), 1, "cancelled").is_err());
        assert_eq!(cancelled.close().unwrap().current_bytes, 0);

        let closed = ScalarResolutionPermit::new(10);
        closed.close().unwrap();
        assert!(closed.reserve((1, 0), 1, "closed").is_err());
    }
}
