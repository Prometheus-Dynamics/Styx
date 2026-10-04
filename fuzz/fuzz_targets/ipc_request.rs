//! Camera service requests (wire format 4: a `FrameRequest` and optionally a camera name) as a
//! client process sends them: decoded, checked as the service checks them, and planned on
//! virtual cameras, alone and shared with another client.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    styx::ipc::fuzz_service_request(data);
});
