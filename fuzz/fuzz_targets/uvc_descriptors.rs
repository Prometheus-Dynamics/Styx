//! USB device and configuration descriptors with UVC class descriptors (what a camera, or
//! anything claiming to be one, sends), and everything Styx derives from them: formats, frame
//! sizes and intervals, alternate settings by bandwidth, controls, mode lookup.

#![no_main]

use libfuzzer_sys::fuzz_target;
use styx_uvc::controls::{Unit, announced};
use styx_uvc::descriptors::UvcFunction;
use styx_uvc::{find_mode, interval_fraction};

fuzz_target!(|data: &[u8]| {
    let Ok(f) = UvcFunction::parse(data) else {
        return;
    };
    let c = &f.control;
    for (unit, bitmap) in [
        (Unit::CameraTerminal, c.camera_terminal()),
        (Unit::ProcessingUnit, c.processing_unit()),
    ] {
        if let Some((_, bitmap)) = bitmap {
            std::hint::black_box(announced(unit, bitmap).count());
        }
    }
    for e in &c.entities {
        std::hint::black_box(e.id());
    }
    for vs in &f.streaming {
        let _ = vs.transfer();
        for payload in [0, 1, 1024, 3072, usize::MAX] {
            let _ = vs.alt_for_bandwidth(payload);
        }
        for alt in &vs.alts {
            for ep in &alt.endpoints {
                let _ = (ep.is_in(), ep.bytes_per_interval());
            }
        }
        for format in &vs.formats {
            let fourcc = format.fourcc();
            let _ = format.frame(format.default_frame);
            for frame in &format.frames {
                let _ = format.frame_bytes(frame);
                let _ = interval_fraction(frame.default_interval);
                for iv in frame.intervals.listed(frame.default_interval) {
                    let _ = interval_fraction(iv);
                }
                for wanted in [0, 1, 333_333, frame.default_interval, u32::MAX] {
                    let _ = frame.intervals.closest(wanted);
                }
                if let Some(fourcc) = fourcc {
                    for interval in [None, Some(333_333), Some(u32::MAX)] {
                        let _ = find_mode(
                            &f,
                            fourcc,
                            frame.width.into(),
                            frame.height.into(),
                            interval,
                        );
                    }
                }
            }
        }
    }
});
