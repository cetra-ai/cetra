#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| cetra_saml::fuzz_entry::saml_metadata(data));
