//! One `pispbe` node group for a stream of frames: the input imports dma-bufs (the front
//! end's raw buffers, no copy), up to two outputs with their own sizes and formats, a fresh
//! config per job, and output buffers held by the caller until released.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::media::MediaDevice;
use styx_kernel::v4l2::{
    BufType, BufferFlags, Format, Memory, MetaFormat, Planes, QueueBuffer, QueuePlane,
};

use super::be::BE_CFG_FOURCC;
use super::be_output::{OutputMemory, OutputQueue};
use super::be_spare::{take_buffer, top_up_free};
use super::be_tdn::TdnBuffers;
use super::config_buf::{ConfigBuffer, ConfigMemory};
use super::{DeviceError, Result, bayer16_fourcc, find_media};
use crate::format::formats;
use crate::uapi::{BayerOrder, BeTilesConfig, ImageFormatConfig, rgb_enable};

/// An output format of the back end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeFormat {
    /// `NV12`: Y plane then interleaved CbCr in one buffer.
    Nv12,
    /// `RGB3`: R, G, B bytes.
    Rgb24,
    /// `GREY`: 8-bit luma only.
    Grey,
}

impl BeFormat {
    /// The V4L2 pixel format.
    pub fn fourcc(self) -> FourCc {
        FourCc::new(match self {
            Self::Nv12 => b"NV12",
            Self::Rgb24 => b"RGB3",
            Self::Grey => b"GREY",
        })
    }

    /// The PiSP image format.
    pub fn pisp_format(self) -> u32 {
        match self {
            Self::Nv12 => formats::NV12,
            Self::Rgb24 => formats::RGB888,
            Self::Grey => crate::uapi::image_format::BPS_8,
        }
    }
}

/// One output: format and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeOutputSetup {
    /// Format.
    pub format: BeFormat,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// A finished job: which output buffers hold the results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeJob {
    /// Output buffer index per output (`None` for an output that is not set up or that the
    /// job's config does not enable).
    pub outputs: [Option<u32>; 2],
    /// From queueing the config to output 0 (or 1) completing: the hardware's time, from the
    /// driver's completion timestamp (when the outputs were done, not when Styx took them).
    pub elapsed: Duration,
}

/// A queued job (see [`BackEndStream::process_queued`]).
#[derive(Debug)]
#[must_use = "a queued job holds output buffers until waited for"]
pub struct QueuedJob {
    outputs: [Option<u32>; 2],
    start: Instant,
    /// `start` on `CLOCK_MONOTONIC`, the clock of the driver's completion timestamps.
    start_mono: Duration,
    /// The frame's last job (its only one, unless split: [`BackEndStream::process_queued_part`]).
    last: bool,
}

struct Output {
    queue: OutputQueue,
    format: ImageFormatConfig,
    /// Buffers free for the next job, least recently released first: a buffer handed back
    /// (and possibly exported as dma-bufs that consumers still read, e.g. a frame server's
    /// latest frame) is written again only after every other free buffer.
    free: VecDeque<u32>,
    /// Jobs dropped because no buffer was free or ready (the spare was not ready yet).
    misses: u64,
}

impl Output {
    /// A buffer for a job, without waiting: a free one, else a ready spare.
    fn take(&mut self) -> Option<u32> {
        take_buffer(&mut self.free, &mut self.queue, &mut self.misses)
    }
}

/// The back end set up for a stream. See the [module documentation](self).
pub struct BackEndStream {
    _media: MediaDevice,
    input: styx_kernel::v4l2::VideoDevice,
    inputs: Vec<OwnedFd>,
    input_len: u32,
    outputs: [Option<Output>; 2],
    config: ConfigBuffer,
    /// Input width, height, fourcc and stride (the TDN buffers' format).
    input_layout: (u32, u32, FourCc, u32),
    tdn: Option<TdnBuffers>,
}

pub(super) fn mplane(w: u32, h: u32, fourcc: FourCc, bpl: u32) -> Format {
    Format::Multi(styx_kernel::v4l2::PixFormatMplane {
        width: w,
        height: h,
        fourcc,
        field: 1,
        planes: vec![styx_kernel::v4l2::PlaneFormat {
            bytes_per_line: bpl,
            size_image: 0,
        }],
        ..Default::default()
    })
}

