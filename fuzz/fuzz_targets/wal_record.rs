#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| llmobs_storage::fuzz_entry::wal_record(data));
