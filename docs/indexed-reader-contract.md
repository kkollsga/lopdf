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
stop at PDFium's limit of 4,194,304, and a table's offsets at 10 digits.

A file whose last byte isn't a line ending gets `\n%line-ending-added\n` before the first object, so that
the revision list can give back the original's exact length. A revision's range ends past the line ending
after its `%%EOF`, and `eof_marker_end` gives the end before it. A range exists only if a `startxref` names
the section and the sections run forward through the file. A linearized file's two sections count as one
revision. Revisions are listed on request and nothing about them is held open.
