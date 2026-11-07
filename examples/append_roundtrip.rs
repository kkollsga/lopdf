//! Round trips real PDFs through the append writer, and measures it. Doesn't touch its inputs.
//!
//!   append_roundtrip corpus <out-dir> <file>...     Two updates per file, every object compared, a JSON line each.
//!   append_roundtrip measure <mode> <out-dir> <file>   One update, timed. Modes: open, copy, atomic.
//!   append_roundtrip revisions <file>...            A file's revisions, a JSON line each.
//!
//! Wrap `measure` in `/usr/bin/time -l` for the whole process's peak footprint.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use lopdf::{
    AppendDestination, AppendError, AppendOptions, AppendResult, AppendWriter, FileSource, IndexedReader, Object,
    RandomAccessSource, append_atomically, dictionary,
};
use serde_json::json;
use sha2::{Digest, Sha256};

const KEYWORDS: &str = "rectopdf-roundtrip";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("corpus") if args.len() >= 3 => {
            let out = PathBuf::from(&args[1]);
            for path in &args[2..] {
                let path = PathBuf::from(path);
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let line =
                    std::panic::catch_unwind(|| round_trip(&path, &out)).unwrap_or_else(|_| json!({"status": "panic"}));
                let mut line = line;
                line["file"] = json!(name);
                println!("{line}");
                std::io::stdout().flush().unwrap();
            }
        }
        Some("measure") if args.len() == 4 => measure(&args[1], Path::new(&args[2]), Path::new(&args[3])),
        Some("revisions") if args.len() >= 2 => {
            for path in &args[1..] {
                let chain =
                    open(Path::new(path)).and_then(|reader| reader.revisions().map_err(|error| error.to_string()));
                let line = match chain {
                    Ok(chain) => json!({
                        "file": path,
                        "regular": chain.is_regular(),
                        "trailing_bytes": chain.trailing_bytes,
                        "revisions": chain.revisions.iter().map(|revision| json!({
                            "start": revision.range.as_ref().map(|range| range.start),
                            "end": revision.range.as_ref().map(|range| range.end),
                            "eof_marker_end": revision.eof_marker_end,
                            "startxref": revision.startxref,
                            "kind": format!("{:?}", revision.xref_kind),
                            "linearized": revision.linearized,
                            "entries": revision.entry_count,
                        })).collect::<Vec<_>>(),
                    }),
                    Err(error) => json!({"file": path, "error": error}),
                };
                println!("{line}");
            }
        }
        _ => {
            eprintln!(
                "usage: append_roundtrip corpus <out-dir> <file>... | measure <open|copy|atomic> <out-dir> <file>"
            );
            std::process::exit(2);
        }
    }
}

fn open(path: &Path) -> Result<IndexedReader, String> {
    let source = FileSource::open(path).map_err(|error| error.to_string())?;
    IndexedReader::open(source).map_err(|error| error.to_string())
}

/// A new object, and `/Info`'s `/Keywords` set, the two smallest real edits. Returns the new object's number.
fn small_edit<W: Write>(reader: &IndexedReader, writer: &mut AppendWriter<'_, W>) -> AppendResult<u32> {
    let added = writer.add_object(&Object::Dictionary(dictionary! {
        "RectoRoundTrip" => true,
        "Note" => Object::string_literal("added by append_roundtrip"),
    }))?;
    let keywords = Object::string_literal(KEYWORDS);
    match reader.trailer_entry_raw_owned(b"Info") {
        Some(Object::Reference(id)) if reader.object_location(id).is_some() => {
            let mut info = match reader.resolve_object(id) {
                Ok(Object::Dictionary(info)) => info,
                _ => lopdf::Dictionary::new(),
            };
            info.set("Keywords", keywords);
            writer.write_object(id, &Object::Dictionary(info))?;
        }
        _ => {
            let info = writer.add_object(&Object::Dictionary(dictionary! { "Keywords" => keywords }))?;
            writer.set_info(Object::Reference(info.id))?;
        }
    }
    Ok(added.id.0)
}

fn digest_prefix(path: &Path, length: u64) -> Vec<u8> {
    use std::io::Read;
    let mut hash = Sha256::new();
    let mut file = File::open(path).unwrap().take(length);
    let mut buffer = vec![0_u8; 64 * 1_024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            return hash.finalize().to_vec();
        }
        hash.update(&buffer[..read]);
    }
}

