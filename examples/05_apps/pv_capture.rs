//! The reference flow for a PhotonVision-style driver (a JNI crate, or any other host): camera
//! selection, a grey stream with pyramid levels, per-frame access, JPEG previews, runtime
//! controls and a clean stop, through the Styx API only.
//!
//! The target is a Raspberry Pi CM5 with an OV9782 whose stock `ov9782` kernel driver stays
//! bound (kernel-driven sensor mode, `--kernel-sensor`): Styx sets the sensor through its V4L2
//! subdev controls and drives the PiSP ISP with its own 3A. Frames are grey (the luma plane of
//! the processed `NV12` output), so a detector reads the same bytes the sensor produced.
//!
//! ```text
//! cargo run --release -p styx-examples --features native,preview,codec-turbojpeg \
//!     --bin pv_capture -- --kernel-sensor --size 1280x800 --fps 30 --seconds 10 \
//!     --exposure-us 8000 --gain 2 --awb auto --save-jpeg /tmp/pv
//!
//! # host smoke test, with the virtual camera (no sensor; the frames are synthetic):
//! cargo run -p styx-examples --features native,preview,codec-turbojpeg --bin pv_capture -- \
//!     --virtual --frames 60
//! ```
//!
//! ## JNI mapping
//!
//! The JNI crate copies these calls (Rust names; the Java method is the first column).
//!
//! | PhotonVision call | Styx API, in order |
//! |---|---|
//! | `createCamera(name)` | `styx::probe_all_with_errors_with_config(&StyxConfig::default())`, then pick the device whose `identity.display` is `name` (a native camera is `BackendKind::Native`; `properties` has `backend = kernel` for a kernel-driven sensor) |
//! | `startCamera(size, fps)` | `Frames::gray().size_at_most(w, h).fps_at_least(fps).pyramid(2).plan_best(&devices)?` (or `.open_best(&devices)`), then `plan.start()?` for a `Frames` |
//! | `awaitNewFrame(timeout)` | `frames.next_frame(timeout)`: `RecvOutcome::Data(lease)`; `Empty` is a timeout, `Closed` is the end |
//! | grey frame (`getGreyFrame`) | `lease.luma_rows()`: `row(y)?.data()` per row, `stride()`, `visible_len()`; level 1 and 2 of the same instant: `lease.pyramid_level(1)`, `lease.pyramid_level(2)` (same timestamp) |
//! | `takeColorFrame` / JPEG | `preview.offer(&lease)` (a JPEG at `PreviewConfig`'s size and rate cap), then `subscriber.recv(timeout)` gives `PreviewFrame::jpeg` |
//! | `getFrameCaptureTime` | `lease.meta().timestamp_in(TimestampClock::Boottime)` (the clock `FrameMeta::clock` says; the raw one is `lease.meta().timestamp`) |
//! | `setExposure(us)` / `setGain(g)` / `setAwb(on)` / `setColourTemperature(k)` | `frames.standard_controls().set_exposure_us(us)`, `set_gain(g)`, `set_awb(on)`, `set_colour_temperature(k)`: each returns `AppliedControl` (value in effect, `clamped`, `frame` the change lands on) |
//! | `release frame` | drop the `FrameLease` (at once: see the buffer rule in `main`) |
//! | `stop()` | `frames.stop()`, then `preview.stop()` (the subscribers end) |
//!
//! Processed (grey, `NV12`) frames run the 3A loop: `set_exposure_us` and `set_gain` fix AE at
//! that value, `set_ae(true)` hands it back. Changing the frame rate of a processed capture is an
//! error (restart at the new rate). See `docs/native-stack/pipeline.md#controls-through-the-styx-api`
//! and `docs/preview.md`.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use styx::capture_api::{make_virtual_device_with_controls, native_controls};
use styx::prelude::*;
use styx::preview::{Preview, PreviewConfig, PreviewSubscriber};

/// Set by SIGINT: the loop ends, the preview and the capture stop cleanly.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

