//! The live job path: one frame's back-end job posted on `camera.isp/1` and completed on its
//! output streams, in the sequence the library's README gives for the reference board
//! (`drivers/pisp/README.md`, "Running a job live").
//!
//! [`LiveIsp::open`] does the setup: the ISP's input and outputs are described
//! (`set_input`, `set_output` for each output), its streams are opened by the names the ISP
//! gives (`config`, `input`, `output0`, `output1`; writable, so their buffers can be written),
//! temporal denoise's buffers are enabled when the geometry has them, and the ISP starts.
//!
//! [`LiveIsp::post`] runs one frame. The configuration is built by [`BackEndBuilder`] and written
//! into one of the two config buffers, the raw frame is copied into a free input buffer (the one
//! copy the README's sequence needs), and the job is posted naming them and the two output
//! buffers. Two config buffers alternate: a frame posted while both are in flight is refused with
//! [`PispError::Busy`] and nothing is changed. [`LiveIsp::wait`] returns the frames whose jobs
//! have completed on both outputs, as [`FrameLease`]s over the output buffers: nothing is copied
//! on the way out. A buffer a lease holds is never named by a job.
//!
//! Copies: the input write is a [`FrameLease::copy_into`], counted by `styx_frame::COPY_INTO`
//! and by [`LiveIsp::input_copies`]. The output frames and the config buffer are not copied: the
//! config is encoded straight into its buffer.
//!
//! Untested on board (H1). The sequence is the README's; the simulated backend tests check it
//! against a simulated ISP that parses the job with the contract's codec (`tests/live_sim.rs`).

use std::time::{Duration, Instant};

use lemnos_contract_camera_isp::{CameraIsp, IspInput, IspOutput, MAX_STREAM_NAME, OutputLayout};
use lemnos_contract_camera_receiver::BayerOrder as ContractBayer;
use lemnos_contract_isp_pisp::{IspPisp, LAYOUT_BACK_END, OUTPUT_BACK_END_0, OUTPUT_BACK_END_1};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};
use styx_lemnos::pisp::uapi::{BayerOrder, BeTilesConfig, ImageFormatConfig, Wire};
use styx_lemnos::{Client, DeviceEntry, HeldFrame, JobStream, open_job_stream};
use styx_settings::IspSettings;

use crate::PispError;
use crate::back_end::{BackEndBuilder, Geometry, image_layouts};
use crate::job::{JOB_CAPACITY, back_end_job, encode_job};

/// The ISP's output numbers for the two back-end outputs (`camera.isp/1`).
const OUTPUTS: [u8; 2] = [OUTPUT_BACK_END_0, OUTPUT_BACK_END_1];

/// Config buffers that alternate: a job's configuration is not written again until its job has
/// completed.
const CONFIG_BUFFERS: usize = 2;

/// How long one poll of an output stream waits before the other is looked at.
const POLL: Duration = Duration::from_millis(2);

/// A completed frame: the job's identity, the time the frame was made and its two outputs.
#[derive(Debug)]
pub struct LiveJob {
    pub identity: u64,
    pub timestamp_ns: u64,
    /// Output 0 (full size) and output 1, as leases over their buffers. Dropping a lease gives
    /// its buffer back for a later job.
    pub outputs: [FrameLease<()>; 2],
}

/// A job posted and not yet complete.
#[derive(Debug)]
struct Flight {
    identity: u64,
    config: usize,
    input: usize,
    outputs: [usize; 2],
    done: [Option<FrameLease<()>>; 2],
    timestamp_ns: u64,
}

/// The live back end: see the module documentation.
pub struct LiveIsp {
    isp: lemnos_core::Bound<dyn CameraIsp, lemnos_client::RemoteCaller>,
    config: JobStream<()>,
    input: JobStream<()>,
    outputs: [JobStream<()>; 2],
    input_layouts: Layouts,
    output_media: [MediaFormat; 2],
    output_layouts: [Layouts; 2],
    builder: BackEndBuilder,
    flights: Vec<Flight>,
    input_busy: Vec<bool>,
    output_busy: [Vec<bool>; 2],
    timeout: Duration,
    input_copies: usize,
    /// Frames this session did not post because a buffer Styx picks was not free (the config, the
    /// input or an output). Nothing was sent, so the ISP never saw them (see [`LiveIsp::post`]).
    starved: u64,
    /// Frames Styx posted that the job stream refused: its job ring was full, so the post returned
    /// `false` and nothing was queued. Not a Lemnos counter: the Lemnos `refused` stat counts only
    /// jobs that starved for a buffer the driver picks, and this path picks its own.
    posts_refused: u64,
}

