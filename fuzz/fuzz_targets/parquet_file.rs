#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| llmobs_storage::fuzz_entry::parquet_file(data));
