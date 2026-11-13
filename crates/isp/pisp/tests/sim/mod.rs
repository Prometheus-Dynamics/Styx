//! A simulated `camera.isp/1` + `isp.pisp/1` back end on `lemnos-sim`, for the live job path.
//!
//! Copied in shape from Lemnos's simulated camera (`crates/core/lemnos/tests/sim/mod.rs`, a test
//! module there) and extended to the PiSP back end's streams: `config` and `input` are consumer
//! filled, `output0` and `output1` are filled here. Each tick the simulated ISP takes every buffer
//! the consumer has returned, reads each posted job with the contract's own codec
//! ([`IspJob::decode`]), reads the job's configuration and its input once into its own memory,
//! checks them against what the test expects for the job's identity, and publishes a pattern in
//! each output the job names. Nothing about the pixels is modelled: the outputs carry the job's
//! identity as their fill byte, so a test can tell which job made each frame.
//!
//! The gate (`set_gate`) holds jobs in the ring while the test checks what a full set of config
//! buffers does; the log records every job the simulated ISP ran, in the order it ran them.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters as IspCounters, IspInfo, IspInput, IspJob, IspOutput,
    NO_BUFFER, OutputLayout,
};
use lemnos_contract_isp_pisp::{IspPisp, LAYOUT_BACK_END, PispInfo};
use lemnos_core::{Config, Decode, Lifecycle, Reader, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;
use styx_format::FourCc;
use styx_lemnos::pisp::format::compute_stride_align;
use styx_lemnos::pisp::uapi::{BeTilesConfig, ImageFormatConfig, Wire};
use styx_pisp::back_end::image_layouts;
use styx_pisp::format::pisp_format;

/// What the test expects of one job, by its identity.
#[derive(Clone, Debug)]
pub struct Expect {
    /// The configuration bytes the job must name (the builder's `BeTilesConfig`, as `Wire`).
    pub config: Vec<u8>,
    /// FNV-1a of the input frame's bytes (its first `input_len` bytes).
    pub input_hash: u64,
    pub input_len: usize,
}

/// One job the simulated ISP ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ran {
    pub identity: u64,
    pub layout: u32,
    pub config_buffer: u32,
    pub input_buffer: u32,
    pub outputs: [u32; 2],
    /// The configuration the job named matched what the test expects.
    pub config_ok: bool,
    /// The input the job named matched what the test expects.
    pub input_ok: bool,
}

static EXPECTS: Mutex<Option<HashMap<u64, Expect>>> = Mutex::new(None);
static RAN: Mutex<Vec<Ran>> = Mutex::new(Vec::new());
static GATE: AtomicBool = AtomicBool::new(false);

/// Expectations for the jobs the test will post, by identity.
pub fn expect(identity: u64, e: Expect) {
    EXPECTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(identity, e);
}