impl std::fmt::Debug for LiveIsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveIsp")
            .field("config", &self.config.name())
            .field("input", &self.input.name())
            .field("in_flight", &self.flights.len())
            .field("input_copies", &self.input_copies)
            .finish()
    }
}

impl LiveIsp {
    /// Sets up `device` (a `camera.isp/1` device with the PiSP back end) for `geometry`, and
    /// starts it. `timeout` bounds a [`Self::wait`] for the frames of posted jobs.
    pub fn open(
        client: &Client,
        device: &DeviceEntry,
        geometry: &Geometry,
        timeout: Duration,
    ) -> Result<Self, PispError> {
        let builder = BackEndBuilder::new(geometry)?;
        let mut isp = client
            .open::<dyn CameraIsp>(&device.name)
            .map_err(|e| setup(format!("{}: {e:?}", device.name)))?;
        let input = geometry.input;
        isp.set_input(IspInput {
            width: u32::from(input.width),
            height: u32::from(input.height),
            bit_depth: 16,
            bayer: contract_bayer(geometry.bayer)?,
        })
        .map_err(|e| setup(format!("set_input: {e:?}")))?;
        let mut layouts = [None, None];
        for (k, layout) in layouts.iter_mut().enumerate() {
            let image = builder.image(k);
            let fourcc = geometry.outputs[k].media.code;
            let out: OutputLayout = isp
                .set_output(
                    OUTPUTS[k],
                    IspOutput {
                        width: u32::from(image.width),
                        height: u32::from(image.height),
                        fourcc: fourcc.to_u32(),
                        pad_bytes: 0,
                    },
                )
                .map_err(|e| setup(format!("set_output {k}: {e:?}")))?;
            let bytes = image_bytes(&image)?;
            if u64::from(out.size) < bytes {
                return Err(setup(format!(
                    "output {k} needs {bytes} bytes, the ISP gives {}",
                    out.size
                )));
            }
            *layout = Some(builder.layouts(k)?);
        }
        let output_layouts = [layouts[0].expect("set"), layouts[1].expect("set")];

        let config_name = stream_name(|b| isp.config_stream(b))?;
        let input_name = stream_name(|b| isp.input_stream(b))?;
        let out_names = [
            stream_name(|b| isp.output_stream(OUTPUTS[0], b))?,
            stream_name(|b| isp.output_stream(OUTPUTS[1], b))?,
        ];
        let config = open_stream(client, device, &config_name, "config")?;
        if config.buffers() < CONFIG_BUFFERS || config.buffer_bytes() < BeTilesConfig::SIZE {
            return Err(setup(format!(
                "config stream: {} buffers of {} bytes; the job alternates two of {} bytes",
                config.buffers(),
                config.buffer_bytes(),
                BeTilesConfig::SIZE
            )));
        }
        if config.job_bytes() < JOB_CAPACITY {
            return Err(setup(format!(
                "config stream: jobs of {} bytes, a job takes {JOB_CAPACITY}",
                config.job_bytes()
            )));
        }
        let input_bytes = image_bytes(&input)?;
        let input_stream = open_stream(client, device, &input_name, "input")?;
        if input_stream.buffer_bytes() < input_bytes as usize {
            return Err(setup(format!(
                "input stream: buffers of {} bytes, a frame takes {input_bytes}",
                input_stream.buffer_bytes()
            )));
        }
        let [n0, n1] = &out_names;
        let outputs = [
            open_stream(client, device, n0, "output0")?,
            open_stream(client, device, n1, "output1")?,
        ];
        for (k, s) in outputs.iter().enumerate() {
            if s.buffer_bytes() < image_bytes(&builder.image(k))? as usize {
                return Err(setup(format!("output {k} stream: buffers too small")));
            }
        }
        let input_buffers = input_stream.buffers();
        let out_buffers = [outputs[0].buffers(), outputs[1].buffers()];

        if geometry.tdn.is_some() {
            let mut pisp = client
                .open::<dyn IspPisp>(&device.name)
                .map_err(|e| setup(format!("{}: isp.pisp/1: {e:?}", device.name)))?;
            pisp.enable_temporal_denoise(false)
                .map_err(|e| setup(format!("enable_temporal_denoise: {e:?}")))?;
        }
        isp.start().map_err(|e| setup(format!("start: {e:?}")))?;
        let output_media = [geometry.outputs[0].media, geometry.outputs[1].media];
        Ok(Self {
            isp,
            config,
            input: input_stream,
            outputs,
            input_layouts: image_layouts(&input)?,
            output_media,
            output_layouts,
            builder,
            flights: Vec::new(),
            input_busy: vec![false; input_buffers],
            output_busy: [vec![false; out_buffers[0]], vec![false; out_buffers[1]]],
            timeout,
            input_copies: 0,
            starved: 0,
            posts_refused: 0,
        })
    }

