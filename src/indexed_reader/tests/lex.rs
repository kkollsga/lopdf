//! Indexed-reader tests: lex.

use super::*;

#[test]
fn incomplete_dictionary_probe_tracks_nesting_and_pdf_strings() {
    assert!(dictionary_may_be_truncated(b"<< /Nested << /Value 1 >>"));
    assert!(!dictionary_may_be_truncated(b"<< /Broken @"));
    assert!(!dictionary_may_be_truncated(b"<< /Nested << /Value 1 >> >>"));
    assert!(dictionary_may_be_truncated(b"<< /Text (a >> nested \\) value)"));
    assert!(!dictionary_may_be_truncated(
        b"<< /Text (a >> nested \\) value) /Hex <3e3e> >>"
    ));
}
