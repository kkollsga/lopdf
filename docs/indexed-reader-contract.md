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
can always be read. `verify_appended` compares every original byte, parses every object written, compares what the
trailer carries, and checks every added byte against a hash, all in small pieces.

There is no way to append to a file in place, and no way to give `AppendWriter` a file: its output is a sealed trait,
implemented for memory, `Sink`, and a temporary file only `append_atomically` can make. A file on disk goes through
`append_atomically`, which says where the result goes:

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

The temporary file is created exclusively, no more readable than the file it becomes, writable by its owner until the
prepare step has run, and named from a hash of the target's name. A later save for the same target removes a leftover whose
process is gone, if it's a regular file of this user's, over an hour old, and its name fits the pattern exactly, asking `ps`
about at most 16 processes. One of these names that's a link to the target itself is removed whatever its age, since it isn't
a hard link anyone made. After the rename, only `AppendedFile::durability_warning` can report a problem, since the file is
already in place.

A small gap remains between the last check and the rename, which safe code can't close.

A file whose last byte isn't a line ending gets `\n%line-ending-added\n` before the first object, so that
the revision list can give back the original's exact length. A revision's range ends past the line ending
after its `%%EOF`, and `eof_marker_end` gives the end before it. A range exists only if a `startxref` names
the section, the sections run forward through the file, and every section down its chain, and any `/XRefStm`, is
inside it. A linearized file's two sections count as one revision, as do sections closed by the very same `startxref`. Any other section
sitting inside bytes an earlier revision holds has no range, and `is_regular` is false.
`open_revision` refuses bytes that would need a rescan. Revisions are listed on request and nothing about them is
held open.

One walk of the `/Prev` chain is bounded at `max_xref_sections` sections, 4,194,304 different numbers, 33,554,432
entries declared, charged before they're decoded, 256 MiB inflated, and four times the file's length read, plus 8 MiB. No chain of sections that don't overlap can reach
the last. None of these fall back to a rescan.

# Rewrite contract

`RewriteChanges` and `rewrite_atomically` write a whole new file from an open `IndexedReader`, holding only what the new trailer can
reach, without loading the original. The module docs in `src/indexed_reader/rewrite/mod.rs` say what happens to each kind of reference
to a removed page. This is what the tests in `tests/rewrite.rs` and `src/indexed_reader/rewrite/tests.rs` hold it to.

**Old data is gone.** The main test gives everything that should go a marker of its own, and searches the raw bytes and every inflated
stream of the result for each, plainly, in hex and as UTF-16, with a search that doesn't use a reader. Everything that should stay has a marker
too. That runs for a classic table, a cross-reference stream with object streams, and a hybrid.

**The output's form.** A file with only tables and no object streams stays that way, since whatever reads it today may not read anything newer.
Any other gets a single cross-reference stream and new object streams of at most 100 members or 64 KiB, or it would grow by every object it had compressed.
Past 500,000 objects a table can't be read back, so those get the stream form, and past 900,000 a rewrite is refused. Object numbers and generations stay, so references need no rewriting
and what a caller holds stays valid. A linearized file isn't linearized any more. The trailer carries `/Root`, `/Info` and `/ID`, and nothing else.

**Stream data isn't decoded,** except to look for names in a `/Resources` dictionary a removed page could draw from that something kept can reach. Every
kept thing that could draw from one is searched, following names in every category, under its own resources and everything above it, since PDFium falls
back a category at a time and other viewers a name at a time. Only a stream decoded to its end counts, since the general decoder gives what it can of a
damaged one, and any doubt leaves that dictionary whole and says so in `RemovalReport::resources_left_whole`. A wrong `/Length` is replaced by what's
found the way a viewer finds it, by looking for `endstream`, and an object a little off its offset is found within 4 KiB.

**What stays is said.** `RewriteSummary::document_data_kept` counts what's the document's rather than a page's, which can repeat a removed page's words:
embedded files, XMP, `/Info`, JavaScript, `/PieceInfo`, document parts, XFA, destinations by position, hidden layers. The caller has to say so, or offer to
remove it, before saying anything is gone.

**Destinations** are `AppendDestination`, with the same checks, temporary file, sync order and `prepare_replacement`. `verify_rewritten` hashes every byte of
the finished file, opens it, and requires one section, no `/Prev`, no rescan, every object where it was written and readable, nothing dead, and the
right pages. A failure at any step, or a kill, leaves what was at the destination byte for byte. A kill can't remove its temporary file, and a later save to that target does, once it's an hour old and its process is gone.

**Signatures.** A rewritten copy of a signed file is unsigned, never broken (RectoPDF's decision 27). A file with one is refused unless
`RewritePolicy::remove_signatures` gives consent, and even then never over the signed file itself. Signatures are found by form field, by `/Perms`, by every
`/ByteRange` name in the file's bytes, however it's spelled and wherever it falls, which covers old revisions, and by anything the walk comes across. With
consent every signature goes: the field, its value, its widgets and their appearances, `/Perms` with certification and usage rights, `/SigFlags`, `/DSS`. The
check proves the copy has none. Usage rights are classed as their own kind, and need the same consent. Nothing here checks one or says one is valid.

**Refused:** encryption, a rebuilt index, a `/Prev` loop, an ambiguous hybrid, removing every page, and an object that can be reached and can't be read.

**Known limits.** Reals pass through this crate's `f32`, as they do in PDFium: `612.00001` comes back `612.0`. A structure element that spans kept and removed
pages keeps its own `/ActualText` and `/Alt`, which may describe both; it's counted. `/DPartRoot` and web-capture structures keep their shape with the removed
page's references dropped. The check after writing is structural: bytes, cross-reference data, every object readable, the page count, no signature where there
should be none. It doesn't render anything, so a kept page drawn differently would pass it; the corpus runs compare renders. `write_unverified_to` is
unchecked until its `verify` is called. A hostile object costs up to about ninety times its bytes once parsed, within the reader's object limit.
