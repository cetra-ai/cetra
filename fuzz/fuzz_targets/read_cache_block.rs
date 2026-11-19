#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| cetra_storage::fuzz_entry::read_cache_block(data));
