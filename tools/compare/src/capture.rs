//! One measured capture path: open, start-up window (first frame, AE convergence, exposure
//! settling), steady-state window (timing, CPU, memory), a saved frame.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use styx::prelude::*;
use styx_softisp::CfaPattern;

use crate::image::{ImageStats, image_stats};
use crate::proc::{CpuSnapshot, CpuStats, MemoryStats, memory_stats, system_dmabuf_bytes};
use crate::timing::{
    ExposureSample, FrameSample, Spread, TimingStats, first_converged_ms, settled_ms, timing_stats,
};

/// libcamera control ids read back from request metadata.
pub const LC_AE_STATE: ControlId = ControlId(2);
const LC_EXPOSURE_TIME: ControlId = ControlId(7);
const LC_ANALOGUE_GAIN: ControlId = ControlId(9);
const LC_DIGITAL_GAIN: ControlId = ControlId(28);

/// What to capture and how long to measure.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub label: String,
    pub backend: BackendKind,
    /// Fourcc to capture; `None` picks the first mode of the size.
    pub format: Option<FourCc>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Opens measured for start latency; the last one continues into the steady window.
    pub repeat: usize,
    /// Frames skipped after the start-up window before measuring.
    pub warmup: usize,
    /// Frames measured in the steady window.
    pub frames: usize,
    /// How long the start-up window waits for AE convergence and exposure settling.
    pub converge_timeout: Duration,
    /// Control whose value is an AE state (2 = converged), read back per frame. Defaults to
    /// libcamera `AeState` on the libcamera backend; the native path has none yet.
    pub ae_state_control: Option<ControlId>,
    /// Frames the exposure must stay within 2% for it to count as settled.
    pub settle_window: usize,
    pub cfa: Option<CfaPattern>,
    /// Where to write the saved frame's bytes (planes back to back).
    pub save: Option<std::path::PathBuf>,
}

