//! The PiSP processed path on a host, without a device: a frame's front end statistics (the raw
//! copy the device's dequeue makes, parsed into the algorithms' statistics), the algorithms
//! and their requests (`Algorithms::process`), the back end config of every frame (the
//! `BackEnd` submit of the device loop: `BeConfigBuilder::update_frame`), and the extra
//! passes' configs, over many frames of a scene, counted with a thread-local allocator.
//!
//! Only the device's calls (the statistics read, the job queue and wait, the sensor's control
//! writes) stand in; everything the loop computes and builds per frame is the real code.
//! `STYX_PISP_ALLOC_TRACE=1` prints each steady-state allocation's call stack (symbolised), to
//! name the call site of what remains.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use styx_algo::{Statistics, Tuning};
use styx_pipeline::Result;
use styx_pipeline::controller::{Controller, SensorValues};
use styx_pipeline::isp::IspSettings;
use styx_pipeline::pisp_be::BeConfigBuilder;
use styx_pipeline::pisp_passes::{PassConfigs, PassSpec, PassTdn};
use styx_pipeline::process::{Algorithms, FrameIsp, InlineIsp, NoControls};
use styx_pipeline::{SensorInfo, stats};
use styx_pisp::fe::FrontEnd;
use styx_pisp::format::{compute_stride_align, formats};
use styx_pisp::uapi::{
    AWB_STATS_NUM_ZONES, BayerOrder, BeCropConfig, ImageFormatConfig, RawStatistics,
};
use styx_sensor::SensorDescription;

// --- the counting allocator -------------------------------------------------------------------

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        trace::record();
    }
}

// SAFETY: forwards to the system allocator unchanged; the bookkeeping only touches
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Allocations on this thread while `f` runs.
fn counted<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = COUNT.with(Cell::get);
    let v = f();
    (v, COUNT.with(Cell::get) - before)
}

/// Each steady-state allocation's stack (`STYX_PISP_ALLOC_TRACE=1`): raw return addresses, a
/// fixed table, symbolised with `addr2line` at the end.
mod trace {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::Mutex;

    const DEPTH: usize = 40;
    const MAX: usize = 4096;

    thread_local! {
        static INSIDE: Cell<bool> = const { Cell::new(false) };
    }

    static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static STACKS: Mutex<Vec<[usize; DEPTH]>> = Mutex::new(Vec::new());

    #[cfg(target_env = "gnu")]
    unsafe extern "C" {
        fn backtrace(buffer: *mut *mut c_void, size: std::ffi::c_int) -> std::ffi::c_int;
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> std::ffi::c_int;
    }

    #[cfg(target_env = "gnu")]
    #[repr(C)]
    struct DlInfo {
        fname: *const std::ffi::c_char,
        fbase: *mut c_void,
        sname: *const std::ffi::c_char,
        saddr: *mut c_void,
    }

