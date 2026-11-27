#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    cetra_column::fuzz_entry::column_frame(data);
});
