//! The public processing API.

use alloc::{format, string::String, vec, vec::Vec};

use crate::format::RawFormat;
use crate::output::{Kind, OutputBuffers, Scale};
use crate::params::{Arithmetic, IspParams};
use crate::pipeline::{Geom, Source, Worker};
#[cfg(feature = "std")]
use crate::pool::Pool;
use crate::prepare::Prepared;
use crate::stats::{IspStats, StatsAccum};
#[cfg(feature = "std")]
use std::sync::Mutex;
#[cfg(feature = "std")]
use std::sync::atomic::{AtomicUsize, Ordering};

/// A worker's scratch state: behind a mutex with `std` (helper threads take it), plain without.
#[cfg(feature = "std")]
type WorkerSlot = Mutex<Worker>;
#[cfg(not(feature = "std"))]
#[derive(Default)]
struct WorkerSlot(Worker);

fn worker_mut(w: &mut WorkerSlot) -> &mut Worker {
    #[cfg(feature = "std")]
    {
        w.get_mut().unwrap_or_else(|e| e.into_inner())
    }
    #[cfg(not(feature = "std"))]
    {
        &mut w.0
    }
}

/// A rectangle of the frame in mosaic pixels ([`SoftIsp::process_window`]): even origin and
/// size.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Window {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Window {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Why a frame could not be processed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IspError {
    #[error("invalid ISP parameters: {0}")]
    InvalidParams(String),
    #[error("unsupported frame: {0}")]
    Unsupported(String),
    #[error("input too short: {0}")]
    InputTooShort(String),
    #[error("output buffer too small: {0}")]
    OutputTooSmall(String),
}

/// A software ISP configured for one raw format: tables prepared from the parameters and
/// scratch rows reused from frame to frame.
///
/// ```
/// use styx_softisp::*;
/// let format = RawFormat::new(64, 48, CfaPattern::Bggr, RawPacking::Csi2Raw10);
/// let mut isp = SoftIsp::new(format, IspParams::default()).unwrap();
/// let raw = vec![0u8; format.min_stride() * 48];
/// let mut rgb = vec![0u8; 64 * 48 * 3];
/// isp.process(&raw, format.min_stride(), Scale::Full, OutputBuffers::Rgb24 { data: &mut rgb, stride: 64 * 3 })
///     .unwrap();
/// ```
pub struct SoftIsp {
    format: RawFormat,
    params: IspParams,
    prepared: Prepared,
    workers: Vec<WorkerSlot>,
    #[cfg(feature = "std")]
    threads: usize,
    #[cfg(feature = "std")]
    pool: Option<Pool>,
    copy_input: bool,
    lsc_tolerance: f32,
    statistics: bool,
}

impl core::fmt::Debug for SoftIsp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SoftIsp")
            .field("format", &self.format)
            .field("params", &self.params)
            .field("threads", &self.thread_setting())
            .finish()
    }
}

impl SoftIsp {
    pub fn new(format: RawFormat, params: IspParams) -> Result<Self, IspError> {
        let prepared = Prepared::new(&format, &params, None, 0.0)?;
        Ok(Self {
            format,
            params,
            prepared,
            workers: vec![WorkerSlot::default()],
            #[cfg(feature = "std")]
            threads: 1,
            #[cfg(feature = "std")]
            pool: None,
            copy_input: true,
            lsc_tolerance: 0.0,
            statistics: true,
        })
    }