    /// Input frames copied into the ISP's input buffers, since the setup.
    pub fn input_copies(&self) -> usize {
        self.input_copies
    }

    /// Frames not posted since the setup because a buffer this session picks (config, input or
    /// output) was not free. Nothing was sent and nothing was dropped from the ISP's output: a job
    /// that is posted always gets its buffers.
    pub fn starved(&self) -> u64 {
        self.starved
    }

    /// Frames Styx posted that the job ring refused (full): the post returned `false`, nothing was
    /// queued. A job the ISP ran is never counted here.
    pub fn posts_refused(&self) -> u64 {
        self.posts_refused
    }

    /// Completions the ISP's job streams were too late for, summed over the config, input and output
    /// streams. A job-driven stream never replaces an unread result, so this stays 0.
    pub fn skipped(&self) -> u64 {
        self.config.skipped()
            + self.input.skipped()
            + self.outputs.iter().map(JobStream::skipped).sum::<u64>()
    }

    /// Jobs posted and not yet complete.
    pub fn in_flight(&self) -> usize {
        self.flights.len()
    }

    /// Posts one frame: `raw` (a 16-bit Bayer frame of the geometry's input size) is copied into
    /// a free input buffer, its configuration built from `settings` (with `exposure` for temporal
    /// denoise) into a free config buffer, and the job naming both and two free output buffers is
    /// posted. A frame that cannot be posted now is [`PispError::Busy`] with nothing changed in
    /// the ISP.
    pub fn post<M>(
        &mut self,
        identity: u64,
        settings: &IspSettings,
        exposure: f64,
        raw: &FrameLease<M>,
    ) -> Result<(), PispError> {
        let starve = |this: &mut Self, why: &'static str| {
            this.starved += 1;
            PispError::Busy(why)
        };
        let Some(config) =
            (0..CONFIG_BUFFERS).find(|c| !self.flights.iter().any(|f| f.config == *c))
        else {
            return Err(starve(self, "both config buffers are in flight"));
        };
        let Some(input) = (0..self.input_busy.len())
            .find(|&i| !self.input_busy[i] && self.input.pool().is_free(i))
        else {
            return Err(starve(self, "no free input buffer"));
        };
        let mut outputs = [0usize; 2];
        for (k, slot) in outputs.iter_mut().enumerate() {
            let free = (0..self.output_busy[k].len())
                .find(|&j| !self.output_busy[k][j] && self.outputs[k].pool().is_free(j));
            let Some(free) = free else {
                return Err(starve(self, "no free output buffer"));
            };
            *slot = free;
        }
        if self.flights.len() >= CONFIG_BUFFERS {
            return Err(starve(self, "both config buffers are in flight"));
        }
        // The configuration, encoded straight into the config buffer.
        self.builder.update(settings, exposure)?;
        self.write_config(config, identity)?;
        // The raw frame, copied into the input buffer: the one copy the sequence needs.
        self.copy_input(input, identity, raw)?;
        let job = back_end_job(
            identity,
            config as u32,
            Some(input as u32),
            [Some(outputs[0] as u32), Some(outputs[1] as u32), None, None],
        );
        debug_assert_eq!(job.layout, LAYOUT_BACK_END);
        let mut bytes = [0u8; JOB_CAPACITY];
        let len = encode_job(&job, &mut bytes)?;
        let posted = self
            .config
            .post(&bytes[..len])
            .map_err(|e| PispError::Sink(format!("{e}")))?;
        if !posted {
            // Nothing was sent: the temporal average starts over with the next frame.
            self.builder.reset_tdn();
            self.posts_refused += 1;
            return Err(PispError::Busy("the job ring is full"));
        }
        self.input_busy[input] = true;
        for (k, &j) in outputs.iter().enumerate() {
            self.output_busy[k][j] = true;
        }
        self.flights.push(Flight {
            identity,
            config,
            input,
            outputs,
            done: [None, None],
            timestamp_ns: raw.timestamp_ns(),
        });
        Ok(())
    }

    /// Waits up to the timeout for a posted frame to complete on both outputs, and returns it.
    ///
    /// `Ok(None)` means nothing is in flight, so there is nothing to wait for (it returns at
    /// once). A job in flight that does not complete within the timeout is
    /// [`PispError::Stalled`], with the flights and the device's counters: a completion that
    /// never comes is never reported as "none yet".
    pub fn wait(&mut self) -> Result<Option<LiveJob>, PispError> {
        if self.flights.is_empty() {
            return Ok(None);
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(done) = self.take_done() {
                return Ok(Some(done));
            }
            if Instant::now() >= deadline {
                return Err(self.stalled());
            }
            if let Some(held) = self.outputs[0]
                .wait(POLL)
                .map_err(|e| PispError::Sink(format!("output 0: {e}")))?
            {
                self.complete(0, held)?;
            }
            if let Some(held) = self.outputs[1]
                .wait(Duration::ZERO)
                .map_err(|e| PispError::Sink(format!("output 1: {e}")))?
            {
                self.complete(1, held)?;
            }
        }
    }

    /// Stops the ISP and gives its streams back.
    pub fn stop(mut self) -> Result<(), PispError> {
        self.isp.stop().map_err(|e| setup(format!("stop: {e:?}")))?;
        self.flights.clear();
        Ok(())
    }

    /// The error for jobs in flight that did not complete in time: each flight's state, the
    /// buffers held, and the device's own counters (`jobs`, `dropped`, `errors`) when it answers.
    fn stalled(&mut self) -> PispError {
        let flights: Vec<String> = self
            .flights
            .iter()
            .map(|f| {
                format!(
                    "job {} (config {}, input {}, outputs {:?}, completed {:?})",
                    f.identity,
                    f.config,
                    f.input,
                    f.outputs,
                    f.done.iter().map(Option::is_some).collect::<Vec<_>>()
                )
            })
            .collect();
        let device = match self.isp.counters() {
            Ok(c) => format!(
                "device jobs {}, dropped {}, errors {}",
                c.jobs, c.dropped, c.errors
            ),
            Err(e) => format!("device counters unavailable: {e:?}"),
        };
        PispError::Stalled(format!(
            "no completion within {:?} for {}; {device}; buffers in use: input {}, outputs {:?}",
            self.timeout,
            flights.join(", "),
            self.input_busy.iter().filter(|b| **b).count(),
            self.output_busy
                .iter()
                .map(|v| v.iter().filter(|b| **b).count())
                .collect::<Vec<_>>(),
        ))
    }

    /// The first flight whose two outputs are both in: its frame, with its buffers freed for
    /// the jobs that follow.
    fn take_done(&mut self) -> Option<LiveJob> {
        let at = self
            .flights
            .iter()
            .position(|f| f.done.iter().all(Option::is_some))?;
        let flight = self.flights.swap_remove(at);
        self.input_busy[flight.input] = false;
        let [d0, d1] = flight.done;
        Some(LiveJob {
            identity: flight.identity,
            timestamp_ns: flight.timestamp_ns,
            outputs: [d0.expect("done"), d1.expect("done")],
        })
    }

    /// Takes a completion from output `k`: its lease over the buffer, then its frame given back to
    /// Lemnos (the buffer is free for jobs once the lease is gone).
    fn complete(&mut self, k: usize, held: HeldFrame) -> Result<(), PispError> {
        let buffer = held.buffer() as usize;
        let identity = held.identity();
        let timestamp_ns = held.timestamp_ns();
        let Some(flight) = self
            .flights
            .iter_mut()
            .find(|f| f.identity == identity && f.outputs[k] == buffer && f.done[k].is_none())
        else {
            self.outputs[k]
                .release(held)
                .map_err(|e| PispError::Sink(format!("{e}")))?;
            return Err(setup(format!(
                "output {k}: a completion for job {identity} on buffer {buffer} that no job names"
            )));
        };
        let entry = FrameEntry::new(
            identity,
            timestamp_ns,
            self.output_media[k],
            self.output_layouts[k],
        );
        let lease = self.outputs[k].pool().acquire_at(buffer, entry);
        self.outputs[k]
            .release(held)
            .map_err(|e| PispError::Sink(format!("{e}")))?;
        let lease = lease.map_err(|e| setup(format!("output {k} buffer {buffer}: {e}")))?;
        flight.done[k] = Some(lease);
        self.output_busy[k][buffer] = false;
        Ok(())
    }

    /// Writes the configuration the builder holds into config buffer `config`.
    fn write_config(&mut self, config: usize, identity: u64) -> Result<(), PispError> {
        let layouts = Layouts::new(&[PlaneLayout::new(
            0,
            BeTilesConfig::SIZE,
            BeTilesConfig::SIZE,
        )])
        .ok_or_else(|| setup("config layout".to_string()))?;
        let format = MediaFormat::with_defaults(
            FourCc::GREY,
            Resolution::new(BeTilesConfig::SIZE as u32, 1).expect("non-empty"),
        );
        let entry = FrameEntry::new(identity, 0, format, layouts);
        let mut lease = self
            .config
            .pool()
            .acquire_at(config, entry)
            .map_err(|e| setup(format!("config buffer {config}: {e}")))?;
        let mut planes = lease
            .write()
            .ok_or_else(|| setup(format!("config buffer {config} is not writable")))?;
        let bytes = planes.bytes();
        let n = self
            .builder
            .config()
            .write_to(bytes)
            .map_err(|e| PispError::Job(format!("config buffer: {e}")))?;
        debug_assert_eq!(n, BeTilesConfig::SIZE);
        Ok(())
    }

    /// Copies `raw` into input buffer `input`.
    fn copy_input<M>(
        &mut self,
        input: usize,
        identity: u64,
        raw: &FrameLease<M>,
    ) -> Result<(), PispError> {
        let entry = FrameEntry::new(
            identity,
            raw.timestamp_ns(),
            *raw.format(),
            self.input_layouts,
        );
        let mut dst = self
            .input
            .pool()
            .acquire_at(input, entry)
            .map_err(|e| setup(format!("input buffer {input}: {e}")))?;
        raw.copy_into(&mut dst)
            .map_err(|e| PispError::Job(format!("input copy: {e:?}")))?;
        self.input_copies += 1;
        Ok(())
    }
}

