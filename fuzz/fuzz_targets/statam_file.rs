#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    cetra_statam::fuzz_entry::fuzz_statam_file(data);
});