    pub fn init() {
        let on = cfg!(target_env = "gnu")
            && std::env::var_os("STYX_PISP_ALLOC_TRACE").is_some_and(|v| v != "0");
        if on {
            let mut warm = [0usize; 4];
            // SAFETY: `warm` holds that many words; glibc loads its unwinder here.
            unsafe { backtrace(warm.as_mut_ptr().cast(), 4) };
        }
        ON.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn record() {
        if !ON.load(std::sync::atomic::Ordering::Relaxed)
            || INSIDE.try_with(|c| c.replace(true)).unwrap_or(true)
        {
            return;
        }
        let mut row = [0usize; DEPTH];
        #[cfg(target_env = "gnu")]
        // SAFETY: `row` holds `DEPTH` words.
        unsafe {
            backtrace(row.as_mut_ptr().cast(), DEPTH as std::ffi::c_int)
        };
        if let Ok(mut s) = STACKS.try_lock()
            && s.len() < MAX
        {
            s.push(row);
        }
        let _ = INSIDE.try_with(|c| c.set(false));
    }

    pub fn finish() {
        let stacks = std::mem::take(&mut *STACKS.lock().unwrap());
        if stacks.is_empty() {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let mut groups: Vec<(Vec<usize>, usize)> = Vec::new();
        for s in &stacks {
            let st: Vec<usize> = s.iter().copied().take_while(|&a| a != 0).collect();
            match groups.iter_mut().find(|g| g.0 == st) {
                Some(g) => g.1 += 1,
                None => groups.push((st, 1)),
            }
        }
        groups.sort_by_key(|g| std::cmp::Reverse(g.1));
        let per_frame = std::env::var("STYX_PISP_ALLOC_FRAMES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.0);
        for (st, n) in groups.iter() {
            let mut names = Vec::new();
            for a in st.iter() {
                let mut info = DlInfo {
                    fname: std::ptr::null(),
                    fbase: std::ptr::null_mut(),
                    sname: std::ptr::null(),
                    saddr: std::ptr::null_mut(),
                };
                // SAFETY: `info` is a valid out-parameter.
                if unsafe { dladdr(*a as *const c_void, &mut info) } == 0 {
                    continue;
                }
                let off = *a - info.fbase as usize;
                let out = std::process::Command::new("addr2line")
                    .args(["-f", "-C", "-p", "-e"])
                    .arg(&exe)
                    .arg(format!("{:#x}", off - 1))
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default();
                let t = out.trim();
                if t.contains("styx_") && !t.contains("pisp_alloc") {
                    let short: String = t
                        .split(" at ")
                        .next()
                        .unwrap_or(t)
                        .chars()
                        .take(160)
                        .collect();
                    names.push(short);
                }
                if names.len() >= 5 {
                    break;
                }
            }
            eprintln!("=== {:.2} per frame", *n as f64 / per_frame);
            for t in names {
                eprintln!("    {t}");
            }
        }
    }
}

// --- the scene and the stand-ins for the device ------------------------------------------------

const FRAMES: u64 = 240;
const WARM: u64 = 60;
/// Frames with name-bearing controls set (metering, constraint and AWB mode names).
const MODE_WINDOW: (u64, u64) = (150, 201);

fn fmt(w: u16, h: u16, format: u32) -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: w,
        height: h,
        format,
        ..Default::default()
    };
    compute_stride_align(&mut f, 64);
    f
}

fn info() -> SensorInfo {
    let desc = SensorDescription::from_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sensor/sensors/ov9782.toml"
    ))
    .unwrap();
    SensorInfo::from_description(&desc, "1280x800", "raw10")
        .unwrap()
        .with_fps(30.0, 30.0)
        .unwrap()
}

/// The front end statistics of frame `n` for a scene of light `light` at `exposure` × `gain`
/// (a grey scene with a warm cast; the device copies them out at the dequeue, here they are
/// written in place).
fn fill_stats(raw: &mut RawStatistics, n: u64, light: f64, exposure: f64) {
    let _ = n;
    let level = (light * exposure / 0.01).clamp(0.0, 0.95);
    for (i, z) in raw.awb.zones[..AWB_STATS_NUM_ZONES].iter_mut().enumerate() {
        let k = 1.0 + (i % 7) as f64 * 0.01;
        z.r_sum = (level * 0.8 * 65536.0 * k) as u32;
        z.g_sum = (level * 65536.0) as u32;
        z.b_sum = (level * 0.5 * 65536.0 / k) as u32;
        z.counted = 1024;
    }
    for (i, b) in raw.agc.histogram.iter_mut().enumerate() {
        let d = (i as f64 / 1024.0 - level).abs();
        *b = (4000.0 * (-d * 30.0).exp()) as u32;
    }
    for (i, f) in raw.cdaf.foms.iter_mut().enumerate() {
        *f = 1000 + i as u64;
    }
}

/// The device loop's back end as a [`FrameIsp`]: the config patched for the frame, the job
/// (stood in) finished at once.
struct BackEnd {
    builder: BeConfigBuilder,
}