impl BackEndStream {
    /// Opens node group `group` for 16-bit Bayer `input` frames held in `inputs` (dma-bufs of
    /// `input_len` bytes, e.g. [`super::FrontEndDevice::image_dmabufs`]; input buffer `i` is
    /// `inputs[i]`), with `outputs` (at least output 0) and `buffers` buffers per output.
    pub fn open(
        group: usize,
        input: ImageFormatConfig,
        bayer: BayerOrder,
        inputs: Vec<OwnedFd>,
        input_len: u32,
        outputs: [Option<BeOutputSetup>; 2],
        buffers: u32,
    ) -> Result<Self> {
        Self::open_with(
            group,
            input,
            bayer,
            inputs,
            input_len,
            outputs,
            [buffers; 2],
            OutputMemory::Driver,
            ConfigMemory::default(),
        )
    }

    /// [`Self::open`] with the output buffers from `memory`, `buffers[i]` for output `i`, and the
    /// config buffer from `config`.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with(
        group: usize,
        input: ImageFormatConfig,
        bayer: BayerOrder,
        inputs: Vec<OwnedFd>,
        input_len: u32,
        outputs: [Option<BeOutputSetup>; 2],
        buffers: [u32; 2],
        memory: OutputMemory,
        config: ConfigMemory,
    ) -> Result<Self> {
        Self::open_with_initial(
            group, input, bayer, inputs, input_len, outputs, buffers, buffers, memory,
        )
    }

    /// [`Self::open_with`], allocating only `initial[i]` of output `i`'s `buffers[i]`
    /// [`OutputMemory::CachedHeap`] buffers now: a job that finds no free buffer allocates
    /// another, up to `buffers[i]` (the driver's buffers are all allocated at once). Buffers
    /// that consumers never hold at once are then never allocated.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_initial(
        group: usize,
        input: ImageFormatConfig,
        bayer: BayerOrder,
        inputs: Vec<OwnedFd>,
        input_len: u32,
        outputs: [Option<BeOutputSetup>; 2],
        buffers: [u32; 2],
        initial: [u32; 2],
        memory: OutputMemory,
    ) -> Result<Self> {
        if outputs[0].is_none() || inputs.is_empty() {
            return Err(DeviceError::Setup("need output 0 and input buffers".into()));
        }
        let path = find_media("pispbe")
            .into_iter()
            .nth(group)
            .ok_or_else(|| DeviceError::Setup(format!("no pispbe node group {group}")))?;
        let media = MediaDevice::open(path)?;
        let t = media.topology()?;
        let node = |name: &str| -> Result<styx_kernel::v4l2::VideoDevice> {
            let id = t
                .entity_by_name(name)
                .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))?
                .id;
            let p = t
                .devnode_path(id)
                .ok_or_else(|| DeviceError::Setup(format!("no device node for '{name}'")))?;
            Ok(styx_kernel::v4l2::VideoDevice::open(p)?)
        };
        let in_dev = node("pispbe-input")?;
        let fmt = mplane(
            u32::from(input.width),
            u32::from(input.height),
            bayer16_fourcc(bayer),
            input.stride as u32,
        );
        let Format::Multi(g) = in_dev.set_format(BufType::VideoOutputMplane, &fmt)? else {
            return Err(DeviceError::Setup(
                "pispbe-input is not multi-planar".into(),
            ));
        };
        if g.planes.first().map(|p| p.bytes_per_line) != Some(input.stride as u32) {
            return Err(DeviceError::Setup(format!(
                "pispbe-input planes {:?}",
                g.planes
            )));
        }
        let got = in_dev.request_buffers(
            BufType::VideoOutputMplane,
            Memory::DmaBuf,
            inputs.len() as u32,
        )?;
        if (got.count as usize) < inputs.len() {
            return Err(DeviceError::Setup(format!(
                "pispbe-input gave {} of {} buffers",
                got.count,
                inputs.len()
            )));
        }
        let mut outs: [Option<Output>; 2] = [None, None];
        for (i, setup) in outputs.iter().enumerate() {
            let Some(o) = setup else { continue };
            let dev = node(&format!("pispbe-output{i}"))?;
            let Format::Multi(g) = dev.set_format(
                BufType::VideoCaptureMplane,
                &mplane(o.width, o.height, o.format.fourcc(), 0),
            )?
            else {
                return Err(DeviceError::Setup(format!(
                    "pispbe-output{i} is not multi-planar"
                )));
            };
            if (g.width, g.height, g.fourcc) != (o.width, o.height, o.format.fourcc()) {
                return Err(DeviceError::Setup(format!(
                    "pispbe-output{i} gave {}x{} {}",
                    g.width, g.height, g.fourcc
                )));
            }
            let stride = g.planes[0].bytes_per_line as i32;
            let format = ImageFormatConfig {
                width: g.width as u16,
                height: g.height as u16,
                format: o.format.pisp_format(),
                stride,
                stride2: if o.format == BeFormat::Nv12 {
                    stride
                } else {
                    0
                },
            };
            let name = if i == 0 {
                "pispbe-output0"
            } else {
                "pispbe-output1"
            };
            let size = g.planes[0].size_image as usize;
            let count = buffers[i].max(2);
            let queue = OutputQueue::new(dev, memory, count, initial[i].min(count), size, name)?;
            let free = (0..queue.len() as u32).collect();
            outs[i] = Some(Output {
                queue,
                format,
                free,
                misses: 0,
            });
        }
        let cfg_dev = node("pispbe-config")?;
        cfg_dev.set_format(
            BufType::MetaOutput,
            &Format::Meta(MetaFormat {
                fourcc: BE_CFG_FOURCC,
                ..Default::default()
            }),
        )?;
        let config = ConfigBuffer::new(
            cfg_dev,
            BufType::MetaOutput,
            size_of::<BeTilesConfig>(),
            "pispbe-config",
            config,
        )?;
        let s = Self {
            _media: media,
            input: in_dev,
            inputs,
            input_len,
            outputs: outs,
            config,
            input_layout: (
                u32::from(input.width),
                u32::from(input.height),
                bayer16_fourcc(bayer),
                input.stride as u32,
            ),
            tdn: None,
        };
        s.input.stream_on(BufType::VideoOutputMplane)?;
        for o in s.outputs.iter().flatten() {
            o.queue.stream_on()?;
        }
        s.config.stream_on()?;
        Ok(s)
    }

    /// Sets up temporal denoise: two buffers of the input's format on `pispbe-tdn_input` and
    /// `pispbe-tdn_output`, swapped every job. Jobs whose config enables `TDN_OUTPUT` (and
    /// `TDN_INPUT`) use them; the config's TDN formats must be [`Self::tdn_format`].
    pub fn enable_tdn(&mut self) -> Result<()> {
        if self.tdn.is_some() {
            return Ok(());
        }
        let t = self._media.topology()?;
        let node = |name: &str| -> Result<styx_kernel::v4l2::VideoDevice> {
            let id = t
                .entity_by_name(name)
                .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))?
                .id;
            let p = t
                .devnode_path(id)
                .ok_or_else(|| DeviceError::Setup(format!("no device node for '{name}'")))?;
            Ok(styx_kernel::v4l2::VideoDevice::open(p)?)
        };
        self.tdn = Some(TdnBuffers::new(
            node("pispbe-tdn_input")?,
            node("pispbe-tdn_output")?,
            self.input_layout,
        )?);
        Ok(())
    }

    /// Temporal denoise is set up ([`Self::enable_tdn`]).
    pub fn tdn_enabled(&self) -> bool {
        self.tdn.is_some()
    }

    /// The format of the TDN buffers (the input's).
    ///
    /// `compressed`: 8-bit PiSP mode 1 (as libcamera keeps it: half the TDN memory traffic), in
    /// the same buffers. With `debug-hooks`, `STYX_PISP_TDN_COMPRESS=0|1` overrides it.
    pub fn tdn_format(&self, compressed: bool) -> ImageFormatConfig {
        let (w, h, _, stride) = self.input_layout;
        #[cfg(feature = "debug-hooks")]
        let compressed = match std::env::var("STYX_PISP_TDN_COMPRESS").as_deref() {
            Ok("0") => false,
            Ok("1") => true,
            _ => compressed,
        };
        if compressed {
            return ImageFormatConfig {
                width: w as u16,
                height: h as u16,
                format: formats::PISP_COMP1,
                stride: w.next_multiple_of(16) as i32,
                stride2: 0,
            };
        }
        ImageFormatConfig {
            width: w as u16,
            height: h as u16,
            format: formats::BAYER16,
            stride: stride as i32,
            stride2: 0,
        }
    }

    /// Forgets the temporal average: the next job must have `TDN_INPUT` off.
    pub fn reset_tdn(&mut self) {
        if let Some(t) = &mut self.tdn {
            t.reset();
        }
    }

    /// Where the config buffer comes from (`mmap`, or a dma-heap: [`ConfigMemory`]).
    pub fn config_source(&self) -> String {
        self.config.source()
    }

    /// Output `i`'s format as the node set it (put it in the config's output format).
    pub fn output_format(&self, i: usize) -> Option<ImageFormatConfig> {
        self.outputs.get(i)?.as_ref().map(|o| o.format)
    }

    /// Runs one job on input buffer `input` with `cfg`, waiting up to `timeout`. The output
    /// buffers stay with the caller until [`Self::release`]; fails when an output has no free
    /// buffer.
    pub fn process(&mut self, input: u32, cfg: &BeTilesConfig, timeout: Duration) -> Result<BeJob> {
        let job = self.process_queued(input, cfg)?;
        self.wait_job(job, timeout)
    }

    /// Queues one job on input buffer `input` with `cfg` and returns at once; finish it with
    /// [`Self::wait_job`] (the caller can work meanwhile: the job takes about 0.8 ms for
    /// 1280x800).
    pub fn process_queued(&mut self, input: u32, cfg: &BeTilesConfig) -> Result<QueuedJob> {
        self.process_queued_part(input, cfg, None, true)
    }

    /// Queues one job of a frame split into several (`styx_pisp::be::split_tiles`): the first
    /// (`held: None`) takes output buffers and the temporal-denoise slots as a whole frame
    /// does; the next ones pass the outputs the first returned ([`BeJob::outputs`]) and write
    /// into the same buffers and slots, each its own tiles. `last`: the frame's final job, whose
    /// completion makes the average it wrote the one the next frame reads.
    pub fn process_queued_part(
        &mut self,
        input: u32,
        cfg: &BeTilesConfig,
        held: Option<[Option<u32>; 2]>,
        last: bool,
    ) -> Result<QueuedJob> {
        let patched = super::config_dump::be_patch(cfg);
        let cfg = patched.as_deref().unwrap_or(cfg);
        let bytes = cfg.as_bytes();
        super::profile::time("pispbe-config", "copy", || self.config.write(bytes));
        super::config_dump::be_config(bytes);
        // Only the outputs the config enables get a buffer (a job can write one of them, e.g.
        // an extra pass for a region of the frame).
        let enabled = cfg.config.global.rgb_enables;
        let mut picked = held.unwrap_or_default();
        for (i, o) in self.outputs.iter_mut().enumerate() {
            if held.is_none()
                && let Some(o) = o
                && enabled & rgb_enable::output(i) != 0
            {
                // Every buffer is out and no spare is ready: the frame is dropped, not waited for.
                let Some(b) = o.take() else {
                    self.give_back(picked);
                    return Err(DeviceError::OutputsHeld(i));
                };
                picked[i] = Some(b);
            }
        }
        if let Some(t) = &mut self.tdn
            && let Err(e) = t.queue_part(cfg.config.global.bayer_enables, held.is_none())
        {
            t.abandon();
            self.give_back(picked);
            return Err(e);
        }
        let queued = self.queue_job(input, picked, bytes.len() as u32);
        match queued {
            Ok((start, start_mono)) => Ok(QueuedJob {
                outputs: picked,
                start,
                start_mono,
                last,
            }),
            Err(e) => {
                self.give_back(picked);
                Err(e)
            }
        }
    }

    /// Waits up to `timeout` for a job from [`Self::process_queued`] to finish.
    pub fn wait_job(&mut self, job: QueuedJob, timeout: Duration) -> Result<BeJob> {
        let mut result = self.wait(&job, timeout);
        if let Some(t) = &mut self.tdn {
            match &result {
                Ok(_) => {
                    if let Err(e) = t.complete(timeout, job.last) {
                        t.abandon();
                        result = Err(e);
                    }
                }
                Err(_) => t.abandon(),
            }
        }
        if result.is_err() {
            // Whatever completed is unusable; hand the buffers back.
            self.give_back(job.outputs);
        }
        result
    }

    fn give_back(&mut self, picked: [Option<u32>; 2]) {
        for (o, b) in self.outputs.iter_mut().zip(picked) {
            if let (Some(o), Some(b)) = (o, b)
                && !o.free.contains(&b)
            {
                o.free.push_front(b);
            }
        }
    }

    fn queue_job(
        &self,
        input: u32,
        picked: [Option<u32>; 2],
        cfg_len: u32,
    ) -> Result<(Instant, Duration)> {
        let fd = self
            .inputs
            .get(input as usize)
            .ok_or_else(|| DeviceError::Setup(format!("no input buffer {input}")))?
            .as_fd();
        for (o, b) in self.outputs.iter().zip(picked) {
            if let (Some(o), Some(b)) = (o, b) {
                o.queue.queue(b)?;
            }
        }
        let mut q = QueueBuffer::dmabuf(BufType::VideoOutputMplane, input, &[fd]);
        q.planes = Planes::one(QueuePlane {
            dmabuf: Some(fd),
            length: self.input_len,
            bytes_used: self.input_len,
            data_offset: 0,
        });
        super::profile::time("pispbe-input", "qbuf", || self.input.queue(&q))?;
        let (start, start_mono) = (Instant::now(), styx_kernel::monotonic_now());
        // The job starts when the config is queued (the driver writes it to the hardware).
        self.config.queue(cfg_len)?;
        Ok((start, start_mono))
    }

    fn wait(&self, job: &QueuedJob, timeout: Duration) -> Result<BeJob> {
        let mut elapsed = None;
        let mut error = false;
        for (o, b) in self.outputs.iter().zip(job.outputs) {
            let (Some(o), Some(_)) = (o, b) else { continue };
            let done = o.queue.dequeue(timeout)?;
            error |= done.flags.contains(BufferFlags::ERROR);
            // The driver stamps the outputs when the hardware finishes (`CLOCK_MONOTONIC`); the
            // time until here also counts whatever this thread did meanwhile (the 3A
            // algorithms run while the job does).
            let hw = done
                .timestamp
                .checked_sub(job.start_mono)
                .filter(|_| !done.timestamp.is_zero());
            elapsed.get_or_insert_with(|| hw.unwrap_or_else(|| job.start.elapsed()));
        }
        // The input and config buffers completed with the outputs.
        let deadline = Instant::now() + timeout;
        loop {
            if super::profile::time("pispbe-input", "dqbuf", || {
                self.input
                    .dequeue(BufType::VideoOutputMplane, Memory::DmaBuf)
            })?
            .is_some()
            {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DeviceError::Timeout("pispbe-input"));
            }
            super::profile::time("pispbe-input", "poll", || self.input.wait(Some(left)))?;
        }
        self.config.dequeue(timeout)?;
        if error {
            return Err(DeviceError::Setup("pispbe returned an error buffer".into()));
        }
        Ok(BeJob {
            outputs: job.outputs,
            elapsed: elapsed.unwrap_or_default(),
        })
    }

    /// The bytes of output `i`'s buffer `index`.
    pub fn output_data(&self, i: usize, index: u32) -> Option<&[u8]> {
        let o = self.outputs.get(i)?.as_ref()?;
        o.queue.data(index)
    }

    /// Brackets CPU reads of output `i`'s buffer `index` (`DMA_BUF_IOCTL_SYNC`): `start`
    /// before reading (invalidates a cached buffer), then again with `start` false. Needed
    /// for [`OutputMemory::CachedHeap`] buffers, a no-op for the driver's.
    pub fn sync_output(&self, i: usize, index: u32, start: bool) -> Result<()> {
        let fd = self
            .output_dmabuf(i, index)
            .ok_or_else(|| DeviceError::Setup(format!("no output {i} buffer {index}")))?;
        styx_kernel::dma_heap::sync(fd, styx_kernel::dma_heap::Access::Read, start)?;
        Ok(())
    }

    /// Whether output `i`'s buffers are cached for the CPU (a cached dma-heap) rather than the
    /// driver's uncached ones.
    pub fn output_cached(&self, i: usize) -> bool {
        self.outputs
            .get(i)
            .and_then(Option::as_ref)
            .is_some_and(|o| o.queue.cached())
    }

    /// The dma-buf of output `i`'s buffer `index`.
    pub fn output_dmabuf(&self, i: usize, index: u32) -> Option<BorrowedFd<'_>> {
        let o = self.outputs.get(i)?.as_ref()?;
        o.queue.dmabuf(index)
    }

    /// Keeps the spare buffers coming ([`Self::open_with_initial`]): adopts a ready spare for
    /// each output with no free buffer, and asks for the next spare while an output's free
    /// list is low. Call between frames. Never allocates on this thread.
    pub fn top_up(&mut self) {
        for o in self.outputs.iter_mut().flatten() {
            top_up_free(&mut o.free, &mut o.queue);
        }
    }

    /// Jobs dropped per output because no buffer was free or ready, since the stream opened.
    pub fn spare_misses(&self) -> [u64; 2] {
        self.outputs
            .each_ref()
            .map(|o| o.as_ref().map_or(0, |o| o.misses))
    }

    /// Spare buffers per output adopted before their prepare (the queueing mapped them).
    pub fn spare_unprepared(&self) -> [u64; 2] {
        self.outputs
            .each_ref()
            .map(|o| o.as_ref().map_or(0, |o| o.queue.unprepared()))
    }

    /// Hands output `i`'s buffer `index` back for reuse (outputs of one job can be held for
    /// different times).
    pub fn release_output(&mut self, i: usize, index: u32) {
        if let Some(Some(o)) = self.outputs.get_mut(i)
            && !o.free.contains(&index)
            && (index as usize) < o.queue.len()
        {
            o.free.push_back(index);
        }
    }

    /// Hands a job's output buffers back for reuse.
    /// Output `i`'s node descriptor: readable once a queued job has written its buffer (to
    /// wait for a job without blocking, e.g. alongside other descriptors).
    pub fn output_fd(&self, i: usize) -> Option<BorrowedFd<'_>> {
        self.outputs.get(i)?.as_ref().map(|o| o.queue.fd())
    }

    pub fn release(&mut self, job: &BeJob) {
        for (o, b) in self.outputs.iter_mut().zip(job.outputs) {
            if let (Some(o), Some(b)) = (o, b)
                && !o.free.contains(&b)
            {
                o.free.push_back(b);
            }
        }
    }

    /// Stops streaming and frees the buffers.
    pub fn stop(self) -> Result<()> {
        let t = self.tdn.map(TdnBuffers::close);
        let a = self.config.close();
        let b = self.input.stream_off(BufType::VideoOutputMplane);
        let c = self
            .input
            .free_buffers(BufType::VideoOutputMplane, Memory::DmaBuf);
        let mut first = a.err();
        for o in self.outputs.into_iter().flatten() {
            if let Err(e) = o.queue.close() {
                first.get_or_insert(e);
            }
        }
        b?;
        c?;
        if let Some(Err(e)) = t {
            first.get_or_insert(e);
        }
        first.map_or(Ok(()), Err)
    }
}