/// One open: how long to the first frame and to a stable exposure.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StartSample {
    /// `CaptureRequest::start` returning.
    pub start_call_ms: f64,
    /// Open to first frame delivered.
    pub first_frame_ms: f64,
    /// Open to the first frame the AE reports converged.
    pub converged_ms: Option<f64>,
    /// Open to the first frame from which exposure × gain stays within 2%.
    pub settled_ms: Option<f64>,
    pub exposure: Vec<ExposureSample>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StartStats {
    pub first_frame_ms: Option<Spread>,
    pub start_call_ms: Option<Spread>,
    pub converged_ms: Option<Spread>,
    pub settled_ms: Option<Spread>,
    /// Where convergence comes from: `ae-state 0x...`, or `none`.
    pub convergence_source: String,
    /// Where per-frame exposure comes from.
    pub exposure_source: String,
    pub samples: Vec<StartSample>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub label: String,
    pub backend: String,
    pub device: String,
    pub mode: String,
    /// Format of the delivered frames.
    pub delivered_format: String,
    pub residency: String,
    pub requested_fps: f64,
    pub probe_ms: f64,
    pub start: StartStats,
    pub timing: TimingStats,
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub image: Option<ImageStats>,
    pub notes: Vec<String>,
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

fn as_f64(v: &ControlValue) -> Option<f64> {
    match v {
        ControlValue::Int(i) => Some(f64::from(*i)),
        ControlValue::Uint(u) => Some(f64::from(*u)),
        ControlValue::Float(f) => Some(f64::from(*f)),
        ControlValue::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// Exposure of `frame`: from the frame's native metadata, else libcamera's read-back.
fn exposure_of(frame: &FrameLease, handle: &CaptureHandle, cfg: &RunConfig) -> ExposureSample {
    let read = |id| handle.get_control(id).ok().as_ref().and_then(as_f64);
    let product = match frame.meta().native() {
        Some(n) => Some(n.exposure_ns as f64 / 1e3 * f64::from(n.gain())),
        None if cfg.backend == BackendKind::Libcamera => {
            let digital = read(LC_DIGITAL_GAIN).unwrap_or(1.0);
            read(LC_EXPOSURE_TIME)
                .zip(read(LC_ANALOGUE_GAIN))
                .map(|(e, g)| e * g * digital)
        }
        None => None,
    };
    ExposureSample {
        at_ms: 0.0,
        exposure_product_us: product,
        ae_state: cfg.ae_state_control.and_then(read).map(|v| v as i64),
    }
}

fn recv(handle: &CaptureHandle, wait: Duration) -> Result<FrameLease, CaptureError> {
    match handle.recv_blocking(wait) {
        RecvOutcome::Data(f) => Ok(f),
        _ => Err(handle
            .last_error()
            .unwrap_or_else(|| CaptureError::Backend("no frame within the timeout".into()))),
    }
}

/// Opens the capture and runs its start-up window.
fn open(
    device: &ProbedDevice,
    mode: &Mode,
    cfg: &RunConfig,
) -> Result<(CaptureHandle, StartSample), CaptureError> {
    let t0 = Instant::now();
    let interval = Interval::from_fps(cfg.fps)
        .ok_or_else(|| CaptureError::Backend(format!("bad fps {}", cfg.fps)))?;
    let handle = CaptureRequest::new(device)
        .backend(cfg.backend)
        .mode(mode.id.clone())
        .interval(interval)
        .start()?;
    let mut sample = StartSample {
        start_call_ms: ms(t0),
        ..StartSample::default()
    };
    let first = recv(&handle, Duration::from_secs(5))?;
    sample.first_frame_ms = ms(t0);
    let mut frame = first;
    let deadline = t0 + cfg.converge_timeout;
    loop {
        let mut e = exposure_of(&frame, &handle, cfg);
        e.at_ms = ms(t0);
        sample.exposure.push(e);
        sample.converged_ms = first_converged_ms(&sample.exposure);
        sample.settled_ms = settled_ms(&sample.exposure, cfg.settle_window, 0.02);
        let converged = sample.converged_ms.is_some() || cfg.ae_state_control.is_none();
        if (converged && sample.settled_ms.is_some()) || Instant::now() >= deadline {
            break;
        }
        drop(frame);
        frame = recv(&handle, Duration::from_secs(2))?;
    }
    Ok((handle, sample))
}

fn pick_mode(backend: &ProbedBackend, cfg: &RunConfig) -> Option<Mode> {
    let size = |m: &&Mode| {
        m.format.resolution.width.get() == cfg.width
            && m.format.resolution.height.get() == cfg.height
    };
    backend
        .descriptor
        .modes
        .iter()
        .filter(size)
        .find(|m| cfg.format.is_none_or(|f| m.format.code == f))
        .cloned()
}

/// Runs one path and measures it.
pub fn run(cfg: &RunConfig) -> Result<RunResult, CaptureError> {
    let mut result = RunResult {
        label: cfg.label.clone(),
        backend: cfg.backend.to_string(),
        requested_fps: f64::from(cfg.fps),
        ..RunResult::default()
    };
    let t = Instant::now();
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    result.probe_ms = ms(t);
    for e in &probe.errors {
        result.notes.push(format!("probe error: {e}"));
    }
    let Some(device) = probe.devices.iter().find(|d| {
        d.backend(cfg.backend)
            .and_then(|b| pick_mode(b, cfg))
            .is_some()
    }) else {
        return Err(CaptureError::Backend(format!(
            "no {} camera with a {}x{} {} mode",
            cfg.backend,
            cfg.width,
            cfg.height,
            cfg.format.map_or("any".into(), |f| f.to_string())
        )));
    };
    let mode = device
        .backend(cfg.backend)
        .and_then(|b| pick_mode(b, cfg))
        .expect("checked above");
    result.device = device.identity.display.clone();
    result.mode = format!(
        "{} {}x{}",
        mode.format.code, mode.format.resolution.width, mode.format.resolution.height
    );

    let dmabuf_baseline = system_dmabuf_bytes();
    let mut samples = Vec::new();
    let mut handle = None;
    for i in 0..cfg.repeat.max(1) {
        let (h, s) = open(device, &mode, cfg)?;
        samples.push(s);
        if i + 1 == cfg.repeat.max(1) {
            handle = Some(h);
        } else {
            h.stop();
        }
    }
    let handle = handle.expect("at least one open");
    result.start = start_stats(samples, cfg);

    for _ in 0..cfg.warmup {
        drop(recv(&handle, Duration::from_secs(2))?);
    }
    let cpu0 = CpuSnapshot::take();
    let mut frames = Vec::with_capacity(cfg.frames);
    let mut last = None;
    for _ in 0..cfg.frames {
        let f = recv(&handle, Duration::from_secs(2))?;
        frames.push(FrameSample {
            sequence: f.meta().sequence(),
            timestamp_ns: f.meta().timestamp,
        });
        last = Some(f);
    }
    result.cpu = cpu0.until(&CpuSnapshot::take());
    result.memory = memory_stats(dmabuf_baseline);
    result.timing = timing_stats(&frames, f64::from(cfg.fps));
    if let Some(f) = last {
        let meta = f.meta();
        result.delivered_format = format!(
            "{} {}x{}",
            meta.format.code, meta.format.resolution.width, meta.format.resolution.height
        );
        result.residency = meta.residency.map_or("unknown".into(), |r| r.to_string());
        let planes = f.planes();
        let refs: Vec<(&[u8], usize)> = planes.iter().map(|p| (p.data(), p.stride())).collect();
        if refs.iter().all(|(d, _)| d.is_empty()) {
            result
                .notes
                .push("saved frame has no host-readable planes".into());
        }
        result.image = Some(image_stats(
            meta.format.code,
            meta.format.resolution.width.get(),
            meta.format.resolution.height.get(),
            &refs,
            cfg.cfa,
        ));
        if let Some(path) = &cfg.save {
            let bytes: Vec<u8> = refs.iter().flat_map(|(d, _)| d.iter().copied()).collect();
            if let Err(e) = std::fs::write(path, bytes) {
                result.notes.push(format!("saving {}: {e}", path.display()));
            }
        }
    }
    handle.stop();
    Ok(result)
}

fn start_stats(samples: Vec<StartSample>, cfg: &RunConfig) -> StartStats {
    let all = |f: fn(&StartSample) -> Option<f64>| {
        let v: Vec<f64> = samples.iter().filter_map(f).collect();
        Spread::of(&v)
    };
    let exposure_known = samples
        .iter()
        .flat_map(|s| &s.exposure)
        .any(|e| e.exposure_product_us.is_some());
    StartStats {
        first_frame_ms: all(|s| Some(s.first_frame_ms)),
        start_call_ms: all(|s| Some(s.start_call_ms)),
        converged_ms: all(|s| s.converged_ms),
        settled_ms: all(|s| s.settled_ms),
        convergence_source: cfg
            .ae_state_control
            .map_or("none".into(), |c| format!("ae-state {:#x}", c.0)),
        exposure_source: match (cfg.backend, exposure_known) {
            (_, false) => "none".into(),
            (BackendKind::Libcamera, true) => "libcamera metadata read-back".into(),
            (_, true) => "frame metadata".into(),
        },
        samples,
    }
}
