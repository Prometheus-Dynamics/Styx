//! A virtual sensor over a raw recording: each frame is a recorded frame re-exposed for the
//! exposure and gain the loop asked for (light above black scales linearly, clipped at full
//! scale), with requests landing on the frames they name. The closed loop (requests, delays,
//! convergence) then runs on a host with real image content.
//!
//! Re-exposing is exact for exposure steps down to the recorded level and for pixels the
//! recording did not clip; brighter re-exposures of clipped pixels stay clipped (as they would
//! on the sensor), darker ones keep the clipped value's scaled level.

use std::collections::BTreeMap;
use std::time::Duration;

use styx_algo::SensorRequest;
use styx_softisp::{RawFormat, RawPacking};

use crate::controller::SensorValues;
use crate::rawrec::RawRecording;
use crate::reexpose::{Recorded, exposure_ratio, re_expose};

/// The virtual sensor. Frames come out as 16-bit little-endian samples at the recording's
/// bit depth.
pub struct VirtualSensor<'a> {
    rec: &'a RawRecording,
    pending: BTreeMap<u64, SensorRequest>,
    current: SensorValues,
    next: u64,
    brightness: Box<dyn Fn(u64) -> f64 + Send + 'a>,
    out: Vec<u8>,
    row: Vec<u16>,
}

impl std::fmt::Debug for VirtualSensor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualSensor")
            .field("current", &self.current)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl<'a> VirtualSensor<'a> {
    /// A sensor replaying `rec` (cycling through its frames), starting at its first frame's
    /// values.
    pub fn new(rec: &'a RawRecording) -> Self {
        let first = rec.frames.first().map_or(
            SensorValues {
                frame: 0,
                exposure: Duration::from_millis(10),
                analogue_gain: 1.0,
                digital_gain: 1.0,
                frame_duration: Duration::from_millis(33),
                verified: false,
            },
            |f| f.sensor,
        );
        let w = rec.header.format.width as usize;
        Self {
            rec,
            pending: BTreeMap::new(),
            current: SensorValues { frame: 0, ..first },
            next: 0,
            brightness: Box::new(|_| 1.0),
            out: Vec::new(),
            row: vec![0; w],
        }
    }

    /// Scene brightness relative to the recording, per frame (e.g. a step at frame 60).
    pub fn set_brightness(&mut self, f: impl Fn(u64) -> f64 + Send + 'a) {
        self.brightness = Box::new(f);
    }

    /// Values from `r.frame` on (or the next frame, if that has passed).
    pub fn request(&mut self, r: &SensorRequest) {
        self.pending.insert(r.frame.max(self.next), *r);
    }

    /// The layout of the frames produced.
    pub fn format(&self) -> RawFormat {
        let f = self.rec.header.format;
        RawFormat::new(
            f.width,
            f.height,
            f.pattern,
            RawPacking::U16Le {
                bits: f.packing.bit_depth(),
            },
        )
    }

    /// Bytes per row of the frames produced.
    pub fn stride(&self) -> usize {
        self.rec.header.format.width as usize * 2
    }

    /// The next frame and what produced it.
    pub fn next_frame(&mut self) -> (&[u8], SensorValues) {
        let f = self.next;
        self.next += 1;
        let landed: Vec<u64> = self.pending.range(..=f).map(|(&k, _)| k).collect();
        for k in landed {
            let r = self.pending.remove(&k).expect("listed");
            self.current.exposure = r.exposure;
            self.current.analogue_gain = r.analogue_gain;
            self.current.frame_duration = r.frame_duration;
        }
        self.current.frame = f;
        self.current.verified = true;
        let src_index = (f as usize) % self.rec.len().max(1);
        let src = &self.rec.frames[src_index].sensor;
        let k = exposure_ratio(&self.current, src, (self.brightness)(f));
        let h = &self.rec.header;
        let rec = Recorded {
            packing: h.format.packing,
            stride: h.stride,
            width: h.format.width as usize,
            height: h.format.height as usize,
            black_level: h.sensor.black_level,
        };
        self.out.resize(rec.width * 2 * rec.height, 0);
        re_expose(
            self.rec.frame(src_index),
            &rec,
            k,
            &mut self.out,
            &mut self.row,
        );
        (&self.out, self.current)
    }
}

#[cfg(test)]
mod tests {
    use styx_softisp::CfaPattern;

    use super::*;
    use crate::rawrec::{FrameRecord, Header, RawRecording, VERSION};
    use crate::sensor::{SensorInfo, ov9782};

    fn flat_recording(level: u16) -> RawRecording {
        let info = SensorInfo::from_description(&ov9782(), "640x400", "raw10").unwrap();
        let format = RawFormat::new(4, 2, CfaPattern::Bggr, RawPacking::U16Le { bits: 10 });
        let mut data = Vec::new();
        for i in 0..8u16 {
            data.extend_from_slice(&if i == 7 { 1023 } else { level }.to_le_bytes());
        }
        let sensor = SensorValues {
            frame: 0,
            exposure: Duration::from_millis(10),
            analogue_gain: 1.0,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: true,
        };
        let header = Header {
            styx_raw_recording: VERSION,
            format,
            stride: 8,
            sensor: info,
            notes: String::new(),
        };
        let frames = vec![FrameRecord {
            sensor,
            timestamp_ns: 0,
            offset: 0,
            len: 16,
        }];
        RawRecording::from_parts(header, frames, data).unwrap()
    }

    #[test]
    fn requests_land_on_their_frame_and_re_expose() {
        let rec = flat_recording(64 + 100);
        let mut s = VirtualSensor::new(&rec);
        s.request(&SensorRequest {
            frame: 2,
            exposure: Duration::from_millis(20),
            analogue_gain: 1.5,
            frame_duration: Duration::from_millis(33),
        });
        let sample = |d: &[u8], i: usize| u16::from_le_bytes([d[2 * i], d[2 * i + 1]]);
        for f in 0..3 {
            let (d, v) = s.next_frame();
            let want = if f < 2 { 164 } else { 64 + 300 };
            assert_eq!((v.frame, sample(d, 0)), (f, want));
            // Clipped stays clipped.
            assert_eq!(sample(d, 7), 1023);
        }
        s.set_brightness(|_| 0.5);
        let (d, _) = s.next_frame();
        assert_eq!(sample(d, 0), 64 + 150);
        assert_eq!(s.stride(), 8);
    }
}
