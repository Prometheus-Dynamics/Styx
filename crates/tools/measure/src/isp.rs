//! The software ISP as a consumer of the socket's frames: each raw frame goes through
//! [`SoftIspStage`] into a picture the output pool hands out. The pool and the stage are sized
//! once, from the first frame's format; a frame after that allocates nothing.

use std::fmt;
use std::str::FromStr;

use styx_aaa::Statistics;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_settings::IspSettings;
use styx_soft::{Options, SoftIspStage};

use crate::clock::monotonic_ns;
use crate::hist::Histogram;
use crate::stats::IspStats;

/// Output buffers in the picture pool. Each output is dropped as soon as its frame is done, so
/// the pool needs only one; a few more keep the count honest if a consumer were to hold one.
const OUTPUT_BUFFERS: usize = 4;

/// The `--soft-isp` argument: an output FourCc, and optionally its size (`NV12:640x400`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftIspArg {
    pub code: FourCc,
    pub size: Option<(u32, u32)>,
}

impl FromStr for SoftIspArg {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (name, size) = match text.split_once(':') {
            Some((name, size)) => (name, Some(size)),
            None => (text, None),
        };
        let code = FourCc::from_str(name).map_err(|e| format!("--soft-isp {name}: {e:?}"))?;
        let size = match size {
            None => None,
            Some(size) => {
                let (w, h) = size
                    .split_once('x')
                    .ok_or_else(|| format!("--soft-isp size {size}: expected WxH"))?;
                let w: u32 = w.parse().map_err(|_| format!("--soft-isp width {w}"))?;
                let h: u32 = h.parse().map_err(|_| format!("--soft-isp height {h}"))?;
                if w == 0 || h == 0 {
                    return Err("--soft-isp size: width and height must be above zero".into());
                }
                Some((w, h))
            }
        };
        Ok(Self { code, size })
    }
}

#[derive(Debug)]
pub enum IspError {
    /// The stage could not be made for the frames' format.
    Setup(String),
    /// The output pool could not be made.
    Pool(String),
    /// A frame did not go through the stage.
    Frame(String),
}

impl fmt::Display for IspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IspError::Setup(why) => write!(f, "soft ISP: {why}"),
            IspError::Pool(why) => write!(f, "soft ISP output pool: {why}"),
            IspError::Frame(why) => write!(f, "soft ISP frame: {why}"),
        }
    }
}

impl std::error::Error for IspError {}

/// The runner: a stage and its output pool, made at the first frame.
#[derive(Debug)]
pub struct IspRunner {
    arg: SoftIspArg,
    stage: Option<SoftIspStage>,
    pool: Option<FramePool<()>>,
    out_code: FourCc,
    out_size: (u32, u32),
    times: Histogram,
    errors: u64,
}

impl IspRunner {
    pub fn new(arg: SoftIspArg) -> Self {
        Self {
            out_code: arg.code,
            out_size: arg.size.unwrap_or((0, 0)),
            arg,
            stage: None,
            pool: None,
            times: Histogram::new(),
            errors: 0,
        }
    }

    /// Processes one raw frame. The first call makes the stage and the pool; every later call
    /// reuses them.
    pub fn process(&mut self, input: &FrameLease<()>) -> Result<(), IspError> {
        if self.stage.is_none() {
            self.setup(input)?;
        }
        let pool = self.pool.as_ref().expect("the pool is made with the stage");
        let stage = self.stage.as_mut().expect("the stage is made first");
        let (out_w, out_h) = self.out_size;
        let res = Resolution::new(out_w, out_h)
            .ok_or_else(|| IspError::Setup("output size is zero".into()))?;
        let format = MediaFormat::with_defaults(self.out_code, res);
        let layouts = packed_layouts(self.out_code, out_w, out_h, None, None)
            .map_err(|e| IspError::Setup(format!("{e:?}")))?;
        let entry = FrameEntry::new(input.id(), input.timestamp_ns(), format, layouts);
        let mut out = match pool.acquire(entry) {
            Ok(out) => out,
            Err(_) => {
                // Counted by the pool's own miss counter; the frame is not processed.
                return Ok(());
            }
        };
        let start = monotonic_ns();
        let result = stage.process::<(), ()>(input, &mut out, None::<&mut Statistics>);
        let took = monotonic_ns().saturating_sub(start);
        drop(out);
        match result {
            Ok(()) => {
                self.times.record(took);
                Ok(())
            }
            Err(e) => {
                self.errors += 1;
                Err(IspError::Frame(format!("frame {}: {e}", input.id())))
            }
        }
    }

    fn setup(&mut self, input: &FrameLease<()>) -> Result<(), IspError> {
        let format = input.format();
        let (w, h) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        if self.out_size == (0, 0) {
            self.out_size = (w, h);
        }
        let settings = IspSettings::neutral(0.0);
        let stage = SoftIspStage::new(format.code, w, h, &settings, Options::default())
            .map_err(|e| IspError::Setup(format!("{:?}: {e}", format.code)))?;
        let (out_w, out_h) = self.out_size;
        let layouts = packed_layouts(self.out_code, out_w, out_h, None, None)
            .map_err(|e| IspError::Pool(format!("{e:?}")))?;
        let payload = layouts
            .span()
            .ok_or_else(|| IspError::Pool("the output picture has no size".into()))?;
        let pool = FramePool::host(OUTPUT_BUFFERS, BufferSize::new(payload, 0))
            .map_err(|e| IspError::Pool(format!("{e:?}")))?;
        self.stage = Some(stage);
        self.pool = Some(pool);
        Ok(())
    }

    /// The counts so far.
    pub fn stats(&self) -> IspStats {
        let pool = self.pool.as_ref().map(|p| p.stats()).unwrap_or_default();
        IspStats {
            times: self.times.clone(),
            errors: self.errors,
            pool_misses: pool.misses as u64,
            pool_acquired: pool.acquired as u64,
            pool_buffers: pool.buffers,
            output: self.arg.code.to_string(),
            width: self.out_size.0,
            height: self.out_size.1,
        }
    }
}