    /// Process each frame in row bands on `threads` threads: the calling thread and
    /// `threads - 1` helper threads this ISP starts on first use and keeps, asleep between
    /// frames (0: one thread per CPU). Bands are claimed dynamically, two per thread. Needs
    /// `std`; without it frames run on the calling thread.
    #[cfg(feature = "std")]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.set_threads(threads);
        self
    }

    #[cfg(feature = "std")]
    pub fn set_threads(&mut self, threads: usize) {
        if threads != self.threads {
            self.pool = None;
        }
        self.threads = threads;
    }

    /// The thread count set with [`Self::with_threads`].
    #[cfg(feature = "std")]
    pub fn threads(&self) -> usize {
        self.threads
    }

    fn thread_setting(&self) -> usize {
        #[cfg(feature = "std")]
        {
            self.threads
        }
        #[cfg(not(feature = "std"))]
        {
            1
        }
    }

    /// Whether input rows are copied, 16 KiB at a time, into a cached buffer before they are
    /// unpacked (default: yes). Frames in DMA buffers the CPU maps uncached (write-combined),
    /// such as V4L2 MMAP buffers of many receivers, read several times faster that way: on the
    /// CM5 a 1280x800 RAW10 frame took 10.0 instead of 5.1 ms without it. From cached memory
    /// the copy costs under 0.1 ms per frame there; input known to be cached can skip it.
    pub fn set_copy_input(&mut self, copy: bool) {
        self.copy_input = copy;
    }

    /// Whether input rows are staged ([`Self::set_copy_input`]).
    pub fn copies_input(&self) -> bool {
        self.copy_input
    }

    /// See [`Self::set_copy_input`].
    pub fn with_copy_input(mut self, copy: bool) -> Self {
        self.copy_input = copy;
        self
    }

    /// Replace the parameters (for example new white balance gains from the last statistics).
    pub fn set_params(&mut self, params: IspParams) -> Result<(), IspError> {
        self.prepared = Prepared::new(
            &self.format,
            &params,
            Some(&self.prepared),
            self.lsc_tolerance,
        )?;
        self.params = params;
        Ok(())
    }

    /// Keep the lens shading tables built for an earlier grid while every node of the new
    /// grid is within `tolerance` (relative, e.g. `0.002`) of it (default 0: rebuild on any
    /// change). Rebuilding costs about a third of a millisecond at 1280x800 on a Cortex-A76,
    /// and adaptive lens shading changes its grid a little on most frames. Only the fp16
    /// arithmetic keeps tables across white balance and gain changes; the integer
    /// arithmetic rebuilds them whenever any gain changes.
    pub fn set_lens_shading_tolerance(&mut self, tolerance: f32) {
        self.lsc_tolerance = tolerance.max(0.0);
    }

    /// Whether frames gather the statistics their parameters ask for (default: yes). A 3A
    /// loop that has settled can skip them on some frames: at the pipeline's settings they
    /// cost 0.2-0.5 ms of a 1280x800 frame on a Cortex-A76.
    pub fn set_statistics(&mut self, on: bool) {
        self.statistics = on;
    }

    /// The arithmetic the current parameters run with ([`Arithmetic::Auto`] resolved).
    pub fn arithmetic(&self) -> Arithmetic {
        self.prepared.arithmetic()
    }

    pub fn params(&self) -> &IspParams {
        &self.params
    }

    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// The output image size at `scale`.
    pub fn output_size(&self, scale: Scale) -> (u32, u32) {
        output_size(&self.format, scale)
    }

    /// Process one frame (`stride` bytes per input row) into `out`. Returns the statistics
    /// when the parameters ask for them.
    pub fn process(
        &mut self,
        input: &[u8],
        stride: usize,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<Option<IspStats>, IspError> {
        let src = self.source(input, stride)?;
        let (_, oh) = output_size(&self.format, scale);
        let g = Geom::frame(self.format.width as usize, scale, true);
        let acc = self.run(&src, g, oh as usize, out)?;
        Ok(self.finish(acc))
    }

    /// Process only `window` of the frame, at `scale`, into `out` (sized
    /// [`Self::window_size`]): the rows and columns of the window and the neighbours the
    /// demosaic needs, so a small window costs a small part of a frame. The pixels are those
    /// of the same window of [`Self::process`]'s picture, bit for bit. No statistics: those of
    /// the whole frame come from [`Self::process_binned`] or [`Self::statistics`].
    pub fn process_window(
        &mut self,
        input: &[u8],
        stride: usize,
        window: Window,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<(), IspError> {
        let src = self.source(input, stride)?;
        let (w, h) = (self.format.width, self.format.height);
        let Window {
            x,
            y,
            width,
            height,
        } = window;
        if width == 0
            || height == 0
            || !(x | y | width | height).is_multiple_of(2)
            || x + width > w
            || y + height > h
        {
            return Err(IspError::Unsupported(format!(
                "window {width}x{height} at ({x}, {y}): even origin and size within the \
                 {w}x{h} frame"
            )));
        }
        let margin = match self.prepared.demosaic {
            crate::params::Demosaic::Mhc => 2,
            crate::params::Demosaic::Bilinear => 1,
        };
        let g = Geom::window(
            w as usize,
            (x as usize, y as usize, width as usize),
            scale,
            margin,
        );
        let (_, oh) = self.window_size(window, scale);
        self.run(&src, g, oh as usize, out).map(drop)
    }

    /// The output size of `window` at `scale`.
    pub fn window_size(&self, window: Window, scale: Scale) -> (u32, u32) {
        match scale {
            Scale::Full => (window.width, window.height),
            Scale::Half => (window.width / 2, window.height / 2),
        }
    }

    /// The whole frame at 1/`factor` size (`factor` even) into `out` (sized
    /// [`Self::binned_size`]), with the whole frame's statistics: each pixel is a 2x2 quad
    /// of the mosaic, as [`Scale::Half`] (`factor` 2: every quad, the same picture); beyond 2
    /// every (`factor` / 2)-th quad of every (`factor` / 2)-th quad row, so the front end
    /// runs on 2 / `factor` of the rows (and the statistics' own). The cheap overview of a
    /// frame whose regions are processed with [`Self::process_window`].
    pub fn process_binned(
        &mut self,
        input: &[u8],
        stride: usize,
        factor: u32,
        out: OutputBuffers<'_>,
    ) -> Result<Option<IspStats>, IspError> {
        let src = self.source(input, stride)?;
        let (ow, oh) = self.binned_size(factor);
        if factor < 2 || !factor.is_multiple_of(2) || ow == 0 || oh == 0 {
            return Err(IspError::Unsupported(format!(
                "binning by {factor}: an even factor from 2 leaving at least 2x2 pixels"
            )));
        }
        let step = factor as usize / 2;
        let g = Geom::quads(self.format.width as usize, step, ow as usize, true);
        let acc = self.run(&src, g, oh as usize, out)?;
        // Quad rows below the last output row's.
        let (covered, quad_rows) = (oh as usize * step, self.format.height as usize / 2);
        let acc = match acc {
            Some(acc) if covered < quad_rows => {
                self.stats_rows(&src, covered..quad_rows, Some(acc))
            }
            acc => acc,
        };
        Ok(self.finish(acc))
    }

    /// The output size of [`Self::process_binned`] by `factor`: even sides.
    pub fn binned_size(&self, factor: u32) -> (u32, u32) {
        let f = factor.max(1);
        ((self.format.width / f) & !1, (self.format.height / f) & !1)
    }

    /// Only the frame's statistics (as [`Self::process`] gathers them), for a frame whose
    /// picture is made of windows alone: the front end on the quad rows they sample.
    pub fn statistics(
        &mut self,
        input: &[u8],
        stride: usize,
    ) -> Result<Option<IspStats>, IspError> {
        let src = self.source(input, stride)?;
        if !self.statistics || self.prepared.stats.is_none() {
            return Ok(None);
        }
        let quad_rows = self.format.height as usize / 2;
        let acc = self.stats_rows(&src, 0..quad_rows, None);
        Ok(self.finish(acc))
    }

    /// `acc` with the statistics of `quad_rows` added (gathered on the first worker).
    fn stats_rows(
        &mut self,
        src: &Source<'_>,
        quad_rows: core::ops::Range<usize>,
        acc: Option<StatsAccum>,
    ) -> Option<StatsAccum> {
        let p = &self.prepared;
        let w = worker_mut(&mut self.workers[0]);
        w.begin_frame(p, true);
        w.begin_band();
        let g = Geom::frame(p.width, Scale::Half, true);
        for qy in quad_rows {
            w.stats_pair(p, src, &g, 2 * qy);
        }
        let part = w.stats.as_ref()?;
        Some(match acc {
            Some(mut acc) => {
                acc.merge(part);
                acc
            }
            None => part.clone(),
        })
    }

    /// The statistics of `acc`.
    fn finish(&self, acc: Option<StatsAccum>) -> Option<IspStats> {
        let p = &self.prepared;
        Some(acc?.finish(p.stats.as_ref()?, p.channel_gains))
    }

    /// The input checked against the format.
    fn source<'a>(&self, input: &'a [u8], stride: usize) -> Result<Source<'a>, IspError> {
        let (w, h) = (self.format.width as usize, self.format.height as usize);
        let row = self.format.min_stride();
        if stride < row || input.len() < stride * (h - 1) + row {
            return Err(IspError::InputTooShort(format!(
                "{} bytes with stride {stride} for {w}x{h} {:?} (rows of {row} bytes)",
                input.len(),
                self.format.packing
            )));
        }
        Ok(Source {
            data: input,
            stride,
            packing: self.format.packing,
            copy: self.copy_input,
        })
    }

    /// `oh` output rows of `g` into `out`, in bands on the threads; the statistics when `g`
    /// gathers them and the parameters ask.
    fn run(
        &mut self,
        src: &Source<'_>,
        g: Geom,
        oh: usize,
        out: OutputBuffers<'_>,
    ) -> Result<Option<StatsAccum>, IspError> {
        let ow = g.ow;
        out.validate(ow, oh)?;
        if matches!(out.kind(), Kind::Nv12 | Kind::I420)
            && (!ow.is_multiple_of(2) || !oh.is_multiple_of(2))
        {
            return Err(IspError::Unsupported(format!(
                "4:2:0 output of odd size {ow}x{oh}"
            )));
        }
        let stats = g.stats && self.statistics;
        let p = &self.prepared;
        let threads = self.thread_count();
        let bands = band_rows(oh, threads);
        let outs = out.into_bands(bands, oh);
        let used = threads.min(outs.len()).max(1);
        if self.workers.len() < used {
            self.workers.resize_with(used, Default::default);
        }
        let workers = &mut self.workers[..used];
        for w in workers.iter_mut() {
            worker_mut(w).begin_frame(p, stats);
        }
        let g = &g;
        if used <= 1 {
            let w = worker_mut(&mut workers[0]);
            for (k, o) in outs.into_iter().enumerate() {
                let o0 = k * bands;
                w.run_band(p, src, g, o0, bands.min(oh - o0), o);
            }
        } else {
            // Without std there is one thread (`thread_count`): this is the std build.
            #[cfg(feature = "std")]
            {
                let pool = match &mut self.pool {
                    Some(pool) if pool.helpers() + 1 >= used => pool,
                    slot => slot.insert(Pool::new(threads - 1)),
                };
                // Bands are claimed in order by whichever thread is free.
                let jobs: Vec<Mutex<Option<OutputBuffers<'_>>>> =
                    outs.into_iter().map(|o| Mutex::new(Some(o))).collect();
                let next = AtomicUsize::new(0);
                let workers = &*workers;
                pool.run(used, &|t| {
                    let mut w = workers[t].lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(k) else { break };
                        let out = job
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                            .expect("each band is claimed once");
                        let o0 = k * bands;
                        w.run_band(p, src, g, o0, bands.min(oh - o0), out);
                    }
                });
            }
        }
        Ok(p.stats.as_ref().filter(|_| stats).map(|_| {
            let mut ws = self.workers[..used].iter_mut().map(worker_mut);
            let first = ws.next().expect("one worker at least");
            let mut acc = first.stats.clone().expect("stats prepared");
            for w in ws {
                acc.merge(w.stats.as_ref().expect("stats prepared"));
            }
            acc
        }))
    }

    /// Threads processing a frame.
    fn thread_count(&self) -> usize {
        #[cfg(feature = "std")]
        {
            match self.threads {
                0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
                n => n,
            }
        }
        #[cfg(not(feature = "std"))]
        {
            1
        }
    }
}

/// Output rows per band: all of them on one thread, else two bands per thread (claimed by
/// whichever thread is free, so a thread the system delays holds up less of the frame).
fn band_rows(oh: usize, threads: usize) -> usize {
    if threads <= 1 {
        return oh;
    }
    oh.div_ceil(2 * threads).next_multiple_of(2).max(2)
}

fn output_size(format: &RawFormat, scale: Scale) -> (u32, u32) {
    match scale {
        Scale::Full => (format.width, format.height),
        Scale::Half => (format.width / 2, format.height / 2),
    }
}

/// Process one frame with a throwaway [`SoftIsp`]; to process a stream, keep a `SoftIsp`.
pub fn process(
    format: RawFormat,
    params: &IspParams,
    input: &[u8],
    stride: usize,
    scale: Scale,
    out: OutputBuffers<'_>,
) -> Result<Option<IspStats>, IspError> {
    SoftIsp::new(format, params.clone())?.process(input, stride, scale, out)
}