const USAGE: &str = "\
pv_capture: a PhotonVision-style grey stream, JPEG previews and runtime controls

  --camera NAME|INDEX      the native camera (probe display name, or its index among the native cameras)
  --kernel-sensor          require a kernel-driven native sensor (stock ov9782 bound); fail otherwise
  --virtual                use the virtual camera (host smoke test; synthetic frames)
  --size WxH               grey frame size, at most (default 1280x800)
  --fps N                  frame rate, at least (default 30)
  --frames N               stop after N frames
  --seconds S              stop after S seconds (default 10 when neither is given)
  --exposure-us US         exposure at start (fixes AE on processed modes)
  --gain G                 total gain at start
  --awb auto|K             automatic white balance, or a fixed colour temperature in kelvin
  --jpeg-size WxH          preview size (default 640x400)
  --jpeg-fps N             preview rate cap (default 15)
  --jpeg-quality Q         JPEG quality 1-100 (default 70)
  --save-jpeg DIR          write the first --save-count JPEGs to DIR
  --save-count N           how many JPEGs to save (default 3)
  -h, --help               this text

With --camera or --kernel-sensor the camera must exist; without either, the first native camera
is used, or the virtual one when there is none.";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Awb {
    Auto,
    Kelvin(u32),
}

#[derive(Debug)]
struct Args {
    camera: Option<String>,
    kernel_sensor: bool,
    virtual_camera: bool,
    size: (u32, u32),
    fps: u32,
    frames: Option<u64>,
    seconds: Option<f64>,
    exposure_us: Option<u32>,
    gain: Option<f32>,
    awb: Option<Awb>,
    jpeg_size: (u32, u32),
    jpeg_fps: u32,
    jpeg_quality: u8,
    save_jpeg: Option<PathBuf>,
    save_count: usize,
}

fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| format!("'{text}': expected WxH"))?;
    let w: u32 = w.parse().map_err(|_| format!("'{text}': bad width"))?;
    let h: u32 = h.parse().map_err(|_| format!("'{text}': bad height"))?;
    if w == 0 || h == 0 {
        return Err(format!("'{text}': the size must be non-zero"));
    }
    Ok((w, h))
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        camera: None,
        kernel_sensor: false,
        virtual_camera: false,
        size: (1280, 800),
        fps: 30,
        frames: None,
        seconds: None,
        exposure_us: None,
        gain: None,
        awb: None,
        jpeg_size: (640, 400),
        jpeg_fps: 15,
        jpeg_quality: 70,
        save_jpeg: None,
        save_count: 3,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--camera" => a.camera = Some(value()?),
            "--kernel-sensor" => a.kernel_sensor = true,
            "--virtual" => a.virtual_camera = true,
            "--size" => a.size = parse_size(&value()?)?,
            "--fps" => a.fps = value()?.parse().map_err(|_| "--fps: a number")?,
            "--frames" => a.frames = Some(value()?.parse().map_err(|_| "--frames: a number")?),
            "--seconds" => a.seconds = Some(value()?.parse().map_err(|_| "--seconds: a number")?),
            "--exposure-us" => {
                a.exposure_us = Some(value()?.parse().map_err(|_| "--exposure-us: a number")?);
            }
            "--gain" => a.gain = Some(value()?.parse().map_err(|_| "--gain: a number")?),
            "--awb" => {
                let v = value()?;
                a.awb = Some(match v.as_str() {
                    "auto" => Awb::Auto,
                    k => Awb::Kelvin(k.parse().map_err(|_| "--awb: auto or kelvin")?),
                });
            }
            "--jpeg-size" => a.jpeg_size = parse_size(&value()?)?,
            "--jpeg-fps" => a.jpeg_fps = value()?.parse().map_err(|_| "--jpeg-fps: a number")?,
            "--jpeg-quality" => {
                a.jpeg_quality = value()?.parse().map_err(|_| "--jpeg-quality: 1-100")?;
            }
            "--save-jpeg" => a.save_jpeg = Some(PathBuf::from(value()?)),
            "--save-count" => {
                a.save_count = value()?.parse().map_err(|_| "--save-count: a number")?;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (see --help)")),
        }
    }
    if a.virtual_camera && a.kernel_sensor {
        return Err("--virtual and --kernel-sensor exclude each other".into());
    }
    if a.frames.is_none() && a.seconds.is_none() {
        a.seconds = Some(10.0);
    }
    Ok(a)
}