struct HashingSink(Sha256);

impl Write for HashingSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn round_trip(path: &Path, out: &Path) -> serde_json::Value {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let original_len = std::fs::metadata(path).unwrap().len();
    let started = Instant::now();
    let reader = match open(path) {
        Ok(reader) => reader,
        Err(error) => return json!({"status": "open_failed", "error": error, "bytes": original_len}),
    };
    let recovered = reader.index_stats().map(|stats| stats.recovered()).unwrap_or(false);
    let pages_before = reader.page_count().ok();
    let chain_before = reader.revisions().ok();
    if let Err(error) = reader.check_appendable() {
        let kind = format!("{error:?}");
        return json!({
            "status": "refused", "reason": error.to_string(),
            "kind": kind.split([' ', '{']).next().unwrap_or(""), "bytes": original_len, "recovered": recovered,
        });
    }
    let hybrid = chain_before.as_ref().is_some_and(|chain| {
        chain
            .revisions
            .iter()
            .any(|revision| revision.xref_kind == lopdf::IndexedXrefKind::Hybrid)
    });
    let kind = chain_before
        .as_ref()
        .and_then(|chain| chain.revisions.last())
        .map(|revision| format!("{:?}", revision.xref_kind));
    let linearized = chain_before
        .as_ref()
        .is_some_and(|chain| chain.revisions.iter().any(|revision| revision.linearized));

    // First update: to a new file through the streaming writer.
    let first = out.join(format!("{name}.1.pdf"));
    let mut file = File::create(&first).unwrap();
    let edited = (|| {
        let mut writer = AppendWriter::new(&reader, &mut file)?;
        let added = small_edit(&reader, &mut writer)?;
        Ok::<_, AppendError>((added, writer.finish()?))
    })();
    drop(file);
    let (added, summary) = match edited {
        Ok(done) => done,
        Err(error) => {
            let _ = std::fs::remove_file(&first);
            return json!({"status": "write_failed", "error": error.to_string()});
        }
    };
    let source: Arc<dyn RandomAccessSource> = Arc::new(FileSource::open(&first).unwrap());
    let after = match reader.verify_appended(source, &summary) {
        Ok(after) => after,
        Err(error) => return json!({"status": "verify_failed", "error": error.to_string()}),
    };

    // Every object, before against after.
    let changed: Vec<_> = summary.objects.iter().map(|object| object.id).collect();
    let (mut same, mut unread_both, mut differs, mut lost) = (0_u64, 0_u64, 0_u64, 0_u64);
    for id in reader.object_ids() {
        if changed.contains(&id) {
            continue;
        }
        match (reader.resolve_object(id), after.resolve_object(id)) {
            (Ok(before), Ok(now)) => {
                if before == now {
                    same += 1;
                } else {
                    differs += 1;
                }
            }
            (Err(_), Err(_)) => unread_both += 1,
            _ => lost += 1,
        }
    }
    let added_reads = matches!(after.resolve_object((added, 0)), Ok(Object::Dictionary(_)));

    // Second update: replacing a file, through the atomic path.
    let second = out.join(format!("{name}.2.pdf"));
    let again = append_atomically(
        &after,
        AppendDestination::NewFile(&second),
        AppendOptions::new(),
        |writer| {
            writer.add_object(&Object::Dictionary(dictionary! { "RectoRoundTrip" => 2 }))?;
            Ok(())
        },
    );
    let (summary_two, final_reader) = match again {
        Ok(done) => done,
        Err(error) => return json!({"status": "second_failed", "error": error.to_string()}),
    };
    let chain = final_reader.revisions().unwrap();
    let before_count = chain_before.as_ref().map_or(0, |chain| chain.revisions.len());

