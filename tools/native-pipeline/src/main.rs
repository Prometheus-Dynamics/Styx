//! The native processing pipeline end to end: the 3A loop closed over a native camera with the
//! PiSP or the software ISP, or over a raw recording on a host.
//!
//! ```text
//! native-pipeline pisp   [options]   PiSP: FE statistics, BE NV12 + half-size RGB (device feature)
//! native-pipeline soft   [options]   software ISP on raw frames from csi2_ch0 (device feature)
//! native-pipeline replay --recording BASE [options]   software ISP over a raw recording
//!
//!   --frames N           frames (default 150)
//!   --fps F              frame rate, held fixed (default 30)
//!   --out DIR            images and logs (default /tmp/styx-pipeline)
//!   --tuning PATH        tuning (TOML, or Raspberry Pi JSON); default: grey world, built-in AE
//!   --description PATH   sensor description (added to the search path)
//!   --perturb F:K        at frame F force the exposure to K times its value for 10 frames, then
//!                        hand back to AE (a brightness step of 1/K as AE sees it); repeatable
//!   --record BASE        (soft) record the raw frames and their sensor values
//!   --algo-record PATH   record the algorithms' inputs and outputs (styx-algo replay)
//!   --threads N          (soft, replay) software ISP row bands (default 1)
//!   --heap NAME          (soft) capture into buffers from this dma-heap (e.g. linux,cma:
//!                        cached, synced per frame) instead of the driver's MMAP buffers
//!   --no-read            (pisp) do not read the output on the CPU (no per-frame output mean)
//!   --profile            (pisp) time the device calls (queue, dequeue, wait, copies)
//!   --driver-buffers     (pisp) back end outputs in the driver's (uncached) buffers instead of
//!                        cached dma-heap buffers
//!   --quiet              no per-frame lines
//! ```

#[cfg(feature = "device")]
mod device_run;
mod replay_run;
mod report;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use styx_algo::Tuning;

/// Command line.
#[derive(Clone, Debug)]
pub struct Args {
    pub command: String,
    pub frames: usize,
    pub fps: f64,
    pub out: PathBuf,
    pub tuning: Option<PathBuf>,
    pub description: Option<PathBuf>,
    pub perturb: Vec<(u64, f64)>,
    pub record: Option<PathBuf>,
    pub algo_record: Option<PathBuf>,
    pub recording: Option<PathBuf>,
    pub threads: usize,
    pub heap: Option<String>,
    pub quiet: bool,
    pub no_read: bool,
    pub profile: bool,
    pub driver_buffers: bool,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let command = it
        .next()
        .ok_or("usage: native-pipeline pisp|soft|replay [options]")?;
    let mut a = Args {
        command,
        frames: 150,
        fps: 30.0,
        out: PathBuf::from("/tmp/styx-pipeline"),
        tuning: None,
        description: None,
        perturb: Vec::new(),
        record: None,
        algo_record: None,
        recording: None,
        threads: 1,
        heap: None,
        quiet: false,
        no_read: false,
        profile: false,
        driver_buffers: false,
    };
    while let Some(x) = it.next() {
        let mut val = || it.next().ok_or(format!("{x} needs a value"));
        let num = |v: String| v.parse::<f64>().map_err(|e| format!("{x}: {e}"));
        match x.as_str() {
            "--frames" => a.frames = num(val()?)? as usize,
            "--fps" => a.fps = num(val()?)?,
            "--out" => a.out = val()?.into(),
            "--tuning" => a.tuning = Some(val()?.into()),
            "--description" => a.description = Some(val()?.into()),
            "--record" => a.record = Some(val()?.into()),
            "--algo-record" => a.algo_record = Some(val()?.into()),
            "--recording" => a.recording = Some(val()?.into()),
            "--threads" => a.threads = num(val()?)? as usize,
            "--quiet" => a.quiet = true,
            "--no-read" => a.no_read = true,
            "--profile" => a.profile = true,
            "--driver-buffers" => a.driver_buffers = true,
            "--heap" => a.heap = Some(val()?),
            "--perturb" => {
                let v = val()?;
                let (f, k) = v.split_once(':').ok_or("--perturb takes FRAME:FACTOR")?;
                a.perturb.push((
                    f.parse().map_err(|e| format!("--perturb: {e}"))?,
                    k.parse().map_err(|e| format!("--perturb: {e}"))?,
                ));
            }
            _ => return Err(format!("unknown argument {x}")),
        }
    }
    Ok(a)
}

/// The tuning to run with.
pub fn tuning(a: &Args) -> Result<Tuning, String> {
    match &a.tuning {
        Some(p) => Tuning::load(p).map_err(|e| format!("{}: {e}", p.display())),
        None => Ok(Tuning::default()),
    }
}

/// Frames a perturbation holds the exposure for.
pub const PERTURB_FRAMES: u64 = 10;

/// The controls for frame `f`: AE off with the exposure forced to `k` times `base` while a
/// perturbation holds, the defaults otherwise.
pub fn controls_for(
    a: &Args,
    f: u64,
    base: Option<(Duration, f64)>,
) -> Option<styx_algo::Controls> {
    let (_, k) = a
        .perturb
        .iter()
        .find(|(at, _)| f >= *at && f < at + PERTURB_FRAMES)?;
    let (exposure, gain) = base?;
    Some(styx_algo::Controls {
        ae_enable: false,
        exposure: Some(exposure.mul_f64(*k)),
        analogue_gain: Some(gain),
        ..Default::default()
    })
}

/// `CLOCK_MONOTONIC` now (the clock buffer timestamps use).
pub fn monotonic() -> Duration {
    Duration::from_nanos(
        styx_core::prelude::TimestampClock::Monotonic
            .now_ns()
            .unwrap_or(0),
    )
}

static STOP: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

/// SIGINT or SIGTERM arrived: stop and clean up.
pub fn interrupted() -> bool {
    STOP.get().is_some_and(|f| f.load(Ordering::Relaxed))
}

fn main() -> ExitCode {
    let flag = Arc::clone(STOP.get_or_init(|| Arc::new(AtomicBool::new(false))));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register_conditional_shutdown(sig, 1, Arc::clone(&flag));
        let _ = signal_hook::flag::register(sig, Arc::clone(&flag));
    }
    let result = parse().and_then(|a| {
        std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
        match a.command.as_str() {
            "replay" => replay_run::run(&a),
            #[cfg(feature = "device")]
            "pisp" => device_run::pisp(&a),
            #[cfg(feature = "device")]
            "soft" => device_run::soft(&a),
            c => Err(format!(
                "unknown command {c} (pisp and soft need the device feature)"
            )),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("native-pipeline: {e}");
            ExitCode::FAILURE
        }
    }
}
