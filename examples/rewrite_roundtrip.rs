//! Runs real PDFs through the rewrite, and measures it. Doesn't touch its inputs.
//!
//!   rewrite_roundtrip corpus <out-dir> <file>...           An identity rewrite and one removing a page, for each file, a JSON line each.
//!   rewrite_roundtrip measure <identity|remove:N> <out-dir> <file>   One rewrite, timed. N is a page's position.
//!
//! Wrap `measure` in `/usr/bin/time -l` for the whole process's peak footprint.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lopdf::{
    AppendDestination, FileSource, IndexedReader, RewriteError, RewriteOptions, RewritePolicy, RewriteSummary,
    rewrite_atomically,
};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("corpus") if args.len() >= 3 => {
            let out = PathBuf::from(&args[1]);
            for path in &args[2..] {
                let path = PathBuf::from(path);
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let mut line =
                    std::panic::catch_unwind(|| round_trip(&path, &out)).unwrap_or_else(|_| json!({"status": "panic"}));
                line["file"] = json!(name);
                println!("{line}");
                std::io::stdout().flush().unwrap();
            }
        }
        Some("measure") if args.len() == 4 => measure(&args[1], Path::new(&args[2]), Path::new(&args[3])),
        _ => {
            eprintln!(
                "usage: rewrite_roundtrip corpus <out-dir> <file>... | measure <identity|remove:N> <out-dir> <file>"
            );
            std::process::exit(2);
        }
    }
}

fn open(path: &Path) -> Result<IndexedReader, String> {
    let source = FileSource::open(path).map_err(|error| error.to_string())?;
    IndexedReader::open(source).map_err(|error| error.to_string())
}

fn error_kind(error: &RewriteError) -> String {
    format!("{error:?}")
        .split([' ', '{', '('])
        .next()
        .unwrap_or("")
        .to_string()
}

fn describe(summary: &RewriteSummary) -> serde_json::Value {
    json!({
        "new_len": summary.new_len,
        "form": format!("{:?}", summary.form),
        "objects_written": summary.objects_written,
        "objects_compressed": summary.objects_compressed,
        "object_streams": summary.object_streams,
        "objects_dropped": summary.objects_dropped,
        "stream_bytes_copied": summary.stream_bytes_copied,
        "removed_signatures": summary.removed_signatures.len(),
        "removal": {
            "pages": summary.removal.pages.len(),
            "page_tree_nodes": summary.removal.page_tree_nodes,
            "annotations": summary.removal.annotations,
            "links": summary.removal.links,
            "outline_items": summary.removal.outline_items,
            "named_destinations": summary.removal.named_destinations,
            "form_fields": summary.removal.form_fields,
            "structure_elements": summary.removal.structure_elements,
            "parent_tree_entries": summary.removal.parent_tree_entries,
            "page_labels_rebuilt": summary.removal.page_labels_rebuilt,
            "optional_content_groups": summary.removal.optional_content_groups,
            "resource_names_pruned": summary.removal.resource_names_pruned,
            "resources_left_whole": summary.removal.resources_left_whole.len(),
        },
    })
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
    if let Err(error) = reader.check_rewritable() {
        return json!({
            "status": "refused", "reason": error.to_string(), "kind": error_kind(&error), "bytes": original_len, "recovered": recovered,
        });
    }
    let pages = reader.page_count().ok();
    let signatures = reader.signatures().map(|found| found.len()).unwrap_or(0);
    // What the original can't give back, whatever a rewrite does: reachable or not, this was so before.
    let unread = reader
        .object_ids()
        .into_iter()
        .filter(|id| match reader.resolve_stream_descriptor(*id) {
            Ok(_) => false,
            Err(_) => reader.resolve_object(*id).is_err(),
        })
        .count();
    let policy = RewritePolicy::new().remove_signatures();

    let identity_path = out.join(format!("{name}.identity.pdf"));
    let identity = rewrite_atomically(
        &reader,
        AppendDestination::NewFile(&identity_path),
        RewriteOptions::new().policy(policy),
        |_| Ok(()),
    );
    let identity = match identity {
        Ok(done) => done,
        Err(error) => {
            return json!({
                "status": "identity_failed", "reason": error.to_string(), "kind": error_kind(&error), "bytes": original_len,
                "pages": pages, "signatures": signatures, "unread_in_original": unread,
            });
        }
    };
    let identity_pages = identity.reader.page_count().ok();
    let mut line = json!({
        "status": "ok",
        "bytes": original_len,
        "pages": pages,
        "signatures": signatures,
        "unread_in_original": unread,
        "identity": describe(&identity.summary),
        "identity_pages": identity_pages,
        "identity_output": identity_path,
        "identity_ms": started.elapsed().as_millis() as u64,
    });
    if pages != identity_pages {
        line["status"] = json!("identity_pages_differ");
    }

    // The middle page, or nothing to remove from a document of one.
    if let Some(pages) = pages.filter(|pages| *pages > 1) {
        let removed = pages / 2;
        let started = Instant::now();
        let removal_path = out.join(format!("{name}.removal.pdf"));
        let removal = rewrite_atomically(
            &reader,
            AppendDestination::NewFile(&removal_path),
            RewriteOptions::new().policy(policy),
            |changes| {
                changes.remove_page(removed);
                Ok(())
            },
        );
        match removal {
            Ok(done) => {
                let now = done.reader.page_count().ok();
                line["removed_index"] = json!(removed);
                line["removal"] = describe(&done.summary);
                line["removal_pages"] = json!(now);
                line["removal_output"] = json!(removal_path);
                line["removal_ms"] = json!(started.elapsed().as_millis() as u64);
                if now != Some(pages - 1) && line["status"] == "ok" {
                    line["status"] = json!("removal_pages_differ");
                }
            }
            Err(error) => {
                line["status"] = json!("removal_failed");
                line["reason"] = json!(error.to_string());
                line["kind"] = json!(error_kind(&error));
            }
        }
    }
    line
}

fn measure(mode: &str, out: &Path, path: &Path) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let target = out.join(format!("{name}.measure.pdf"));
    let opened = Instant::now();
    let reader = open(path).unwrap();
    println!("open {:.1} ms", opened.elapsed().as_secs_f64() * 1e3);
    let started = Instant::now();
    let policy = RewritePolicy::new().remove_signatures();
    let done = rewrite_atomically(
        &reader,
        AppendDestination::NewFile(&target),
        RewriteOptions::new().policy(policy),
        |changes| {
            if let Some(index) = mode.strip_prefix("remove:") {
                changes.remove_page(index.parse().unwrap());
            }
            Ok(())
        },
    )
    .unwrap();
    println!(
        "rewrite {:.1} ms, {} -> {} bytes, {:?}, {} objects written, {} dropped, {} stream bytes",
        started.elapsed().as_secs_f64() * 1e3,
        done.summary.previous_len,
        done.summary.new_len,
        done.summary.form,
        done.summary.objects_written,
        done.summary.objects_dropped,
        done.summary.stream_bytes_copied,
    );
    drop(done);
    drop(reader);
    let _ = std::fs::remove_file(&target);
}
