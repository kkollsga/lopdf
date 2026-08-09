//! Compare the eager `Document` path against the read-only `IndexedReader` on
//! the same workload: open a PDF, enumerate its pages, then fetch and decompress
//! every page's content stream(s).
//!
//! One process does one (file, mode) measurement, so wall clock and peak RSS can
//! be attributed cleanly. Run it under `/usr/bin/time -l` (macOS) or
//! `/usr/bin/time -v` (GNU) to capture peak resident set size.
//!
//! ```text
//! cargo run --release --example indexed_vs_eager_bench -- eager   big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- indexed big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- eager-open   big.pdf
//! cargo run --release --example indexed_vs_eager_bench -- indexed-open big.pdf
//! ```
//!
//! Modes:
//!
//! - `eager`: `Document::load` + `get_pages` + `get_page_content` per page.
//! - `indexed`: `IndexedReader::open_cached` + `page_map`, then per page locate each content stream, read its encoded
//!   payload through the bounded 64 KiB chunk reader, and decode it.
//! - `eager-open`: the open half of `eager` only (`Document::load` + `get_pages`).
//! - `indexed-open`: the open half of `indexed` only (`open_cached` + `page_map`).
//!
//! Every mode prints one JSON line. `digest` is a SHA-256 over every page's
//! decompressed content bytes in page order, so the two full modes can be checked
//! for byte identity by comparing that one field.

// The measurement only means anything where it can actually be taken: it opens a
// real file through `FileSource`, which the crate exports on unix and windows
// only, and it times the *synchronous* `Document::load`, which the `async`
// feature replaces with a future. On a wasm target, or under `--all-features`,
// the harness is therefore a stub that says why rather than a build failure —
// `cargo test --all-features` and the wasm job build every example.
#[cfg(all(any(unix, windows), not(feature = "async")))]
mod bench;

#[cfg(all(any(unix, windows), not(feature = "async")))]
fn main() -> std::process::ExitCode {
    bench::run()
}

#[cfg(not(all(any(unix, windows), not(feature = "async"))))]
fn main() {
    eprintln!(
        "indexed_vs_eager_bench needs a file-backed source and the synchronous loader: \
         build it on unix or windows without the `async` feature."
    );
}
