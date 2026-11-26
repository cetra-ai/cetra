#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    cetra_query::fuzz_entry::fuzz_sql(data);
});
