//! The [`Algorithm`] trait and the [`Pipeline`] that runs an ordered set of them.

use crate::algos::{Agc, Alsc, Awb, BlackLevel, Ccm, Contrast, Denoise, Lux};
use crate::config::CameraConfig;
use crate::error::Result;
use crate::frame::FrameMetadata;
use crate::params::Params;
use crate::stats::Statistics;
use crate::tuning::Tuning;
use crate::warm::WarmStart;

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

    /// Start from settled values of an earlier session (called after `prepare`, before
    /// `initial`). The default ignores them.
    fn warm_start(&mut self, _warm: &WarmStart) {}

    /// Write start-up values (before the first frame), e.g. the initial exposure.
    fn initial(&self, _params: &mut Params) {}

    /// Process one frame's statistics. Read what earlier algorithms wrote to `params`, write
    /// this algorithm's outputs.
    fn process(&mut self, stats: &Statistics, meta: &FrameMetadata, params: &mut Params);
}

/// An ordered set of algorithms sharing one [`Params`].
pub struct Pipeline {
    algorithms: Vec<Box<dyn Algorithm>>,
    params: Params,
    sensitivity: f64,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            algorithms: Vec::new(),
            params: Params::default(),
            sensitivity: 1.0,
        }
    }
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
    /// AGC, ALSC (if tuned), CCM, contrast, denoise (if tuned). Sections missing from the tuning use defaults, so an
    /// empty tuning gives a working grey-world, centre-weighted pipeline.
    pub fn from_tuning(tuning: &Tuning) -> Result<Self> {
        tuning.validate()?;
        let mut p = Self::new();
        p.push(BlackLevel::new(tuning.black_level.clone()));
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
        if let Some(d) = &tuning.denoise {
            p.push(Denoise::new(d.clone())?);
        }
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
        self.prepare_warm(config, None)
    }

    /// [`Self::prepare`], starting from an earlier session's settled values when given (see
    /// [`WarmStart`]; invalid values are ignored).
    pub fn prepare_warm(
        &mut self,
        config: &CameraConfig,
        warm: Option<&WarmStart>,
    ) -> Result<&Params> {
        config.validate()?;
        self.params = Params::default();
        self.sensitivity = config.sensitivity;
        for a in &mut self.algorithms {
            a.prepare(config)?;
        }
        if let Some(w) = warm.filter(|w| w.is_valid()) {
            self.params.lux = w.lux;
            for a in &mut self.algorithms {
                a.warm_start(w);
            }
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

    /// What the algorithms have settled on, to start a later session from (`None` before AE
    /// produced a request).
    pub fn warm_state(&self) -> Option<WarmStart> {
        WarmStart::from_params(&self.params, self.sensitivity)
    }
}
