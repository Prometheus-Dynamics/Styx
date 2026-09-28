//! The [`Algorithm`] trait and the [`Pipeline`] that runs an ordered set of them.

use crate::algos::{Agc, Alsc, Awb, BlackLevel, Ccm, Contrast, Lux};
use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::FrameMetadata;
use crate::params::Params;
use crate::stats::Statistics;
use crate::tuning::Tuning;

/// A control algorithm.
///
/// Implementations must be deterministic: the same `prepare` followed by the same sequence of
/// `process` inputs gives the same outputs (no clocks, threads, randomness or hash-order
/// iteration). All state lives in the value itself and `prepare` resets it.
pub trait Algorithm: Send {
    /// A short name, e.g. `"agc"`.
    fn name(&self) -> &'static str;

    /// Configure for a camera mode and reset all per-stream state.
    fn prepare(&mut self, config: &CameraConfig) -> Result<()>;

    /// Write start-up values (before the first frame), e.g. the initial exposure.
    fn initial(&self, _params: &mut Params) {}

    /// Process one frame's statistics. Read what earlier algorithms wrote to `params`, write
    /// this algorithm's outputs.
    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params);
}

/// An ordered set of algorithms sharing one [`Params`].
#[derive(Default)]
pub struct Pipeline {
    algorithms: Vec<Box<dyn Algorithm>>,
    params: Params,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("algorithms", &self.names())
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// The standard pipeline for a tuning, in this order: black level, lux (if tuned), AWB,
    /// AGC, ALSC (if tuned), CCM, contrast. Sections missing from the tuning use defaults, so an
    /// empty tuning gives a working grey-world, centre-weighted pipeline.
    pub fn from_tuning(tuning: &Tuning) -> Result<Self> {
        tuning.validate()?;
        let mut p = Self::new();
        p.push(BlackLevel::new(tuning.black_level));
        if let Some(lux) = &tuning.lux {
            p.push(Lux::new(*lux));
        }
        p.push(Awb::new(tuning.awb.clone().unwrap_or_default())?);
        p.push(Agc::new(tuning.agc.clone().unwrap_or_default())?);
        if let Some(alsc) = &tuning.alsc {
            p.push(Alsc::new(alsc.clone())?);
        }
        p.push(Ccm::new(tuning.ccm.clone().unwrap_or_default())?);
        p.push(Contrast::new(tuning.contrast.clone().unwrap_or_default()));
        Ok(p)
    }

    /// Append an algorithm.
    pub fn push(&mut self, algorithm: impl Algorithm + 'static) {
        self.algorithms.push(Box::new(algorithm));
    }

    /// Append a boxed algorithm.
    pub fn push_boxed(&mut self, algorithm: Box<dyn Algorithm>) {
        self.algorithms.push(algorithm);
    }

    /// Algorithm names in run order.
    pub fn names(&self) -> Vec<&'static str> {
        self.algorithms.iter().map(|a| a.name()).collect()
    }

    /// Prepare every algorithm for a camera mode, reset the parameters and fill in start-up
    /// values. Returns the parameters to apply before streaming.
    pub fn prepare(&mut self, config: &CameraConfig) -> Result<&Params> {
        config.validate()?;
        self.params = Params::default();
        for a in &mut self.algorithms {
            a.prepare(config)?;
        }
        for a in &self.algorithms {
            a.initial(&mut self.params);
        }
        Ok(&self.params)
    }

    /// Run every algorithm on one frame. Parameters not changed this frame keep their values.
    pub fn process(&mut self, stats: &Statistics, meta: &FrameMetadata) -> &Params {
        for a in &mut self.algorithms {
            a.process(stats, meta, &mut self.params);
        }
        &self.params
    }

    /// The latest parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }
}
