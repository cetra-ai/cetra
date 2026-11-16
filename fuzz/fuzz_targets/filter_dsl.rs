#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| cetra_storage::fuzz_entry::filter_dsl(data));