/// A native camera's property (`backend`: `kernel` or `bridge`; `isp`, ...).
fn native_property<'a>(device: &'a ProbedDevice, key: &str) -> Option<&'a str> {
    device
        .backends
        .iter()
        .filter(|b| b.kind == BackendKind::Native)
        .flat_map(|b| b.properties.iter())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn is_native(device: &ProbedDevice) -> bool {
    device
        .backends
        .iter()
        .any(|b| b.kind == BackendKind::Native)
}

fn is_kernel_driven(device: &ProbedDevice) -> bool {
    native_property(device, "backend") == Some("kernel")
}

/// The virtual camera: a 1280x800 grey mode at 30 fps, with native-named controls so the
/// control path runs as on a native camera. Its frames are synthetic: zero-filled, and the
/// virtual source keeps 3 bytes per pixel, so the stride is 3x the width and the mean luma is
/// 0. It checks the flow, not the image.
fn virtual_camera() -> ProbedDevice {
    let res = Resolution::new(1280, 800).expect("non-zero");
    let mode = Mode::with_interval(
        MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown),
        Interval::from_fps(30).expect("30 fps"),
    );
    let meta = |id, name: &str, kind, min, max, default| ControlMeta {
        id,
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        min,
        max,
        default,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    };
    let controls = vec![
        meta(
            native_controls::EXPOSURE_TIME_US,
            "exposure_time_us",
            ControlKind::Uint,
            ControlValue::Uint(100),
            ControlValue::Uint(100_000),
            ControlValue::Uint(10_000),
        ),
        meta(
            native_controls::GAIN,
            "gain",
            ControlKind::Float,
            ControlValue::Float(1.0),
            ControlValue::Float(16.0),
            ControlValue::Float(1.0),
        ),
        meta(
            native_controls::AE_ENABLE,
            "ae_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            native_controls::AWB_ENABLE,
            "awb_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            native_controls::COLOUR_TEMPERATURE,
            "colour_temperature",
            ControlKind::Uint,
            ControlValue::Uint(2_000),
            ControlValue::Uint(10_000),
            ControlValue::Uint(5_000),
        ),
    ];
    make_virtual_device_with_controls("pv-virtual", [mode], controls)
}

/// The device to open, and why it was chosen.
fn choose_camera(args: &Args, probed: &[ProbedDevice]) -> Result<(ProbedDevice, String), String> {
    if args.virtual_camera {
        return Ok((virtual_camera(), "the virtual camera (--virtual)".into()));
    }
    let natives: Vec<&ProbedDevice> = probed.iter().filter(|d| is_native(d)).collect();
    if args.kernel_sensor {
        let kernel: Vec<&ProbedDevice> = natives
            .iter()
            .copied()
            .filter(|d| is_kernel_driven(d))
            .collect();
        let bridged: Vec<&str> = natives
            .iter()
            .filter(|d| !is_kernel_driven(d))
            .map(|d| d.identity.display.as_str())
            .collect();
        let pick = match &args.camera {
            Some(sel) => kernel.iter().find(|d| d.identity.display == *sel).copied(),
            None => kernel.first().copied(),
        };
        return match pick {
            Some(device) => Ok((device.clone(), "a kernel-driven native sensor".into())),
            None if !bridged.is_empty() => Err(format!(
                "--kernel-sensor: no kernel-driven sensor (the stock driver bound); the native cameras found are bridged ({}): drop --kernel-sensor to use the bridge",
                bridged.join(", ")
            )),
            None => Err("--kernel-sensor: no native camera at all (is the ov9782 kernel driver bound, and STYX_SENSOR_PATH set?)".into()),
        };
    }
    if let Some(sel) = &args.camera {
        let pick = match sel.parse::<usize>() {
            Ok(index) => natives.get(index).copied(),
            Err(_) => natives.iter().copied().find(|d| d.identity.display == *sel),
        };
        return pick
            .map(|d| (d.clone(), "the --camera choice".into()))
            .ok_or_else(|| {
                let names: Vec<&str> = natives
                    .iter()
                    .map(|d| d.identity.display.as_str())
                    .collect();
                format!("--camera {sel}: no such native camera (found: {names:?})")
            });
    }
    match natives.first() {
        Some(device) => Ok((
            (*device).clone(),
            if is_kernel_driven(device) {
                "the first native camera (kernel-driven)".into()
            } else {
                "the first native camera (bridged)".into()
            },
        )),
        None => Ok((virtual_camera(), "no native camera: the virtual one".into())),
    }
}

