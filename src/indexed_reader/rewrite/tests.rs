//! Failures that can only be set up from inside: stopping at each step of a save, and a write that fails at each point.

use super::super::append::sealed;
use super::*;

fn document() -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    let mut add = |bytes: &mut Vec<u8>, body: String| {
        offsets.push(bytes.len());
        let number = offsets.len();
        bytes.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
    };
    add(&mut bytes, "<< /Type /Catalog /Pages 2 0 R >>".to_string());
    add(
        &mut bytes,
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 9 9] >>".to_string(),
    );
    add(
        &mut bytes,
        "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>".to_string(),
    );
    add(
        &mut bytes,
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_string(),
    );
    add(&mut bytes, "<< /Length 7 >>\nstream\nkept me\nendstream".to_string());
    add(&mut bytes, "<< /Length 7 >>\nstream\nsecret!\nendstream".to_string());
    let xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    bytes.extend_from_slice(format!("1 {}\n", offsets.len()).as_bytes());
    for offset in &offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    bytes
}

const STEPS: [RewriteStep; 7] = [
    RewriteStep::TemporaryCreated,
    RewriteStep::Written,
    RewriteStep::Synced,
    RewriteStep::Prepared,
    RewriteStep::PermissionsSet,
    RewriteStep::Verified,
    RewriteStep::AboutToCommit,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Same,
    New,
    Other,
}

fn destination<'p>(
    target: Target, same: &'p std::path::Path, new: &'p std::path::Path, other: &'p std::path::Path,
) -> AppendDestination<'p> {
    match target {
        Target::Same => AppendDestination::SameFile(same),
        Target::New => AppendDestination::NewFile(new),
        Target::Other => AppendDestination::OverwriteOther(other),
    }
}

