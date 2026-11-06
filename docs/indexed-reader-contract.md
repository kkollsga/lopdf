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
