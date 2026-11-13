//! A simulated USB camera: a `camera.frames/1` driver on Lemnos's simulated backend.
//!
//! It offers four formats, one of them compressed, and while started publishes one frame per
//! interval of its format on the stream the board gives it. Frames carry sequence, identity,
//! timestamp and size, not pixels (on hardware the device's transfers fill the buffers). Its
//! counters and state are not kept across a restart, as with a replugged camera.

use lemnos_contract_camera_frames::{CameraFrames, Counters, FrameFormat, MAX_STREAM_NAME};
use lemnos_core::{Config, Lifecycle, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;

/// The formats the simulated camera offers, in its own order.
pub const FORMATS: [FrameFormat; 4] = [
    FrameFormat {
        fourcc: u32::from_le_bytes(*b"MJPG"),
        width: 1280,
        height: 720,
        fps_milli: 30_000,
        max_frame_bytes: 300_000,
    },
    FrameFormat {
        fourcc: u32::from_le_bytes(*b"YUYV"),
        width: 320,
        height: 240,
        fps_milli: 30_000,
        max_frame_bytes: 153_600,
    },
    FrameFormat {
        fourcc: u32::from_le_bytes(*b"YUYV"),
        width: 640,
        height: 480,
        fps_milli: 15_000,
        max_frame_bytes: 614_400,
    },
    FrameFormat {
        fourcc: u32::from_le_bytes(*b"YUYV"),
        width: 640,
        height: 480,
        fps_milli: 30_000,
        max_frame_bytes: 614_400,
    },
];

/// The bytes one frame of format `index` carries: YUYV is two bytes a pixel.
fn frame_bytes(index: u16) -> Option<u32> {
    let format = FORMATS.get(usize::from(index))?;
    if format.fourcc == u32::from_le_bytes(*b"YUYV") {
        Some(format.width * format.height * 2)
    } else {
        Some(format.max_frame_bytes)
    }
}

/// The frame stream's producer: one frame per interval, while started.
#[derive(Clone, Copy)]
struct Run {
    interval_ns: u64,
    next_ns: Option<u64>,
    sequence: u64,
    bytes: u32,
}

#[driver("sim-uvc", provides = [CameraFrames])]
pub struct SimUvc {
    #[needs(free)]
    frames: StreamHandle,
    selected: Option<u16>,
    run: Option<Run>,
    counters: Counters,
}

impl Lifecycle for SimUvc {
    type Needs = SimUvcNeeds;
    const TICKS: bool = true;

    fn start(needs: SimUvcNeeds, _cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            frames: needs.frames,
            selected: None,
            run: None,
            counters: Counters::default(),
        })
    }

    /// Makes every frame due by `now_ns`, and returns when the next one is due.
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        let run = self.run.as_mut()?;
        let mut next = match run.next_ns {
            Some(next) => next,
            None => {
                let next = now_ns.saturating_add(run.interval_ns);
                run.next_ns = Some(next);
                return Some(next);
            }
        };
        while next <= now_ns {
            match self.frames.next_buffer() {
                Ok(Some(buffer)) => {
                    let frame = StreamFrame {
                        buffer,
                        bytes: run.bytes,
                        sequence: run.sequence,
                        identity: run.sequence,
                        timestamp_ns: next,
                    };
                    match self.frames.publish(frame) {
                        Ok(()) => self.counters.frames += 1,
                        Err(_) => self.counters.errors += 1,
                    }
                }
                Ok(None) => self.counters.dropped += 1,
                Err(_) => self.counters.errors += 1,
            }
            run.sequence += 1;
            next = next.saturating_add(run.interval_ns);
        }
        run.next_ns = Some(next);
        Some(next)
    }
}

impl CameraFrames for SimUvc {
    fn format_count(&mut self) -> Result<u16> {
        Ok(FORMATS.len() as u16)
    }

    fn format(&mut self, index: u16) -> Result<FrameFormat> {
        FORMATS
            .get(usize::from(index))
            .copied()
            .ok_or(Status::NOT_FOUND)
    }

    fn select_format(&mut self, index: u16) -> Result<()> {
        if self.run.is_some() {
            return Err(Status::BUSY);
        }
        if usize::from(index) >= FORMATS.len() {
            return Err(Status::NOT_FOUND);
        }
        self.selected = Some(index);
        Ok(())
    }

    fn selected(&mut self) -> Result<Option<u16>> {
        Ok(self.selected)
    }

    fn frame_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = b"frames";
        debug_assert!(name.len() <= MAX_STREAM_NAME);
        into.get_mut(..name.len())
            .ok_or(Status::INVALID_ARGUMENT)?
            .copy_from_slice(name);
        Ok(name.len())
    }

    fn start(&mut self) -> Result<()> {
        let index = self.selected.ok_or(Status::INVALID_ARGUMENT)?;
        if self.run.is_none() {
            let format = FORMATS[usize::from(index)];
            let bytes = frame_bytes(index).ok_or(Status::INVALID_ARGUMENT)?;
            self.counters = Counters::default();
            self.run = Some(Run {
                interval_ns: 1_000_000_000_000 / u64::from(format.fps_milli.max(1)),
                next_ns: None,
                sequence: 0,
                bytes,
            });
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.run = None;
        Ok(())
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }
}

/// A board with one simulated camera, `usb0`, and each name in `extra` as one more.
pub fn board(names: &[&str]) -> String {
    let mut text =
        String::from("format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-usb\"\n");
    for name in names {
        text.push_str(&format!(
            "[[devices]]\nid = \"{name}\"\ndriver = \"sim-uvc\"\n\
             [devices.streams.frames]\nbuffers = 4\nbuffer_bytes = 614400\n"
        ));
    }
    text
}