/// The jobs run so far, in order.
pub fn ran() -> Vec<Ran> {
    RAN.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Holds jobs in the ring while `held`: the simulated ISP takes none while it is set.
pub fn set_gate(held: bool) {
    GATE.store(held, Ordering::SeqCst);
}

/// FNV-1a over `bytes`.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The fill byte an output of job `identity` carries for output `k`.
pub fn fill_for(identity: u64, k: usize) -> u8 {
    (identity as u8).wrapping_mul(7).wrapping_add(k as u8 * 101)
}

/// Output `k`'s image as the simulated ISP sizes it (its configuration's strides).
fn output_image(out: &IspOutput) -> Option<ImageFormatConfig> {
    let code = FourCc::from(out.fourcc);
    let format = pisp_format(code)?;
    let mut image = ImageFormatConfig {
        width: u16::try_from(out.width).ok()?,
        height: u16::try_from(out.height).ok()?,
        format,
        stride: 0,
        stride2: 0,
    };
    compute_stride_align(&mut image, 16);
    Some(image)
}

#[derive(Clone, Copy, Debug, Default)]
struct Slots {
    /// Buffers the producer holds (taken with `next_buffer`, not yet published).
    held: u64,
}

impl Slots {
    fn take(&mut self, stream: &mut StreamHandle) -> Result<()> {
        while let Some(b) = stream.next_buffer().map_err(|_| Status::IO)? {
            if b < 64 {
                self.held |= 1 << b;
            }
        }
        Ok(())
    }
    fn has(&self, b: u32) -> bool {
        b < 64 && self.held & (1 << b) != 0
    }
}

#[driver("sim-pisp", provides = [CameraIsp, IspPisp])]
pub struct SimPisp {
    #[needs]
    config: StreamHandle,
    #[needs]
    input: StreamHandle,
    #[needs]
    output0: StreamHandle,
    #[needs]
    output1: StreamHandle,
    input_set: Option<IspInput>,
    outputs: [Option<(IspOutput, u32)>; 2],
    running: bool,
    counters: IspCounters,
    slots: [Slots; 4],
    sequence: u64,
    temporal: Option<bool>,
}

impl SimPisp {
    fn streams(&mut self) -> Result<()> {
        self.slots[0].take(&mut self.config)?;
        self.slots[1].take(&mut self.input)?;
        self.slots[2].take(&mut self.output0)?;
        self.slots[3].take(&mut self.output1)?;
        Ok(())
    }

    /// Runs one job the ring holds: reads it, checks its configuration and input, and publishes
    /// the outputs it names.
    fn run_job(&mut self, bytes: &[u8], now_ns: u64) -> Result<()> {
        let job = match IspJob::decode(&mut Reader::new(bytes)) {
            Ok(job) => job,
            Err(_) => {
                self.counters.dropped += 1;
                return Ok(());
            }
        };
        let names_ok = job.layout == LAYOUT_BACK_END
            && self.slots[0].has(job.config_buffer)
            && self.slots[1].has(job.input_buffer)
            && self.running;
        if !names_ok {
            self.counters.dropped += 1;
            return Ok(());
        }
        let mut cfg = vec![0u8; BeTilesConfig::SIZE];
        let read = self
            .config
            .read(job.config_buffer, 0, &mut cfg)
            .map_err(|_| Status::IO)?;
        let expect = EXPECTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|m| m.get(&job.identity).cloned());
        let config_ok = read && expect.as_ref().is_some_and(|e| e.config == cfg);
        let input_ok = match (&expect, self.input_len()) {
            (Some(e), Some(len)) => {
                let mut frame = vec![0u8; len.max(e.input_len)];
                let read = self
                    .input
                    .read(job.input_buffer, 0, &mut frame)
                    .map_err(|_| Status::IO)?;
                read && fnv1a(&frame[..e.input_len]) == e.input_hash
            }
            _ => false,
        };
        let mut ran = Ran {
            identity: job.identity,
            layout: job.layout,
            config_buffer: job.config_buffer,
            input_buffer: job.input_buffer,
            outputs: [NO_BUFFER, NO_BUFFER],
            config_ok,
            input_ok,
        };
        for k in 0..2 {
            let buffer = job.outputs[k];
            if buffer == NO_BUFFER {
                continue;
            }
            let Some((_, size)) = self.outputs[k] else {
                continue;
            };
            if !self.slots[2 + k].has(buffer) {
                continue;
            }
            let fill = vec![fill_for(job.identity, k); size as usize];
            let stream = if k == 0 {
                &mut self.output0
            } else {
                &mut self.output1
            };
            stream.write(buffer, 0, &fill).map_err(|_| Status::IO)?;
            self.sequence += 1;
            stream
                .publish(StreamFrame {
                    buffer,
                    bytes: size,
                    sequence: self.sequence,
                    identity: job.identity,
                    timestamp_ns: now_ns,
                })
                .map_err(|_| Status::IO)?;
            self.slots[2 + k].held &= !(1 << buffer);
            ran.outputs[k] = buffer;
        }
        RAN.lock().unwrap_or_else(PoisonError::into_inner).push(ran);
        self.counters.jobs += 1;
        // The job's config and input buffers stay the producer's (taken), as the stream has them.
        Ok(())
    }

    /// The bytes of one input frame: its rows at the stride the input's configuration gives.
    fn input_len(&self) -> Option<usize> {
        let input = self.input_set?;
        let mut image = ImageFormatConfig {
            width: u16::try_from(input.width).ok()?,
            height: u16::try_from(input.height).ok()?,
            format: styx_pisp::pisp::format::formats::BAYER16,
            stride: 0,
            stride2: 0,
        };
        compute_stride_align(&mut image, 16);
        Some(image.stride as usize * usize::from(image.height))
    }
}

impl Lifecycle for SimPisp {
    type Needs = SimPispNeeds;
    const TICKS: bool = true;

