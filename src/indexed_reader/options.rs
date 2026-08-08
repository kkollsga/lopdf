//! Public reader options and the internal resolver limits they lower to.

use super::*;

/// Resource limits and optional password used by [`IndexedReader`].
///
/// Defaults preserve the indexed reader's bounded compatibility profile.
#[derive(Clone)]
pub struct IndexedReaderOptions {
    /// Maximum bytes parsed while resolving one ordinary object.
    pub object_bytes: u64,
    /// Maximum declared or decoded bytes retained for one stream.
    pub stream_bytes: u64,
    /// Optional maximum declared encoded span exposed by stream descriptors.
    ///
    /// `None` admits any checked span bounded by the captured source length.
    /// This streaming workload policy does not weaken [`Self::stream_bytes`],
    /// which continues to cap retained, decrypted, or decoded stream data.
    pub encoded_stream_bytes: Option<u64>,
    /// Maximum bytes inspected after a declared stream payload.
    pub endstream_tail_bytes: u64,
    /// Maximum recursive object/reference resolution depth.
    pub reference_depth: usize,
    /// Maximum page-tree depth followed while deriving a page map.
    pub page_tree_depth: usize,
    /// Maximum leaf pages retained in a derived page map.
    pub max_pages: usize,
    /// Optional raw PDF password. Debug output always redacts its value.
    pub password: Option<Vec<u8>>,
}

impl std::fmt::Debug for IndexedReaderOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexedReaderOptions")
            .field("object_bytes", &self.object_bytes)
            .field("stream_bytes", &self.stream_bytes)
            .field("encoded_stream_bytes", &self.encoded_stream_bytes)
            .field("endstream_tail_bytes", &self.endstream_tail_bytes)
            .field("reference_depth", &self.reference_depth)
            .field("page_tree_depth", &self.page_tree_depth)
            .field("max_pages", &self.max_pages)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl Default for IndexedReaderOptions {
    fn default() -> Self {
        Self {
            object_bytes: DEFAULT_OBJECT_LIMIT,
            stream_bytes: DEFAULT_STREAM_LIMIT,
            encoded_stream_bytes: None,
            endstream_tail_bytes: DEFAULT_ENDSTREAM_TAIL_LIMIT,
            reference_depth: DEFAULT_LENGTH_DEPTH_LIMIT,
            page_tree_depth: DEFAULT_PAGE_TREE_DEPTH_LIMIT,
            max_pages: DEFAULT_PAGE_COUNT_LIMIT,
            password: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ResolverLimits {
    pub(crate) max_object_bytes: u64,
    pub(crate) max_stream_bytes: u64,
    pub(crate) max_encoded_stream_bytes: Option<u64>,
    pub(crate) max_endstream_tail_bytes: u64,
    pub(crate) max_length_depth: usize,
}

impl Default for ResolverLimits {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_OBJECT_LIMIT,
            max_stream_bytes: DEFAULT_STREAM_LIMIT,
            max_encoded_stream_bytes: None,
            max_endstream_tail_bytes: DEFAULT_ENDSTREAM_TAIL_LIMIT,
            max_length_depth: DEFAULT_LENGTH_DEPTH_LIMIT,
        }
    }
}

impl From<&IndexedReaderOptions> for ResolverLimits {
    fn from(options: &IndexedReaderOptions) -> Self {
        Self {
            max_object_bytes: options.object_bytes,
            max_stream_bytes: options.stream_bytes,
            max_encoded_stream_bytes: options.encoded_stream_bytes,
            max_endstream_tail_bytes: options.endstream_tail_bytes,
            max_length_depth: options.reference_depth,
        }
    }
}
