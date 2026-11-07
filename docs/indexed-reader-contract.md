# Indexed-reader fixture contract

Indexed-reader unit tests take their expected values from the bytes their fixture builders emit.
They do not use the eager loader as a moving oracle. Assertions cover complete objects, including
dictionary values, stream lengths, strings after decryption, object-stream member indices, page
order, and xref locations.

Cross-engine comparison remains appropriate only when agreement between the public eager and
indexed routes is itself the behavior under test. The object-stream xref-authority integration
matrix is the canonical example: it requires both routes to honor the final xref after members are
freed, moved, reused, or superseded. Eager-only reconstruction and xref tests likewise remain tied
to the eager loader.

Malformed inputs may deliberately differ. Such a test must name the divergence and assert each
route's expected result independently; one route must never manufacture the other's expectation at
runtime.

The currently pinned divergences are:

- malformed stream bodies may remain complete dictionaries on the indexed scalar route while the
  eager loader rejects the indirect object;
- selected object-stream resolution may fall back to declared raw member bytes when unbounded eager
  construction rejects an unsupported container filter;
- indexed xref opening uses conservative bounded framing and typed recovery/refusal, while eager
  reconstruction may accept a different damaged-file envelope.
- a free row beside an `/XRefStm` that cannot be read may be a placeholder, so the eager loader
  takes that object from its container, while the indexed reader refuses the section and answers
  from its rescan;
- a page-tree kid that does not load keeps its slot as a stand-in page on the eager route and is
  left out of the indexed page map;
- a string that cannot be decrypted leaves the eager object partly decrypted with a warning, while
  the indexed reader refuses the whole object with `ObjectDecryption`.

# Append writer contract

`AppendWriter` writes an incremental update from an open `IndexedReader` and a streaming output. The
bytes before the update never change. Tests build their inputs from bytes and check the output with
the indexed reader and, separately, the eager loader, in `tests/append_writer.rs`.

The new section takes the form of the file's newest section: a table for a table or a hybrid, a stream
for a stream. A hybrid's new table carries no `/XRefStm`; its old supplement is still reached through
`/Prev`. The new trailer keeps every entry of the old one except those that describe the old section
(`/Size`, `/Prev`, `/XRefStm`, and a stream section's own `/Type`, `/W`, `/Index`, `/Length`, filters).
The first string of `/ID` never changes and the second does. A file with no `/ID` gets one, and an `/ID`
that isn't two direct strings is left alone.

It refuses, before writing anything: an index rebuilt by a scan, encryption, a header not at byte 0, a
`/Prev` loop, a chain already at `max_xref_sections`, a `/XRefStm` that redefines a live entry of its own
table or sits on a stream section, and a `/Root` that isn't a reference to a dictionary. New object numbers
stop at PDFium's limit of 4,194,304, and a table's offsets at 10 digits. An array or dictionary nested to the
parser's 100th level is refused, and a string's parentheses past that depth go out escaped, so what's written
can always be read. `verify_appended` parses every object written, without reading a stream's data, and
compares what the trailer carries.

There is no way to append to a file in place. `AppendWriter` takes a new, empty output, and a file on disk goes
through `append_atomically`, which says where the result goes:

- `AppendDestination::SameFile` is Save. The path has to still be the file the reader opened: same device,
  inode, length and modification time, checked before the write and just before the rename, or `TargetChanged`.
  A reader of bytes or of an earlier revision has no file of its own, and gets `TargetIdentityUnknown`.
- `NewFile` is Save As to an empty path, moved into place by hard link so that it can't overwrite.
- `OverwriteOther` replaces another file the caller named deliberately, and refuses if it changes meanwhile.

A symbolic link is followed to the real file. A read-only target, or one with other hard links unless
`AppendOptions::allow_breaking_hard_links` is set, is refused. **A replaced file gets the old one's name and mode
bits and nothing else:** extended attributes, including Finder tags and `com.apple.quarantine`, ACLs, owner and
birth time are lost unless `AppendOptions::prepare_replacement` copies them, which runs on the finished temporary
file before it's checked and moved. The crate forbids unsafe code and can't copy them itself.

The temporary file is created exclusively, at the mode of the file it becomes, and named from a hash of the target's
name. A later save for the same target removes a leftover whose process is gone, if it's a regular file of this user's,
over an hour old, and its name fits the pattern exactly. After the rename, only `AppendedFile::durability_warning` can
report a problem, since the file is already in place.

A small gap remains between the last check and the rename, which safe code can't close.

A file whose last byte isn't a line ending gets `\n%line-ending-added\n` before the first object, so that
the revision list can give back the original's exact length. A revision's range ends past the line ending
after its `%%EOF`, and `eof_marker_end` gives the end before it. A range exists only if a `startxref` names
the section, the sections run forward through the file, and every section down its chain, and any `/XRefStm`, is
inside it. A linearized file's two sections count as one revision, as do sections closed by one `startxref`.
`open_revision` refuses bytes that would need a rescan. Revisions are listed on request and nothing about them is
held open.

One walk of the `/Prev` chain is bounded at `max_xref_sections` sections, 4,194,304 different numbers, 33,554,432
entries listed, and four times the file's length read, plus 8 MiB. No chain of sections that don't overlap can reach
the last. None of these fall back to a rescan.
