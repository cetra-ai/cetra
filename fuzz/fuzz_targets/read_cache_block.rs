#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| llmobs_storage::fuzz_entry::read_cache_block(data));
