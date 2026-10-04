//! A firmware-like camera without `std`: Styx's portable runtime, pipeline core, software ISP,
//! algorithms and sensor driver built together as one `no_std` crate, on a mock platform.
//!
//! * [`firmware`]: the camera a microcontroller runs, over any `styx_runtime::Platform` whose
//!   sensor side is a `SensorState`: `Camera` start/stop, the frame-exact control schedule
//!   served at frame starts, the software ISP loop with AE/AWB ([`styx_pipeline::SoftLoop`]),
//!   still and bracket decisions ([`styx_pipeline::still_runner`]) with full-quality
//!   reprocessing, metrics counters ([`styx_runtime::metrics`]).
//! * [`board`]: the mock platform, the part a real port replaces: a replay receiver that
//!   "captures" a raw recording re-exposed with what the sensor's registers say, the OV9782 as
//!   a register model, pins, the clock.
//! * [`run`]: the superloop over a synthetic recording ([`recording`]): frames processed, 3A
//!   converging, a bracket taken once AE has locked, the counters counted.
//!
//! `scripts/check-nostd.sh` builds it for `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`,
//! `riscv32imac-unknown-none-elf` and `wasm32-unknown-unknown`, runs [`run`] as a host test
//! with every dependency built without `std` (`cargo test -p styx-nostd-camera`), runs it
//! again over the `std` builds (`--features std`: the Linux software path's arithmetic) and
//! compares the two traces bit for bit. An application on a microcontroller provides a global
//! allocator, a panic handler and its own `board`.

#![no_std]

extern crate alloc;

pub mod board;
pub mod firmware;

use alloc::vec::Vec;
use core::time::Duration;

use styx_algo::Tuning;
use styx_pipeline::SensorInfo;
use styx_pipeline::still_runner::ShotExposure;
use styx_runtime::metrics::Counters;
use styx_softisp::RawPacking;

use board::{Board, MockBoard, Recording, receiver_config};
use firmware::{FrameReport, Shot};

/// Width of the replayed frames.
pub const WIDTH: u32 = 256;
/// Height of the replayed frames.
pub const HEIGHT: u32 = 192;

/// The OV9782's 1280x800 RAW10 mode at 30 fps, as the algorithms see it, at the replayed size.
pub fn sensor_info() -> SensorInfo {
    let desc = board::description();
    let mut info = SensorInfo::from_description(&desc, "1280x800", "raw10")
        .expect("mode")
        .with_fps(30.0, 30.0)
        .expect("rate");
    info.width = WIDTH;
    info.height = HEIGHT;
    info
}

/// A raw recording of a BGGR scene under a warm light (R 1.0, G 0.8, B 0.45 of a grey's
/// response) with a brightness ramp and a little fixed-pattern texture, four frames recorded
/// at 10 ms and gain 1 well below saturation, 16-bit samples of a 10-bit sensor.
pub fn recording(info: &SensorInfo) -> Recording {
    let (w, h) = (info.width as usize, info.height as usize);
    let black = (info.black_level * 1024.0) as u32;
    let mut data = Vec::with_capacity(w * h * 2 * 4);
    let mut seed = 0x2545_f491u32;
    for frame in 0..4u32 {
        for y in 0..h {
            for x in 0..w {
                let level = 40 + 120 * x as u32 / w as u32 + frame;
                let k = match (y & 1, x & 1) {
                    (0, 0) => 45,
                    (1, 1) => 100,
                    _ => 80,
                };
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let v = (black + level * k / 100 + (seed & 3)).min(1023) as u16;
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let values = styx_pipeline::SensorValues {
        frame: 0,
        exposure: Duration::from_millis(10),
        analogue_gain: 1.0,
        digital_gain: 1.0,
        frame_duration: Duration::from_nanos(33_333_333),
        verified: true,
    };
    Recording {
        layout: styx_pipeline::reexpose::Recorded {
            packing: RawPacking::U16Le { bits: 10 },
            stride: w * 2,
            width: w,
            height: h,
            black_level: info.black_level,
        },
        data,
        values: (0..4)
            .map(|f| styx_pipeline::SensorValues { frame: f, ..values })
            .collect(),
    }
}

/// What [`run`] ends with.
#[derive(Debug, Default)]
pub struct Run {
    /// Every processed frame.
    pub frames: Vec<FrameReport>,
    /// The bracket's shots.
    pub shots: Vec<Shot>,
    /// The frame AE first locked on.
    pub ae_locked_at: Option<u64>,
    /// The camera's counters.
    pub counters: Counters,
    /// Register writes the sensor got.
    pub register_writes: u64,
}

/// The superloop: `frames` frames of [`recording`] through the camera, a bracket of -1, 0 and
/// +1 EV requested the frame after AE first locks.
pub fn run(frames: u64) -> Run {
    let info = sensor_info();
    let rec = recording(&info);
    let config = receiver_config(&rec, 4);
    let mut board = Board::new(rec, 4, Duration::from_nanos(33_333_333));
    let mut fw = firmware::Firmware::<MockBoard, _, _>::new(
        board.receiver.clone(),
        board.sensor.clone(),
        info,
        RawPacking::U16Le { bits: 10 },
        &Tuning::default(),
        board::clock,
    )
    .expect("firmware");
    // The reference arithmetic: the same pictures with and without std (`Auto` picks by the
    // CPU features found at run time, which only std builds can look for).
    fw.set_arithmetic(styx_softisp::Arithmetic::Int);
    fw.start(&config).expect("start");
    let mut out = Run::default();
    for _ in 0..frames {
        // The frame-start interrupt, then the frame's end.
        board.frame_start();
        fw.service().expect("frame start served");
        board.frame_end();
        while let Some(f) = fw.poll().expect("frame") {
            if f.ae_locked && out.ae_locked_at.is_none() {
                out.ae_locked_at = Some(f.values.frame);
                fw.request_still(ShotExposure::Bracket(alloc::vec![-1.0, 0.0, 1.0]), false);
            }
            out.frames.push(f);
        }
    }
    fw.shut_down().expect("shut down");
    out.shots = core::mem::take(&mut fw.shots);
    out.counters = core::mem::take(&mut fw.counters);
    out.register_writes = board.register_writes();
    out
}

#[cfg(test)]
mod tests;
