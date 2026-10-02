//! The public processing API.

use crate::format::RawFormat;
use crate::output::{Kind, OutputBuffers, Scale};
use crate::params::{Arithmetic, IspParams};
use crate::pipeline::{Source, Worker};
use crate::pool::Pool;
use crate::prepare::Prepared;
use crate::stats::IspStats;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    workers: Vec<Mutex<Worker>>,
    threads: usize,
    pool: Option<Pool>,
    copy_input: bool,
    lsc_tolerance: f32,
    statistics: bool,
}

impl std::fmt::Debug for SoftIsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftIsp")
            .field("format", &self.format)
            .field("params", &self.params)
            .field("threads", &self.threads)
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
            workers: vec![Mutex::default()],
            threads: 1,
            pool: None,
            copy_input: true,
            lsc_tolerance: 0.0,
            statistics: true,
        })
    }

    /// Process each frame in row bands on `threads` threads: the calling thread and
    /// `threads - 1` helper threads this ISP starts on first use and keeps, asleep between
    /// frames (0: one thread per CPU). Bands are claimed dynamically, two per thread.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.set_threads(threads);
        self
    }

    pub fn set_threads(&mut self, threads: usize) {
        if threads != self.threads {
            self.pool = None;
        }
        self.threads = threads;
    }

    /// The thread count set with [`Self::with_threads`].
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Whether input rows are copied, 16 KiB at a time, into a cached buffer before they are
    /// unpacked (default: yes). Frames in DMA buffers the CPU maps uncached (write-combined),
    /// such as V4L2 MMAP buffers of many receivers, read several times faster that way: on the
    /// CM5 a 1280x800 RAW10 frame took 10.0 instead of 5.1 ms without it. From cached memory
    /// the copy costs under 0.1 ms per frame there; input known to be cached can skip it.
    pub fn set_copy_input(&mut self, copy: bool) {
        self.copy_input = copy;
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
        let (w, h) = (self.format.width as usize, self.format.height as usize);
        let row = self.format.min_stride();
        if stride < row || input.len() < stride * (h - 1) + row {
            return Err(IspError::InputTooShort(format!(
                "{} bytes with stride {stride} for {w}x{h} {:?} (rows of {row} bytes)",
                input.len(),
                self.format.packing
            )));
        }
        let (ow, oh) = output_size(&self.format, scale);
        let (ow, oh) = (ow as usize, oh as usize);
        out.validate(ow, oh)?;
        if matches!(out.kind(), Kind::Nv12 | Kind::I420) && (ow % 2 != 0 || oh % 2 != 0) {
            return Err(IspError::Unsupported(format!(
                "4:2:0 output of odd size {ow}x{oh}"
            )));
        }
        let src = Source {
            data: input,
            stride,
            packing: self.format.packing,
            copy: self.copy_input,
        };
        let p = &self.prepared;
        let threads = self.thread_count();
        let bands = band_rows(oh, threads);
        let outs = out.into_bands(bands, oh);
        let used = threads.min(outs.len());
        if self.workers.len() < used {
            self.workers.resize_with(used, Default::default);
        }
        let workers = &mut self.workers[..used];
        for w in workers.iter_mut() {
            w.get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .begin_frame(p, self.statistics);
        }
        if used <= 1 {
            let w = workers[0].get_mut().unwrap_or_else(|e| e.into_inner());
            for (k, o) in outs.into_iter().enumerate() {
                let o0 = k * bands;
                w.run_band(p, &src, scale, o0, bands.min(oh - o0), o);
            }
        } else {
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
                    w.run_band(p, &src, scale, o0, bands.min(oh - o0), out);
                }
            });
        }
        Ok(p.stats.as_ref().filter(|_| self.statistics).map(|setup| {
            let mut ws = self.workers[..used]
                .iter_mut()
                .map(|w| w.get_mut().unwrap_or_else(|e| e.into_inner()));
            let first = ws.next().expect("one worker at least");
            let mut acc = first.stats.clone().expect("stats prepared");
            for w in ws {
                acc.merge(w.stats.as_ref().expect("stats prepared"));
            }
            acc.finish(setup, p.channel_gains)
        }))
    }

    /// Threads processing a frame.
    fn thread_count(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
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