/// Bytes one image takes in its buffer, from the last plane's end.
fn image_bytes(image: &ImageFormatConfig) -> Result<u64, PispError> {
    let layouts = image_layouts(image)?;
    Ok(layouts.span().unwrap_or(0) as u64)
}

/// The contract's Bayer order for the library's.
fn contract_bayer(bayer: BayerOrder) -> Result<ContractBayer, PispError> {
    Ok(match bayer {
        BayerOrder::Rggb => ContractBayer::Rggb,
        BayerOrder::Gbrg => ContractBayer::Gbrg,
        BayerOrder::Bggr => ContractBayer::Bggr,
        BayerOrder::Grbg => ContractBayer::Grbg,
        BayerOrder::Greyscale => ContractBayer::Mono,
    })
}

fn setup(text: String) -> PispError {
    PispError::Setup(text)
}

/// A stream's name, as the ISP answers the call that names it.
fn stream_name<E: std::fmt::Debug>(
    mut ask: impl FnMut(&mut [u8]) -> Result<usize, E>,
) -> Result<String, PispError> {
    let mut name = [0u8; MAX_STREAM_NAME];
    let len = ask(&mut name).map_err(|e| setup(format!("stream name: {e:?}")))?;
    std::str::from_utf8(&name[..len])
        .map(str::to_owned)
        .map_err(|_| setup("stream name is not UTF-8".to_string()))
}

/// Opens `name` writable on `device`, as a job stream.
fn open_stream(
    client: &Client,
    device: &DeviceEntry,
    name: &str,
    role: &str,
) -> Result<JobStream<()>, PispError> {
    open_job_stream::<()>(client, device.index, name)
        .map_err(|e| setup(format!("{role} stream {name}: {e}")))
}
