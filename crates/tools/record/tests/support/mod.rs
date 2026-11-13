#![allow(dead_code)]

//! Helpers shared by the recording tests: a deterministic generator, CRC-32, and a set of frames
//! in several formats (packed, padded, multi-plane and compressed).

use styx_controls::{Control, ControlValue, FrameMetadata, Rect};
use styx_format::{FourCc, Interval, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, packed_layouts};

/// xorshift64*: the same sequence on every run.
pub struct Prng(u64);

impl Prng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }
}

/// CRC-32 (IEEE), bit by bit: the tests compute their own checksums to craft records.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// One frame to record: its entry, metadata and the bytes of its span.
#[derive(Clone)]
pub struct Sample {
    pub entry: FrameEntry,
    pub meta: FrameMetadata,
    pub data: Vec<u8>,
}

/// The largest span over `samples`: the payload a recording of them needs.
pub fn payload_of(samples: &[Sample]) -> u64 {
    samples
        .iter()
        .map(|s| s.entry.layouts.span().unwrap() as u64)
        .max()
        .unwrap()
}

fn metadata(i: usize) -> FrameMetadata {
    let mut controls = [ControlValue::Auto; Control::COUNT];
    controls[Control::Exposure.index()] = ControlValue::Manual(12_000 + i as u32);
    controls[Control::Gain.index()] = ControlValue::Manual(2_000);
    controls[Control::Focus.index()] = ControlValue::Manual(500);
    controls[Control::FrameRate.index()] = ControlValue::Manual(30_000);
    controls[Control::HFlip.index()] = ControlValue::Flag(true);
    controls[Control::VFlip.index()] = ControlValue::Flag(i.is_multiple_of(2));
    controls[Control::Crop.index()] = ControlValue::Crop(Some(Rect {
        x: 8,
        y: 16,
        width: 640,
        height: 480,
    }));
    FrameMetadata {
        controls,
        sensor_timestamp_ns: 1_000_000_000 + i as u64 * 33_333_333,
        exposure_us: 12_000 + i as u32,
        analogue_gain_milli: 1_500,
        digital_gain_milli: 1_000,
        lux: if i.is_multiple_of(2) {
            Some(123.5)
        } else {
            None
        },
        colour_temperature_k: Some(5_600),
    }
}

/// Frames in four formats: grey with padded rows, NV12 with two planes, YUYV packed, and an MJPG
/// bitstream. Ids are not sequential, timestamps increase.
pub fn samples(seed: u64) -> Vec<Sample> {
    let mut prng = Prng::new(seed);
    let mut out = Vec::new();
    let packed = |code: FourCc, w: u32, h: u32, stride: Option<usize>, plane: Option<usize>| {
        let layouts = packed_layouts(code, w, h, stride, plane).unwrap();
        let format = MediaFormat::with_defaults(code, Resolution::new(w, h).unwrap());
        (format, layouts)
    };
    let mjpg = FourCc::new(*b"MJPG");
    let shapes: Vec<(MediaFormat, Layouts)> = vec![
        packed(FourCc::GREY, 64, 32, Some(128), None),
        packed(FourCc::NV12, 128, 64, Some(64), Some(64)),
        packed(FourCc::YUYV, 96, 40, None, None),
        (
            MediaFormat::with_defaults(mjpg, Resolution::new(640, 480).unwrap()),
            Layouts::new(&[PlaneLayout::new(0, 0, 300)]).unwrap(),
        ),
    ];
    for (i, (format, layouts)) in shapes.into_iter().enumerate() {
        let span = layouts.span().unwrap();
        let data = prng.bytes(span);
        out.push(Sample {
            entry: FrameEntry::new(
                1_000 + 7 * i as u64,
                1_000_000_000 + i as u64 * 33_333_333,
                format,
                layouts,
            ),
            meta: metadata(i),
            data,
        });
    }
    out
}

/// A raw format's interval, for headers.
pub fn fps30() -> Option<Interval> {
    Interval::from_fps(30)
}
