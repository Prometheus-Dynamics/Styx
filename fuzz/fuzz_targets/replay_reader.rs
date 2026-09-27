//! Recordings (MCAP and `.styxrec`) from arbitrary bytes.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use styx::replay::open_recording_reader;

fuzz_target!(|data: &[u8]| {
    if let Ok((_, frames)) = open_recording_reader(Cursor::new(data.to_vec()), data.len() as u64) {
        for frame in frames.take(256).flatten() {
            let _ = frame.to_visible_vec();
        }
    }
});
