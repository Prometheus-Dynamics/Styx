//! A simulated `image.scaler/1` driver on `lemnos-sim`, for the tests of the hardware converter.
//!
//! It serves the contract the way the Lemnos RGA driver does, on its job stream: it takes the
//! buffers the consumer returns, reads each job's input image from the buffer the job names,
//! scales it, and publishes the output as the job's completion on the output buffer. It computes
//! with the scalar path of `styx-scale`, so a test that compares the hardware converter with the
//! software path checks the protocol, the layout and the copies, not a second implementation of
//! the arithmetic.

use lemnos_contract_image_scaler::{
    Counters, Filter as JobFilter, ImageScaler, JOB_BYTES, MAX_STREAM_NAME, ScaleJob, ScalerInfo,
};
use lemnos_core::{Config, Decode, Lifecycle, Reader, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;
use styx_format::{FourCc, plane_geometry};
use styx_scale::{Filter, Path, layout, resample};

/// The formats the simulated scaler takes. R8 is left out, so a test can see a refused format.
pub const FORMATS: [FourCc; 7] = [
    FourCc::GREY,
    FourCc::RG24,
    FourCc::BG24,
    FourCc::BGRA,
    FourCc::NV12,
    FourCc::NV21,
    FourCc::YU12,
];

/// The largest width and height a job may name.
pub const MAX_SIDE: u32 = 64;

/// How often the job stream is looked at, in nanoseconds.
const POLL_NS: u64 = 1_000_000;

/// Jobs taken from the stream in one tick.
const JOBS_PER_TICK: usize = 8;

/// Samples per pixel of plane `index` of `code`: the chroma pair of NV12 and NV21 is two.
fn channels(code: FourCc, index: usize) -> usize {
    match code {
        FourCc::RG24 | FourCc::BG24 => 3,
        FourCc::BGRA => 4,
        FourCc::NV12 | FourCc::NV21 if index > 0 => 2,
        _ => 1,
    }
}

#[driver("sim-scaler", provides = [ImageScaler])]
pub struct SimScaler {
    #[needs(job)]
    scale: StreamHandle,
    /// The buffers this driver holds: taken from the consumer, not yet published.
    held: u64,
    sequence: u64,
    counters: Counters,
}

impl Lifecycle for SimScaler {
    type Needs = SimScalerNeeds;
    const TICKS: bool = true;

    fn start(needs: SimScalerNeeds, _cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            scale: needs.scale,
            held: 0,
            sequence: 0,
            counters: Counters::default(),
        })
    }

    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        self.serve(now_ns);
        Some(now_ns.saturating_add(POLL_NS))
    }
}

impl ImageScaler for SimScaler {
    fn info(&mut self) -> Result<ScalerInfo> {
        Ok(ScalerInfo {
            max_width: MAX_SIDE,
            max_height: MAX_SIDE,
        })
    }

    fn format_count(&mut self) -> Result<u16> {
        Ok(FORMATS.len() as u16)
    }

    fn format(&mut self, index: u16) -> Result<u32> {
        FORMATS
            .get(usize::from(index))
            .map(|code| code.to_u32())
            .ok_or(Status::NOT_FOUND)
    }

    fn job_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = b"scale";
        debug_assert!(name.len() <= MAX_STREAM_NAME);
        into.get_mut(..name.len())
            .ok_or(Status::BUFFER_TOO_SMALL)?
            .copy_from_slice(name);
        Ok(name.len())
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }
}

impl SimScaler {
    fn holds(&self, buffer: u32) -> bool {
        buffer < 64 && self.held & (1 << buffer) != 0
    }

    /// Takes every buffer the consumer has returned.
    fn take_free(&mut self) {
        for _ in 0..self.scale.buffer_count() {
            match self.scale.next_buffer() {
                Ok(Some(buffer)) if buffer < 64 => self.held |= 1 << buffer,
                _ => break,
            }
        }
    }

    /// Publishes `buffer` empty, which hands it back to the consumer and says nothing was made.
    fn give_up(&mut self, buffer: u32, now_ns: u64) {
        let frame = StreamFrame {
            buffer,
            bytes: 0,
            sequence: self.sequence,
            identity: 0,
            timestamp_ns: now_ns,
        };
        if self.scale.publish(frame).is_ok() {
            self.held &= !(1 << buffer);
            self.sequence = self.sequence.wrapping_add(1);
        }
    }

    /// Buffers taken before the consumer now attached have nothing behind them, and a publish of
    /// one reaches no one, without a sign. So when the first held buffer is stale, everything held
    /// is given up and taken again.
    fn refresh(&mut self, now_ns: u64) {
        let first = (0..64).find(|&b| self.holds(b));
        let stale = first.is_some_and(|b| !matches!(self.scale.buffer_current(b), Ok(true)));
        if stale {
            for buffer in 0..64 {
                if self.holds(buffer) {
                    self.give_up(buffer, now_ns);
                }
            }
        }
        self.take_free();
    }