    fn start(needs: SimPispNeeds, _cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            config: needs.config,
            input: needs.input,
            output0: needs.output0,
            output1: needs.output1,
            input_set: None,
            outputs: [None, None],
            running: false,
            counters: IspCounters::default(),
            slots: [Slots::default(); 4],
            sequence: 0,
            temporal: None,
        })
    }

    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        if self.running {
            let _ = self.streams();
            if !GATE.load(Ordering::SeqCst) {
                let mut encoded = [0u8; 128];
                for _ in 0..4 {
                    let Ok(Some(len)) = self.config.next_job(&mut encoded) else {
                        break;
                    };
                    let bytes = encoded[..len].to_vec();
                    if self.run_job(&bytes, now_ns).is_err() {
                        self.counters.errors += 1;
                    }
                }
            }
        }
        Some(now_ns.saturating_add(500_000))
    }
}

impl CameraIsp for SimPisp {
    fn info(&mut self) -> Result<IspInfo> {
        Ok(IspInfo {
            max_outputs: 2,
            max_width: 4096,
            max_height: 4096,
            config_layouts: 1,
            statistics_layout: 0,
            statistics_bytes: 0,
        })
    }

    fn config_layout(&mut self, index: u8) -> Result<ConfigLayout> {
        if index != 0 {
            return Err(Status::NOT_FOUND);
        }
        Ok(ConfigLayout {
            layout: LAYOUT_BACK_END,
            bytes: BeTilesConfig::SIZE as u32,
        })
    }

    fn set_input(&mut self, input: IspInput) -> Result<()> {
        if self.running {
            return Err(Status::BUSY);
        }
        self.input_set = Some(input);
        Ok(())
    }

    fn set_output(&mut self, output: u8, image: IspOutput) -> Result<OutputLayout> {
        if output > 1 {
            return Err(Status::NOT_FOUND);
        }
        if self.running {
            return Err(Status::BUSY);
        }
        let config = output_image(&image).ok_or(Status::INVALID_ARGUMENT)?;
        let layout = image_layouts(&config).map_err(|_| Status::INVALID_ARGUMENT)?;
        let size = layout.span().unwrap_or(0) as u32;
        self.outputs[usize::from(output)] = Some((image, size));
        Ok(OutputLayout {
            stride: config.stride as u32,
            size,
        })
    }

    fn clear_output(&mut self, output: u8) -> Result<()> {
        if output > 1 {
            return Err(Status::NOT_FOUND);
        }
        self.outputs[usize::from(output)] = None;
        Ok(())
    }

    fn config_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        name(b"config", into)
    }

    fn statistics_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn input_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        name(b"input", into)
    }

    fn output_stream(&mut self, output: u8, into: &mut [u8]) -> Result<usize> {
        match output {
            0 => name(b"output0", into),
            1 => name(b"output1", into),
            _ => Err(Status::NOT_FOUND),
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.input_set.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }

    fn counters(&mut self) -> Result<IspCounters> {
        Ok(self.counters)
    }
}

impl IspPisp for SimPisp {
    fn info(&mut self) -> Result<PispInfo> {
        Ok(PispInfo {
            hw_version: 0,
            queue_depth: 4,
            front_end_config_bytes: 0,
            back_end_config_bytes: BeTilesConfig::SIZE as u32,
            statistics_bytes: 0,
            stalled: 0,
        })
    }

    fn set_front_end_compression(&mut self, _output: u8, _compressed: bool) -> Result<()> {
        Err(Status::NOT_SUPPORTED)
    }

    fn enable_temporal_denoise(&mut self, compressed: bool) -> Result<()> {
        if self.running {
            return Err(Status::BUSY);
        }
        self.temporal = Some(compressed);
        Ok(())
    }

    fn reset_temporal_denoise(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_line_event(&mut self, _line: u32) -> Result<()> {
        Err(Status::NOT_SUPPORTED)
    }
}

fn name(stream: &[u8], into: &mut [u8]) -> Result<usize> {
    into.get_mut(..stream.len())
        .ok_or(Status::BUFFER_TOO_SMALL)?
        .copy_from_slice(stream);
    Ok(stream.len())
}

/// The board: one PiSP back end with its four streams.
pub fn board() -> String {
    "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-pisp\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-pisp\"\n\
     [devices.streams.config]\nbuffers = 4\nbuffer_bytes = 32768\njob_slots = 4\njob_bytes = 128\n\
     [devices.streams.input]\nbuffers = 3\nbuffer_bytes = 2048000\n\
     [devices.streams.output0]\nbuffers = 3\nbuffer_bytes = 1600000\n\
     [devices.streams.output1]\nbuffers = 3\nbuffer_bytes = 800000\n"
        .to_string()
}
