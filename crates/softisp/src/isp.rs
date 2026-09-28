//! The public processing API.

use crate::format::RawFormat;
use crate::output::{Kind, OutputBuffers, Scale};
use crate::params::IspParams;
use crate::pipeline::{Source, Worker};
use crate::prepare::Prepared;
use crate::stats::IspStats;

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
    workers: Vec<Worker>,
    threads: usize,
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
        let prepared = Prepared::new(&format, &params)?;
        Ok(Self {
            format,
            params,
            prepared,
            workers: vec![Worker::default()],
            threads: 1,
        })
    }

    /// Process row bands on `threads` threads of the rayon pool (0: one per pool thread).
    /// Without the `rayon` feature, frames are always processed on the calling thread.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.set_threads(threads);
        self
    }

    pub fn set_threads(&mut self, threads: usize) {
        self.threads = if cfg!(feature = "rayon") { threads } else { 1 };
    }

    /// Replace the parameters (for example new white balance gains from the last statistics).
    pub fn set_params(&mut self, params: IspParams) -> Result<(), IspError> {
        self.prepared = Prepared::new(&self.format, &params)?;
        self.params = params;
        Ok(())
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
        };
        let p = &self.prepared;
        let bands = self.band_rows(oh);
        let outs = out.into_bands(bands, oh);
        if self.workers.len() < outs.len() {
            self.workers.resize_with(outs.len(), Worker::default);
        }
        let jobs: Vec<(usize, OutputBuffers<'_>)> = outs
            .into_iter()
            .enumerate()
            .map(|(i, o)| (i * bands, o))
            .collect();
        run_jobs(&mut self.workers, jobs, |worker, (o0, out)| {
            worker.run_band(p, &src, scale, o0, bands.min(oh - o0), out)
        });
        let used = oh.div_ceil(bands);
        Ok(p.stats.as_ref().map(|setup| {
            let mut acc = self.workers[0].stats.clone().expect("stats prepared");
            for w in &self.workers[1..used] {
                acc.merge(w.stats.as_ref().expect("stats prepared"));
            }
            acc.finish(setup, p.channel_gains)
        }))
    }

    /// Output rows per band: all of them on one thread, else an even share.
    fn band_rows(&self, oh: usize) -> usize {
        #[cfg(feature = "rayon")]
        let threads = if self.threads == 0 {
            rayon::current_num_threads()
        } else {
            self.threads
        };
        #[cfg(not(feature = "rayon"))]
        let threads = 1;
        if threads <= 1 {
            return oh;
        }
        oh.div_ceil(threads).next_multiple_of(2).max(2)
    }
}

#[cfg(feature = "rayon")]
fn run_jobs<J: Send>(workers: &mut [Worker], jobs: Vec<J>, f: impl Fn(&mut Worker, J) + Sync) {
    use rayon::prelude::*;
    if jobs.len() == 1 {
        jobs.into_iter().for_each(|j| f(&mut workers[0], j));
        return;
    }
    workers.par_iter_mut().zip(jobs).for_each(|(w, j)| f(w, j));
}

#[cfg(not(feature = "rayon"))]
fn run_jobs<J>(workers: &mut [Worker], jobs: Vec<J>, f: impl Fn(&mut Worker, J)) {
    workers.iter_mut().zip(jobs).for_each(|(w, j)| f(w, j));
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