    // The original's revision comes back byte for byte.
    let original_revision = chain.revisions.get(before_count.wrapping_sub(1));
    let extraction = original_revision.and_then(|revision| revision.range.as_ref().map(|range| (revision, range.end)));
    let trailing = chain_before
        .as_ref()
        .and_then(|chain| chain.trailing_bytes)
        .unwrap_or(0);
    let revision_exact = extraction.map(|(revision, end)| {
        let mut sink = HashingSink(Sha256::new());
        final_reader.copy_revision_to(revision, &mut sink).unwrap();
        end == original_len - trailing && sink.0.finalize().to_vec() == digest_prefix(path, end)
    });
    // Every proved revision opens as the file it was, without a rescan, and the original's has the
    // original's objects.
    let older_opened = chain
        .revisions
        .iter()
        .filter(|revision| revision.range.is_some())
        .filter(|revision| {
            final_reader
                .open_revision(revision, lopdf::IndexedReaderOptions::default())
                .is_ok_and(|opened| opened.index_stats().is_ok_and(|stats| !stats.recovered()))
        })
        .count();
    let older_provable = chain
        .revisions
        .iter()
        .filter(|revision| revision.range.is_some())
        .count();
    let original_view_same = original_revision.is_some_and(|revision| {
        revision.range.is_none()
            || final_reader
                .open_revision(revision, lopdf::IndexedReaderOptions::default())
                .is_ok_and(|opened| opened.object_ids() == reader.object_ids())
    });
    let whole_original = digest_prefix(path, original_len) == digest_prefix(&second, original_len);
    let pages_after = final_reader.page_count().ok();
    let eager = (std::fs::metadata(&second).unwrap().len() < 64 * 1_024 * 1_024).then(|| {
        // Not `Document::load`, which is async under the `async` feature.
        std::fs::read(&second)
            .ok()
            .and_then(|bytes| lopdf::Document::load_mem(&bytes).ok())
            .is_some_and(|document| document.get_object((added, 0)).is_ok())
    });
    let _ = std::fs::remove_file(&first);

    let ok = differs == 0
        && lost == 0
        && added_reads
        && whole_original
        && revision_exact != Some(false)
        && pages_before == pages_after
        && eager != Some(false)
        && older_opened == older_provable
        && original_view_same;
    json!({
        "status": if ok { "ok" } else { "mismatch" },
        "bytes": original_len,
        "kind": kind,
        "hybrid": hybrid,
        "linearized": linearized,
        "revisions_before": before_count,
        "revisions_after": chain.revisions.len(),
        "regular_before": chain_before.as_ref().map(|chain| chain.is_regular()),
        "trailing_before": chain_before.as_ref().and_then(|chain| chain.trailing_bytes),
        "objects_same": same,
        "objects_unread_both": unread_both,
        "objects_differ": differs,
        "objects_lost": lost,
        "added": added,
        "added_reads": added_reads,
        "whole_original_prefix": whole_original,
        "revision_exact": revision_exact,
        "revisions_provable": older_provable,
        "revisions_opened": older_opened,
        "original_view_same": original_view_same,
        "pages_before": pages_before,
        "pages_after": pages_after,
        "eager_reads_new_object": eager,
        "appended_bytes": summary_two.new_len - original_len,
        "ms": started.elapsed().as_millis() as u64,
        "output": second,
    })
}

fn measure(mode: &str, out: &Path, path: &Path) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let target = out.join(format!("{name}.measure.pdf"));
    let mut phases = Vec::new();
    let mut mark = Instant::now();
    let mut lap = |label: &str| {
        phases.push(format!("{label} {:.1} ms", mark.elapsed().as_secs_f64() * 1e3));
        mark = Instant::now();
    };
    let reader = open(path).unwrap();
    reader.check_appendable().unwrap();
    lap("open+check");
    match mode {
        "open" => {}
        "copy" => {
            let mut file = File::create(&target).unwrap();
            let mut writer = AppendWriter::new(&reader, &mut file).unwrap();
            lap("copy prefix");
            small_edit(&reader, &mut writer).unwrap();
            let summary = writer.finish().unwrap();
            lap("write tail");
            println!("appended {} bytes", summary.new_len - summary.previous_len);
        }
        "atomic" => {
            let (summary, _) = append_atomically(
                &reader,
                AppendDestination::NewFile(&target),
                AppendOptions::new(),
                |writer| {
                    small_edit(&reader, writer)?;
                    Ok(())
                },
            )
            .unwrap();
            lap("copy, write, sync, verify, rename");
            println!("appended {} bytes", summary.new_len - summary.previous_len);
        }
        other => panic!("unknown mode {other}"),
    }
    println!("{}", phases.join(", "));
}