fn names_in(folder: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Failing at each step of a save, for each destination: the original and the other file have to be exactly as they were, nothing new can be at the
/// target, and no temporary name can be left, whichever step it was.
#[test]
fn a_failure_at_any_step_leaves_everything_as_it_was() {
    for target in [Target::Same, Target::New, Target::Other] {
        for stop in STEPS {
            let folder = tempfile::tempdir().unwrap();
            let same = folder.path().join("doc.pdf");
            let new = folder.path().join("new.pdf");
            let other = folder.path().join("other.pdf");
            let original = document();
            std::fs::write(&same, &original).unwrap();
            std::fs::write(&other, b"someone else's").unwrap();
            let before = names_in(folder.path());
            let reader = IndexedReader::open(crate::source::FileSource::open(&same).unwrap()).unwrap();
            let mut reached = false;
            let result = rewrite_atomically_with_steps(
                &reader,
                destination(target, &same, &new, &other),
                RewriteOptions::new(),
                |changes| {
                    changes.remove_page(1);
                    Ok(())
                },
                |step| {
                    if step == stop {
                        reached = true;
                        return Err(std::io::Error::other("stopped"));
                    }
                    Ok(())
                },
            );
            assert!(reached, "{target:?} never got to {stop:?}");
            assert!(
                matches!(result, Err(RewriteError::Io(_))),
                "{target:?} at {stop:?}: {result:?}"
            );
            assert_eq!(std::fs::read(&same).unwrap(), original, "{target:?} at {stop:?}");
            assert_eq!(
                std::fs::read(&other).unwrap(),
                b"someone else's",
                "{target:?} at {stop:?}"
            );
            assert_eq!(names_in(folder.path()), before, "{target:?} at {stop:?}");
        }
    }
}

/// With no step stopped, the same run has to end with the file in place: otherwise the test above proves nothing.
#[test]
fn with_no_failure_the_file_is_in_place() {
    for target in [Target::Same, Target::New, Target::Other] {
        let folder = tempfile::tempdir().unwrap();
        let same = folder.path().join("doc.pdf");
        let new = folder.path().join("new.pdf");
        let other = folder.path().join("other.pdf");
        std::fs::write(&same, document()).unwrap();
        std::fs::write(&other, b"someone else's").unwrap();
        let reader = IndexedReader::open(crate::source::FileSource::open(&same).unwrap()).unwrap();
        let mut steps = Vec::new();
        let done = rewrite_atomically_with_steps(
            &reader,
            destination(target, &same, &new, &other),
            RewriteOptions::new(),
            |changes| {
                changes.remove_page(1);
                Ok(())
            },
            |step| {
                steps.push(step);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(steps.last(), Some(&RewriteStep::Committed));
        assert_eq!(done.reader.page_count().unwrap(), 1);
        let placed = match target {
            Target::Same => &same,
            Target::New => &new,
            Target::Other => &other,
        };
        let written = std::fs::read(placed).unwrap();
        assert!(!written.windows(7).any(|window| window == b"secret!"));
        assert!(written.windows(7).any(|window| window == b"kept me"));
    }
}

/// The test below re-runs this binary with these set, to be killed at a step.
const KILL_FOLDER: &str = "LOPDF_REWRITE_KILL_FOLDER";
const KILL_STEP: &str = "LOPDF_REWRITE_KILL_STEP";
const KILL_TARGET: &str = "LOPDF_REWRITE_KILL_TARGET";

/// Not a test on its own: run by the one below, in a process of its own, which dies at the step it's told to, the way a crash or a power cut would stop
/// a save, with no destructors and no clean-up.
#[test]
fn killed_child() {
    let Some(folder) = std::env::var_os(KILL_FOLDER) else {
        return;
    };
    let stop: usize = std::env::var(KILL_STEP).unwrap().parse().unwrap();
    let target = match std::env::var(KILL_TARGET).unwrap().as_str() {
        "Same" => Target::Same,
        "New" => Target::New,
        _ => Target::Other,
    };
    let folder = std::path::PathBuf::from(folder);
    let same = folder.join("doc.pdf");
    let new = folder.join("new.pdf");
    let other = folder.join("other.pdf");
    let reader = IndexedReader::open(crate::source::FileSource::open(&same).unwrap()).unwrap();
    let mut count = 0;
    let _ = rewrite_atomically_with_steps(
        &reader,
        destination(target, &same, &new, &other),
        RewriteOptions::new(),
        |changes| {
            changes.remove_page(1);
            Ok(())
        },
        |_| {
            if count == stop {
                std::process::abort();
            }
            count += 1;
            Ok(())
        },
    );
    // It was told to die past the last step.
    std::process::abort();
}

/// Killed at each step of a save, for each destination. Before the file is in place, what was there has to be byte for byte what it was; after, the new file has to be
/// there, whole. A killed process can't clean up, so what's left is its temporary file and nothing else, and the next save to that target clears it once it's old
/// enough that its owner can't be working on it.
#[cfg(unix)]
#[test]
fn a_kill_at_any_step_leaves_the_original_or_a_whole_new_file() {
    use crate::source::FileSource;
    let test_binary = std::env::current_exe().unwrap();
    for target in [Target::Same, Target::New, Target::Other] {
        // TemporaryCreated through AboutToCommit, then Committed.
        for stop in 0..=STEPS.len() {
            let folder = tempfile::tempdir().unwrap();
            let same = folder.path().join("doc.pdf");
            let new = folder.path().join("new.pdf");
            let other = folder.path().join("other.pdf");
            let original = document();
            std::fs::write(&same, &original).unwrap();
            std::fs::write(&other, b"someone else's").unwrap();
            let status = std::process::Command::new(&test_binary)
                .args([
                    "--exact",
                    "indexed_reader::rewrite::tests::killed_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(KILL_FOLDER, folder.path())
                .env(KILL_STEP, stop.to_string())
                .env(KILL_TARGET, format!("{target:?}"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success(), "{target:?} at {stop}: the child wasn't killed");
            let committed = stop == STEPS.len();
            let placed = match target {
                Target::Same => &same,
                Target::New => &new,
                Target::Other => &other,
            };
            if committed {
                let written = std::fs::read(placed).unwrap();
                assert!(written.ends_with(b"%%EOF\n"), "{target:?}: the new file isn't whole");
                let opened = IndexedReader::open(FileSource::open(placed).unwrap()).unwrap();
                assert_eq!(opened.page_count().unwrap(), 1);
                assert!(
                    !names_in(folder.path()).iter().any(|name| name.starts_with('.')),
                    "{target:?}"
                );
                continue;
            }
            assert_eq!(std::fs::read(&same).unwrap(), original, "{target:?} at {stop}");
            assert_eq!(
                std::fs::read(&other).unwrap(),
                b"someone else's",
                "{target:?} at {stop}"
            );
            assert!(!new.exists(), "{target:?} at {stop}");
            let leftovers: Vec<String> = names_in(folder.path())
                .into_iter()
                .filter(|name| name.starts_with('.'))
                .collect();
            assert_eq!(leftovers.len(), 1, "{target:?} at {stop}: {leftovers:?}");

            // The next save gets past it, and clears it once it's old enough.
            let leftover = folder.path().join(&leftovers[0]);
            let old = std::time::SystemTime::now() - super::super::append::STALE_TEMPORARY_AGE * 2;
            std::fs::File::options()
                .write(true)
                .open(&leftover)
                .unwrap()
                .set_modified(old)
                .unwrap();
            let reader = IndexedReader::open(FileSource::open(&same).unwrap()).unwrap();
            rewrite_atomically(
                &reader,
                destination(target, &same, &new, &other),
                RewriteOptions::new(),
                |_| Ok(()),
            )
            .unwrap();
            assert!(
                !leftover.exists(),
                "{target:?} at {stop}: the next save didn't clear what the kill left"
            );
        }
    }
}

/// A write that fails anywhere being swallowed, which would leave a file with a gap under offsets that assume none. Every point in the file, in both forms.
#[test]
fn a_write_that_fails_at_any_point_fails_the_rewrite() {
    struct FailsAfter(usize);
    impl Write for FailsAfter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0 < bytes.len() {
                return Err(std::io::Error::other("disk full"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl sealed::Sealed for FailsAfter {}
    impl AppendOutput for FailsAfter {}

    let bytes = document();
    let reader = IndexedReader::open(crate::source::BytesSource::from(bytes)).unwrap();
    for form in [RewriteForm::Table, RewriteForm::Stream] {
        let policy = RewritePolicy::new().form(RewriteFormChoice::Force(form));
        let length = RewriteChanges::new(&reader)
            .unwrap()
            .write_unverified_to(policy, std::io::sink())
            .unwrap()
            .summary()
            .new_len as usize;
        // The output buffer holds 64 KiB, so a small file only reaches its writer at the end, in one piece.
        for allowed in 0..length {
            let result = RewriteChanges::new(&reader)
                .unwrap()
                .write_unverified_to(policy, FailsAfter(allowed));
            assert!(
                matches!(result, Err(RewriteError::Io(_))),
                "{form:?} at {allowed}: {result:?}"
            );
        }
        assert!(
            RewriteChanges::new(&reader)
                .unwrap()
                .write_unverified_to(policy, FailsAfter(length))
                .is_ok()
        );
    }
}

/// A shared dictionary being searched with a decoder that gives what it can of a damaged stream, which misses names past the damage.
#[test]
fn only_a_whole_deflate_stream_counts_as_read() {
    use flate2::write::ZlibEncoder;
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![b'/'; 100_000]).unwrap();
    encoder.write_all(b"Name").unwrap();
    let whole = encoder.finish().unwrap();
    assert_eq!(resources::inflate_whole(&whole, 1 << 20).unwrap().len(), 100_004);
    assert!(
        resources::inflate_whole(&whole[..whole.len() / 2], 1 << 20).is_none(),
        "truncated"
    );
    assert!(resources::inflate_whole(&whole, 1_000).is_none(), "over the limit");
    let mut wrong_check = whole.clone();
    let last = wrong_check.len() - 1;
    wrong_check[last] ^= 0xFF;
    assert!(
        resources::inflate_whole(&wrong_check, 1 << 20).is_none(),
        "its check doesn't match"
    );
}

/// The name search missing a name because of what's around it, which takes out something a page uses.
#[test]
fn the_name_search_finds_every_name_wherever_it_is() {
    let mut found = Vec::new();
    resources::every_name(b"q/A Do(/B)Tj/C#20D[/E]<</F 1>>BI ID \xFF/G\xFFEI /H", |name| {
        found.push(name.to_vec())
    });
    let found: Vec<String> = found
        .iter()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    for name in ["A", "B", "C D", "C#20D", "E", "F", "G\u{FFFD}EI", "H"] {
        assert!(found.iter().any(|known| known == name), "{name} in {found:?}");
    }
}

/// Past the limit on how many resource dictionaries are listed, the rest used to be skipped without a word, and a removed page's content in one of them
/// stayed with an empty report. The limit is 64 in this build. Here 70 removed pages each have their own, and the last also shares one with the kept page.
#[test]
fn resource_dictionaries_past_the_limit_are_reported() {
    const PAGES: u32 = 71;
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = std::collections::BTreeMap::new();
    let mut add = |bytes: &mut Vec<u8>, number: u32, body: String| {
        offsets.insert(number, bytes.len());
        bytes.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
    };
    let kids: Vec<String> = (0..PAGES).map(|page| format!("{} 0 R", 100 + page)).collect();
    add(&mut bytes, 1, "<< /Type /Catalog /Pages 2 0 R >>".to_string());
    add(
        &mut bytes,
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {PAGES} /MediaBox [0 0 9 9] >>",
            kids.join(" ")
        ),
    );
    add(&mut bytes, 3, "<< /Shared 4 0 R /Gone 5 0 R >>".to_string());
    add(
        &mut bytes,
        4,
        "<< /Subtype /Form /BBox [0 0 9 9] /Length 4 >>\nstream\n% ok\nendstream".to_string(),
    );
    add(
        &mut bytes,
        5,
        "<< /Subtype /Form /BBox [0 0 9 9] /Length 8 >>\nstream\n% M_GONE\nendstream".to_string(),
    );
    add(
        &mut bytes,
        6,
        "<< /Length 10 >>\nstream\n/Shared Do\nendstream".to_string(),
    );
    add(
        &mut bytes,
        7,
        "<< /Length 8 >>\nstream\n/Gone Do\nendstream".to_string(),
    );
    // The kept page, sharing 3.
    add(
        &mut bytes,
        100,
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Resources << /XObject 3 0 R >> >>".to_string(),
    );
    for page in 1..PAGES {
        let resources = if page == PAGES - 1 {
            "<< /XObject 3 0 R >>".to_string()
        } else {
            format!("<< /XObject {} 0 R >>", 1_000 + page)
        };
        add(
            &mut bytes,
            100 + page,
            format!("<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources {resources} >>"),
        );
        add(&mut bytes, 1_000 + page, "<< /Own 4 0 R >>".to_string());
    }
    let xref = bytes.len();
    let size = offsets.keys().next_back().unwrap() + 1;
    bytes.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n").as_bytes());
    for (number, offset) in &offsets {
        bytes.extend_from_slice(format!("{number} 1\n{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    let reader = IndexedReader::open(crate::source::BytesSource::from(bytes)).unwrap();
    let mut changes = RewriteChanges::new(&reader).unwrap();
    for page in 1..PAGES as usize {
        changes.remove_page(page);
    }
    let mut output = Vec::new();
    let written = changes.write_unverified_to(RewritePolicy::new(), &mut output).unwrap();
    let report = &written.summary().removal;
    let still_there = output.windows(6).any(|window| window == b"M_GONE");
    assert!(
        !still_there || report.resource_dictionaries_unlisted > 0 || !report.resources_left_whole.is_empty(),
        "the removed page's content stayed and nothing said so: {report:?}"
    );
}