impl FrameIsp for BackEnd {
    type Input<'a> = u32;
    type Job = u32;
    type Output = u32;

    fn submit(
        &mut self,
        index: u32,
        settings: &IspSettings,
        values: &SensorValues,
        _statistics: bool,
    ) -> Result<u32> {
        let exposure = values.exposure.as_secs_f64() * values.analogue_gain * settings.flicker;
        self.builder.update_frame(settings, exposure)?;
        Ok(index)
    }
    fn finish(&mut self, job: u32) -> Result<u32> {
        Ok(job)
    }
}

/// The front end as an [`InlineIsp`]: the statistics parsed into the algorithms' statistics
/// (the device's `FeStats`), the settings applied to the front end's next config.
struct FrontEndStats<'a> {
    raw: &'a RawStatistics,
    fe: &'a mut FrontEnd,
}

impl InlineIsp for FrontEndStats<'_> {
    fn statistics(&mut self, out: &mut Statistics) -> bool {
        stats::from_pisp_raw(self.raw, out);
        true
    }
    fn set_settings(&mut self, settings: &IspSettings) {
        settings.apply_fe(self.fe);
    }
}

#[derive(Default)]
struct Totals {
    /// Steady-state allocations of a frame (the algorithms, the statistics, the back end's
    /// config) and of the extra passes' configs, outside the mode window.
    frame: u64,
    /// Allocations in the mode window (name-bearing controls set: `Controller::meta` copies
    /// the names).
    window: u64,
    passes: u64,
}

/// A hash of a frame's outputs (their `Debug` text, which prints every float exactly).
fn digest(parts: &[&dyn std::fmt::Debug]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        format!("{p:?}").hash(&mut h);
    }
    h.finish()
}

