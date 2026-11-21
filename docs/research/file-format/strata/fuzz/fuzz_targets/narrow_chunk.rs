#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| strata::fuzz_entry::narrow_chunk(data));
