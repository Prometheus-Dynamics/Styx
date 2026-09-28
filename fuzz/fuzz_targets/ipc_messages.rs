//! Messages a camera service and its clients read from other processes, from arbitrary bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    styx::ipc::fuzz_messages(data);
});
