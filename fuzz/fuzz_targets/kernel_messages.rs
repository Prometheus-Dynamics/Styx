//! What the kernel hands back, from bytes: uevents (netlink messages), the media graph
//! (`MEDIA_IOC_G_TOPOLOGY` arrays) with every link resolved, V4L2 events and formats.
//!
//! The first byte picks the decoder; the rest is its input.

#![no_main]

use libfuzzer_sys::fuzz_target;
use styx_kernel::uevent::Uevent;

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    match which % 4 {
        0 => {
            if let Some(u) = Uevent::parse(rest) {
                for key in [
                    "ACTION",
                    "SUBSYSTEM",
                    "DEVNAME",
                    "PRODUCT",
                    "BUSNUM",
                    "DEVNUM",
                ] {
                    let _ = u.get(key);
                }
            }
        }
        1 => styx_kernel::media::fuzz_topology(rest),
        2 => styx_kernel::event::fuzz_event(rest),
        _ => styx_kernel::v4l2::Format::fuzz_from_bytes(rest),
    }
});