    fn serve(&mut self, now_ns: u64) {
        self.refresh(now_ns);
        let mut encoded = [0u8; JOB_BYTES];
        for _ in 0..JOBS_PER_TICK {
            let Ok(Some(len)) = self.scale.next_job(&mut encoded) else {
                break;
            };
            let mut reader = Reader::new(&encoded[..len]);
            let job = match ScaleJob::decode(&mut reader) {
                Ok(job) if reader.remaining() == 0 => job,
                _ => {
                    // A broken consumer: its stream is faulted, and what was held comes back empty.
                    self.counters.errors += 1;
                    let _ = self.scale.fault_consumer();
                    for buffer in 0..64 {
                        if self.holds(buffer) {
                            self.give_up(buffer, now_ns);
                        }
                    }
                    break;
                }
            };
            self.refresh(now_ns);
            let made = self.run(&job);
            let output = job.output.buffer;
            if self.holds(output) {
                let frame = StreamFrame {
                    buffer: output,
                    bytes: made.unwrap_or(0),
                    sequence: self.sequence,
                    identity: job.identity,
                    timestamp_ns: now_ns,
                };
                if self.scale.publish(frame).is_ok() {
                    self.held &= !(1 << output);
                    self.sequence = self.sequence.wrapping_add(1);
                }
            }
            match made {
                Some(_) => self.counters.jobs += 1,
                None => self.counters.errors += 1,
            }
        }
    }

    /// Scales the job's input image into its output buffer. The bytes written, or `None` for a job
    /// the scaler refuses.
    fn run(&mut self, job: &ScaleJob) -> Option<u32> {
        let code = FourCc::new(job.input.fourcc.to_le_bytes());
        if !FORMATS.contains(&code) || job.output.fourcc != job.input.fourcc {
            return None;
        }
        let (iw, ih) = (job.input.width as usize, job.input.height as usize);
        let (ow, oh) = (job.output.width as usize, job.output.height as usize);
        if iw == 0
            || ih == 0
            || ow == 0
            || oh == 0
            || iw > MAX_SIDE as usize
            || ih > MAX_SIDE as usize
            || ow > MAX_SIDE as usize
            || oh > MAX_SIDE as usize
        {
            return None;
        }
        if !layout::fits(code, iw, ih) || !layout::fits(code, ow, oh) {
            return None;
        }
        if job.input.buffer == job.output.buffer
            || !self.holds(job.input.buffer)
            || !self.holds(job.output.buffer)
        {
            return None;
        }
        let input_geometry = plane_geometry(code, iw as u32, ih as u32).ok()?;
        let output_geometry = plane_geometry(code, ow as u32, oh as u32).ok()?;
        if job.input.stride as usize != input_geometry[0].row_bytes {
            return None;
        }
        let in_len: usize = input_geometry.iter().map(|g| g.row_bytes * g.rows).sum();
        let out_len: usize = output_geometry.iter().map(|g| g.row_bytes * g.rows).sum();
        let mut input = vec![0u8; in_len];
        if !matches!(self.scale.read(job.input.buffer, 0, &mut input), Ok(true)) {
            return None;
        }
        let filter = match job.filter {
            JobFilter::Area => Filter::Area,
            JobFilter::Bilinear => Filter::Bilinear,
        };
        let mut output = vec![0u8; out_len];
        let (mut at_in, mut at_out) = (0usize, 0usize);
        for (index, (gi, go)) in input_geometry
            .iter()
            .zip(output_geometry.iter())
            .enumerate()
        {
            let ch = channels(code, index);
            let src_size = (gi.row_bytes / ch, gi.rows);
            let dst_size = (go.row_bytes / ch, go.rows);
            let dst_len = go.row_bytes * go.rows;
            resample(
                Path::Scalar,
                filter,
                &input[at_in..],
                gi.row_bytes,
                src_size,
                ch,
                &mut output[at_out..at_out + dst_len],
                dst_size,
            );
            at_in += gi.row_bytes * gi.rows;
            at_out += dst_len;
        }
        if !matches!(self.scale.write(job.output.buffer, 0, &output), Ok(true)) {
            return None;
        }
        u32::try_from(out_len).ok()
    }
}

/// A board with one simulated image scaler, its job stream of four buffers.
pub fn board() -> String {
    "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-scaler\"\n\
     [[devices]]\nid = \"scaler0\"\ndriver = \"sim-scaler\"\n\
     [devices.streams.scale]\nbuffers = 4\nbuffer_bytes = 16384\n"
        .to_string()
}
