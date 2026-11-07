#![no_main]

include!("../../tests/common/append_property.rs");

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = check_append_on_any_bytes(data);
});
