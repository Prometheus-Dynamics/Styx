//! Messages a camera service and its clients read from other processes, from arbitrary bytes:
//! requests, frames (with their hops trailer, decoded into a reused frame as a client does),
//! releases (with the client's receive and import times), answers (with the client's token),
//! and camera control requests, replies, events and control lists (each decoded one must
//! encode and decode again).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    styx::ipc::fuzz_messages(data);
});
