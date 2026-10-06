//! Messages a camera service and its clients read from other processes, from arbitrary bytes:
//! requests, frames (with their hops trailer, decoded into a reused frame as a client does),
//! releases (with the client's receive and import times), answers.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    styx::ipc::fuzz_messages(data);
});