/// Gaps between consecutive frame timestamps, in nanoseconds.
#[derive(Default)]
struct Deltas {
    count: u64,
    sum: u128,
    min: u64,
    max: u64,
}

impl Deltas {
    fn add(&mut self, ns: u64) {
        if self.count == 0 {
            self.min = ns;
        }
        self.count += 1;
        self.sum += u128::from(ns);
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
    }

    /// (mean, min, max) in nanoseconds.
    fn summary(&self) -> Option<(f64, f64, f64)> {
        (self.count > 0).then(|| {
            (
                self.sum as f64 / self.count as f64,
                self.min as f64,
                self.max as f64,
            )
        })
    }
}

/// Mean of every fourth row of the grey plane: a cheap proof that the bytes are readable.
fn mean_luma(lease: &FrameLease) -> Option<f64> {
    let rows = lease.luma_rows().ok()?;
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (0..rows.len()).step_by(4) {
        let row = rows.row(y)?;
        sum += row.data().iter().map(|&v| u64::from(v)).sum::<u64>();
        n += row.data().len() as u64;
    }
    (n > 0).then(|| sum as f64 / n as f64)
}

/// The viewer's thread: counts the JPEGs it receives and writes the first `save` of them to
/// `dir`. It ends when `done` is set (after the preview has stopped).
fn spawn_viewer(
    mut viewer: PreviewSubscriber,
    dir: Option<PathBuf>,
    save: usize,
    done: Arc<AtomicBool>,
) -> thread::JoinHandle<(u64, u64, usize)> {
    thread::spawn(move || {
        let (mut count, mut bytes, mut saved) = (0u64, 0u64, 0usize);
        while !done.load(Ordering::Acquire) {
            let Some(frame) = viewer.recv(Duration::from_millis(200)) else {
                continue;
            };
            count += 1;
            bytes += frame.jpeg.len() as u64;
            if let Some(dir) = dir.as_ref().filter(|_| saved < save) {
                let path = dir.join(format!("pv-capture-{:04}.jpg", frame.sequence));
                match std::fs::write(&path, &frame.jpeg[..]) {
                    Ok(()) => {
                        println!("  saved {} ({} bytes)", path.display(), frame.jpeg.len());
                        saved += 1;
                    }
                    Err(e) => eprintln!("  {}: {e}", path.display()),
                }
            }
        }
        (count, bytes, saved)
    })
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    // SIGINT ends the loop, then the capture and the preview stop.
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }

    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    for e in &probe.errors {
        println!("probe: {e}");
    }
    let (device, why) = choose_camera(&args, &probe.devices)?;
    println!(
        "camera: {} ({why}); backend {}",
        device.identity.display,
        device
            .backends
            .first()
            .map_or("none".to_string(), |b| format!("{:?}", b.kind))
    );
    let devices = [device];

    // The grey stream: the 3A-processed luma at the requested size and rate, with the two
    // pyramid levels (1/2 and 1/4 of the frame) from the same instant.
    let request = Frames::gray()
        .size_at_most(args.size.0, args.size.1)
        .fps_at_least(args.fps)
        .pyramid(2);
    let plan = request.plan_best(&devices)?;
    print!("{plan}");
    let mut frames = plan.start()?;

    let controls = frames.standard_controls();
    if let Some(us) = args.exposure_us {
        let a = controls.set_exposure_us(us)?;
        println!(
            "set exposure {us} us: in effect {:?}, clamped {}",
            a.value, a.clamped
        );
    }
    if let Some(g) = args.gain {
        let a = controls.set_gain(g)?;
        println!(
            "set gain {g}: in effect {:?}, clamped {}",
            a.value, a.clamped
        );
    }
    match args.awb {
        Some(Awb::Auto) => {
            controls.set_awb(true)?;
            println!("awb: auto");
        }
        Some(Awb::Kelvin(k)) => {
            controls.set_awb(false)?;
            let a = controls.set_colour_temperature(k)?;
            println!("awb: fixed, colour temperature {:?}", a.value);
        }
        None => {}
    }

    // JPEG previews: a low-priority encoder of the frames we offer, at its own rate cap.
    let preview = Preview::new(
        PreviewConfig::new()
            .name("pv_capture")
            .size(args.jpeg_size.0, args.jpeg_size.1)
            .max_fps(args.jpeg_fps as f32)
            .quality(args.jpeg_quality),
    )?;
    if let Some(dir) = &args.save_jpeg {
        std::fs::create_dir_all(dir)?;
    }
    let viewer_done = Arc::new(AtomicBool::new(false));
    let viewer = spawn_viewer(
        preview.subscribe(),
        args.save_jpeg.clone(),
        args.save_count,
        viewer_done.clone(),
    );

    let started = Instant::now();
    let mut mid_run_done = false;
    let mut pending_landing: Option<u64> = None;
    let mut seen = 0u64;
    let mut gaps = 0u64;
    let mut last_seq: Option<u32> = None;
    let mut last_mono: Option<u64> = None;
    let (mut mono_min, mut mono_max) = (u64::MAX, 0u64);
    let mut deltas = Deltas::default();
    let mut clock_offset: Option<(i128, i128, i128)> = None;
    let mut luma_sizes: Vec<(u32, u32, usize)> = Vec::new();
    let mut timeouts = 0u64;
    let mut luma_sum = 0.0f64;
    let mut luma_n = 0u64;

    'run: loop {
        if STOP.load(Ordering::Relaxed) {
            println!("interrupted");
            break;
        }
        if args.frames.is_some_and(|n| seen >= n)
            || args
                .seconds
                .is_some_and(|s| started.elapsed().as_secs_f64() >= s)
        {
            break;
        }
        // Once, two seconds in: a control change while frames flow, with the frame it lands on.
        if !mid_run_done && started.elapsed() >= Duration::from_secs(2) {
            mid_run_done = true;
            let base = args.exposure_us.unwrap_or(10_000);
            let next = (base / 2).max(1_000);
            let a = controls.set_exposure_us(next)?;
            println!(
                "mid-run: exposure {base} -> {next} us, in effect {:?}, lands on sensor frame {:?}",
                a.value, a.frame
            );
            pending_landing = a.frame;
        }

        let lease = match frames.next_frame(Duration::from_millis(500)) {
            RecvOutcome::Data(lease) => lease,
            RecvOutcome::Empty => {
                timeouts += 1;
                continue 'run;
            }
            RecvOutcome::Closed => {
                println!("the capture closed");
                break;
            }
        };
        seen += 1;

        // Everything the frame carries, read now.
        let meta = lease.meta();
        // Native cameras number their frames; the virtual one does not (gaps stay 0 there).
        let seq = meta.sequence();
        if let (Some(seq), Some(prev)) = (seq, last_seq) {
            gaps += u64::from(seq.wrapping_sub(prev).saturating_sub(1));
        }
        last_seq = seq.or(last_seq);
        let native = match &meta.backend {
            Some(BackendFrameMeta::Native(n)) => Some(*n),
            _ => None,
        };
        let raw_mono = meta.timestamp;
        if let Some(prev) = last_mono {
            deltas.add(raw_mono.saturating_sub(prev));
        }
        last_mono = Some(raw_mono);
        mono_min = mono_min.min(raw_mono);
        mono_max = mono_max.max(raw_mono);
        let boot = meta.timestamp_in(TimestampClock::Boottime);
        if let Some(boot) = boot {
            let offset = i128::from(boot) - i128::from(raw_mono);
            clock_offset = Some(match clock_offset {
                None => (offset, offset, offset),
                Some((first, lo, hi)) => (first, lo.min(offset), hi.max(offset)),
            });
        }
        if luma_sizes.is_empty() {
            for level in 0..=2u8 {
                let frame = if level == 0 {
                    Some(&lease)
                } else {
                    lease.pyramid_level(level)
                };
                if let Some(f) = frame
                    && let Ok(rows) = f.luma_rows()
                {
                    let res = f.meta().format.resolution;
                    luma_sizes.push((res.width.get(), res.height.get(), rows.stride()));
                }
            }
        }
        let mean = mean_luma(&lease);
        if let Some(m) = mean {
            luma_sum += m;
            luma_n += 1;
        }
        if let Some(landing) = pending_landing
            && native.is_some_and(|n| u64::from(n.sequence) >= landing)
        {
            let n = native.expect("checked");
            println!(
                "  landed: sensor frame {} uses exposure {} us, gain {:.2}",
                n.sequence,
                n.exposure_ns / 1_000,
                n.analog_gain
            );
            pending_landing = None;
        }

        if seen <= 3 || seen.is_multiple_of(30) {
            println!(
                "frame seq={} t_mono={:.3}s t_boot={} mean_luma={} exposure={} gain={}",
                seq.map_or("n/a".to_string(), |s| s.to_string()),
                raw_mono as f64 / 1e9,
                boot.map_or("n/a".to_string(), |b| format!("{:.3}s", b as f64 / 1e9)),
                mean.map_or("n/a".to_string(), |m| format!("{m:.1}")),
                native.map_or("n/a".to_string(), |n| format!(
                    "{} us",
                    n.exposure_ns / 1_000
                )),
                native.map_or("n/a".to_string(), |n| format!("{:.2}", n.analog_gain)),
            );
        }
        // Hand the frame to the preview and let go of it here. `into_shareable` moves the
        // buffers into shared ownership without copying (the preview's share keeps them until
        // the JPEG is encoded, or the offer is refused and the frame is dropped at once).
        //
        // The 6-buffer rule: the PiSP's output buffers (6 per output by default,
        // `StyxConfig::native_output_buffers`) are shared with every consumer. A frame held
        // past the next capture keeps one of them, and when every buffer of an output is held
        // the ISP drops the next frame (`OutputsHeld`, `drops.isp_skipped`) instead of waiting.
        // So copy what you need out of a lease, then release it.
        preview.offer_owned(lease.into_shareable());
    }
    let elapsed = started.elapsed().as_secs_f64();

    // Stop: the capture first (its frames end the loop of the viewer), then the preview, whose
    // stop ends the subscriber.
    let drops = frames.dropped();
    frames.stop();
    let preview_metrics = preview.metrics();
    preview.stop();
    viewer_done.store(true, Ordering::Release);
    let (jpegs, jpeg_bytes, saved) = viewer.join().unwrap_or((0, 0, 0));

    println!("\nsummary");
    println!(
        "  frames {seen} in {elapsed:.2} s = {:.2} fps; sequence gaps {gaps}; consumer drops {drops}; poll timeouts {timeouts}",
        seen as f64 / elapsed.max(1e-9),
    );
    if let Some((mean, min, max)) = deltas.summary() {
        println!(
            "  frame interval (monotonic timestamps): mean {:.2} ms, min {:.2} ms, max {:.2} ms",
            mean / 1e6,
            min / 1e6,
            max / 1e6
        );
    }
    if let Some((first, lo, hi)) = clock_offset {
        println!(
            "  timestamps: monotonic {:.3}..{:.3} s; boottime - monotonic {first} ns at the first frame, range {}..{} ns",
            mono_min as f64 / 1e9,
            mono_max as f64 / 1e9,
            lo,
            hi
        );
    }
    for (level, (w, h, stride)) in luma_sizes.iter().enumerate() {
        println!("  luma level {level}: {w}x{h}, stride {stride} bytes");
    }
    if luma_n > 0 {
        println!("  mean luma over the run: {:.1}", luma_sum / luma_n as f64);
    }
    println!(
        "  jpeg: {jpegs} received, {jpeg_bytes} bytes ({} saved); encode p50 {} ms over {} frames; offered {}, rate-capped {}, busy {}, errors {}",
        saved,
        preview_metrics
            .encode
            .p50_ms
            .map_or("n/a".to_string(), |v| format!("{v:.2}")),
        preview_metrics.encoded,
        preview_metrics.frames_in,
        preview_metrics.dropped_rate,
        preview_metrics.dropped_busy,
        preview_metrics.errors,
    );
    Ok(())
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("pv_capture: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("pv_capture: {e}");
        std::process::exit(1);
    }
}