#[test]
#[allow(clippy::print_stdout)]
fn the_pisp_processed_path_allocates_per_frame() {
    trace::init();
    let info = info();
    // The tuning the device loads for this sensor (`sensor.toml`'s `tuning` file).
    let tuning = Tuning::load(concat!(env!("CARGO_MANIFEST_DIR"), "/tuning/ov9782.json")).unwrap();
    let mut algo = Algorithms::new(
        Controller::new(&tuning, info.camera.clone()).unwrap(),
        info.black_level,
    );
    let start = algo.start(&NoControls).unwrap();
    let mut fe = FrontEnd::new(info.width as u16, info.height as u16, BayerOrder::Rggb);
    fe.default_stats(styx_pipeline::isp::level16(info.black_level), 1.0, 1.0);
    let input = fmt(info.width as u16, info.height as u16, formats::BAYER16);
    fe.set_output_format(0, input);
    let outputs = [
        Some(fmt(1280, 800, formats::NV12)),
        Some(fmt(640, 400, formats::RGB888)),
    ];
    let template =
        styx_pipeline::isp::be_template(input, BayerOrder::Rggb, info.black_level, outputs)
            .unwrap();
    let mut be = BackEnd {
        builder: BeConfigBuilder::new(template).unwrap(),
    };
    let mut passes = PassConfigs::new(outputs, PassTdn::Read);
    let region = PassSpec {
        crop: BeCropConfig {
            offset_x: 256,
            offset_y: 192,
            width: 512,
            height: 384,
        },
        output: 1,
        size: Some((320, 240)),
        format: None,
    };
    passes.set(0, Some(region), &be.builder).unwrap();
    let mut raw = bytemuck::allocation::zeroed_box::<RawStatistics>();
    let mut totals = Totals::default();
    let mut last_seq: Option<u64> = None;
    let mut sum = 0u64;
    // The scene's light drifts; exposure and gain follow the algorithms' sensor requests, as
    // the device's control schedule applies them.
    let (mut exposure, mut gain, mut frame_duration) = (
        Duration::from_millis(10),
        start.sensor.map_or(1.0, |r| r.analogue_gain),
        Duration::from_nanos(33_333_300),
    );
    let dump = std::env::var_os("STYX_PISP_DUMP").map(|p| std::fs::File::create(p).unwrap());
    let mut dump = dump.map(std::io::BufWriter::new);
    ARMED.with(|a| a.set(false));
    for n in 0..FRAMES {
        let steady = n >= WARM;
        let run = algo.due(last_seq.map(|s| s + 1));
        let light = 0.25 + 0.2 * ((n as f64) / 25.0).sin();
        // A change of mode for a stretch of frames: the metering, constraint and AWB names
        // the tuning has (the string paths of the algorithms).
        if n == 150 || n == 200 {
            let mut c = algo.controller_ref().controls().clone();
            if n == 150 {
                c.metering_mode = Some("spot".into());
                c.constraint_mode = Some("highlight".into());
                c.awb_mode = Some("tungsten".into());
            } else {
                c.metering_mode = None;
                c.constraint_mode = None;
                c.awb_mode = None;
            }
            algo.controller().set_controls(c);
        }
        let values = SensorValues {
            frame: n,
            exposure,
            analogue_gain: gain,
            digital_gain: 1.0,
            frame_duration,
            verified: true,
        };
        // The dequeue's copy of the statistics (device: `next_held_raw`), not counted: it is
        // the front end's own memory traffic, written in place here.
        fill_stats(&mut raw, n, light, exposure.as_secs_f64() * gain);
        let index = (n % 3) as u32;
        ARMED.with(|a| a.set(steady));
        let (processed, frame) = counted(|| {
            let mut inline = FrontEndStats {
                raw: &raw,
                fe: &mut fe,
            };
            algo.process(
                &mut be,
                &mut inline,
                &NoControls,
                index,
                &values,
                run,
                (None, None),
                |_, _| {},
            )
        });
        let processed = processed.unwrap();
        // The back end's extra passes, as the device loop prepares them every frame.
        let (_, pass_allocs) = counted(|| passes.config(0, &be.builder).unwrap().is_some());
        sum = sum.wrapping_add(processed.output as u64 + processed.digital_gain.to_bits());
        last_seq = Some(n);
        ARMED.with(|a| a.set(false));
        if let Some(sensor) = algo.step().sensor {
            exposure = sensor.exposure;
            gain = sensor.analogue_gain;
            frame_duration = sensor.frame_duration;
        }
        if let Some(d) = dump.as_mut() {
            use std::io::Write;
            let step = algo.step();
            let h = digest(&[
                &step.params,
                &step.isp,
                algo.statistics(),
                be.builder.config(),
                &(
                    processed.digital_gain,
                    processed.flicker,
                    processed.ran,
                    processed.request_lands,
                ),
                &passes.config(0, &be.builder).unwrap().is_some(),
            ]);
            writeln!(d, "{n} {h:016x}").unwrap();
        }
        if steady && !(MODE_WINDOW.0..MODE_WINDOW.1).contains(&n) {
            totals.frame += frame + pass_allocs;
            totals.passes += pass_allocs;
        } else if steady {
            totals.window += frame + pass_allocs;
        }
        if std::env::var_os("STYX_PISP_PER_FRAME").is_some() && frame + pass_allocs > 0 {
            eprintln!("frame {n}: {} allocations (run {run})", frame + pass_allocs);
        }
    }
    std::hint::black_box(sum);
    let measured = (FRAMES - WARM) as f64;
    println!(
        "PiSP processed path, steady state: {:.2} allocations per frame (passes {:.2})",
        totals.frame as f64 / measured,
        totals.passes as f64 / measured,
    );
    trace::finish();
    // The steady state allocates nothing on this path: the lease (one record per frame) is
    // counted by the styx crate's own test of the lease path.
    println!(
        "mode window (names set): {:.2} allocations per frame",
        totals.window as f64 / (MODE_WINDOW.1 - MODE_WINDOW.0) as f64
    );
    assert_eq!(totals.frame, 0, "the processed path allocates");
}
