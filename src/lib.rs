#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod content;
pub mod encryption;
pub mod filters;
pub mod source;
pub mod xobject;
pub mod xref;

#[macro_use]
mod object;
mod document;
mod incremental_document;
#[cfg_attr(not(test), allow(dead_code))]
mod indexed_reader;

mod bookmarks;
mod cmap_section;
mod common_data_structures;
mod creator;
mod datetime;
mod destinations;
mod encodings;
mod error;
mod outlines;
mod processor;
mod toc;
mod writer;

mod load_options;
mod object_stream;
mod parser;
mod parser_aux;
mod reader;
mod save_options;

#[cfg(feature = "font_embedding")]
mod font;

pub use document::Document;
pub use object::{Dictionary, Object, ObjectId, Stream, StringFormat};

pub use bookmarks::Bookmark;
pub use common_data_structures::{decode_text_string, text_string};
pub use destinations::Destination;
pub use encodings::{Encoding, encode_utf8, encode_utf16_be};
pub use encryption::{EncryptionState, EncryptionVersion, Permissions};
pub use error::{DecompressError, Error, ParseError, Result};
pub use incremental_document::IncrementalDocument;
pub use load_options::{FilterFunc, LoadOptions};
pub use object_stream::{MAX_SELECTED_OBJECT_STREAM_MEMBERS, ObjectStream, ObjectStreamBuilder, ObjectStreamConfig};
pub use outlines::Outline;
pub use reader::{PdfMetadata, Reader};
pub use save_options::{SaveOptions, SaveOptionsBuilder};
#[cfg(any(unix, windows))]
pub use source::FileSource;
pub use source::{BytesSource, RandomAccessSource, SourceError, SourceResult};
pub use toc::{Toc, TocType};

pub use parser_aux::substr;
pub use parser_aux::substring;

#[cfg(feature = "font_embedding")]
pub use font::FontData;
