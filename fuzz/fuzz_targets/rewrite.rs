#![no_main]

include!("../../tests/common/rewrite_property.rs");

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = check_rewrite_on_any_bytes(data);
});
