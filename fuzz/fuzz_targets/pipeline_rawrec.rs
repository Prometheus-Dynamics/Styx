//! Raw recordings (`<base>.jsonl` index and `<base>.raw` frames, `styx-pipeline`'s format for
//! replaying the control loop on a host) and the virtual sensor that replays them.
//!
//! Input: the index text, a NUL byte, then the frames' bytes.

#![no_main]

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use styx_algo::SensorRequest;
use styx_pipeline::rawrec::{RawRecording, unpack_row};
use styx_pipeline::replay::VirtualSensor;

fuzz_target!(|data: &[u8]| {
    let (index, frames) = match data.iter().position(|&b| b == 0) {
        Some(at) => (&data[..at], &data[at + 1..]),
        None => (data, &[][..]),
    };
    let Ok(rec) = RawRecording::from_reader(index, frames.to_vec()) else {
        return;
    };
    let h = &rec.header;
    let width = h.format.width as usize;
    if width <= 1 << 16 {
        let mut row = vec![0u16; width];
        for i in 0..rec.len() {
            let frame = rec.frame(i);
            unpack_row(h.format.packing, frame, width, &mut row);
        }
    }
    let mut sensor = VirtualSensor::new(&rec);
    let _ = (sensor.format(), sensor.stride());
    for frame in 0..3 {
        let _ = sensor.next_frame();
        sensor.request(&SensorRequest {
            frame: frame + 1,
            exposure: Duration::from_millis(5),
            analogue_gain: 2.0,
            frame_duration: Duration::from_millis(33),
        });
    }
});
