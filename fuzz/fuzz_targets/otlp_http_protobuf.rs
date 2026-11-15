#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| llmobs_otel::fuzz_entry::otlp_http_protobuf(data));
