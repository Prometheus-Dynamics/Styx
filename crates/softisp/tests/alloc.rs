//! Allocations of a warmed-up software ISP frame, counted on the test's thread and the ISP's
//! helper threads (`styx-softisp-*`): what the test harness's own threads do is left out. The
//! tests take turns, the count being shared.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use styx_softisp::*;

struct Counting;

static COUNT: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Whether this thread's allocations count: 0 not known yet, 1 yes, 2 no.
    static COUNTED: Cell<u8> = const { Cell::new(0) };
}

/// The software ISP's helper threads, by name (no allocation: a fixed buffer).
fn is_helper() -> bool {
    let mut name = [0u8; 32];
    // SAFETY: writes at most `name.len()` bytes (NUL-terminated) for the calling thread.
    unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len()) };
    name.starts_with(b"styx-softisp-")
}

fn note() {
    let counted = COUNTED
        .try_with(|c| {
            if c.get() == 0 {
                c.set(if is_helper() { 1 } else { 2 });
            }
            c.get() == 1
        })
        .unwrap_or(false);
    if counted {
        COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts this (the test's) thread.
fn count_this_thread() {
    COUNTED.with(|c| c.set(1));
}

// SAFETY: forwards to the system allocator unchanged; the bookkeeping touches an atomic, a
// const-initialised thread-local and a stack buffer, none of which allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The tests of this binary, one at a time: the count is process-wide.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const W: usize = 320;
const H: usize = 200;

/// Allocations over 20 frames of `isp` once warm, into `out`.
fn per_frame(
    isp: &mut SoftIsp,
    raw: &[u8],
    stride: usize,
    mut out: impl FnMut(&mut SoftIsp, &[u8], usize),
) -> u64 {
    for _ in 0..10 {
        out(isp, raw, stride);
    }
    let start = COUNT.load(Ordering::Relaxed);
    for _ in 0..20 {
        out(isp, raw, stride);
    }
    COUNT.load(Ordering::Relaxed) - start
}

#[test]
fn a_warm_frame_allocates_nothing() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    count_this_thread();
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let stride = format.min_stride();
    let raw: Vec<u8> = (0..stride * H).map(|i| (i * 31 % 251) as u8).collect();
    let post = IspParams {
        denoise: Some(Denoise {
            luma: Some(LumaDenoise {
                constant: 400.0,
                slope: 20.0,
                strength: 0.8,
            }),
            chroma: Some(ChromaDenoise {
                threshold: 3000.0,
                strength: 0.8,
            }),
        }),
        sharpen: Some(Sharpen {
            amount: 0.5,
            threshold: 1.0,
            limit: 20.0,
        }),
        ..IspParams::default()
    };
    let mut rgb = vec![0u8; 3 * W * H];
    let mut y = vec![0u8; W * H];
    let mut uv = vec![0u8; W * H / 2];
    let (mut u, mut v) = (vec![0u8; W * H / 4], vec![0u8; W * H / 4]);
    let mut report = Vec::new();
    for threads in [1, 4] {
        for (name, params) in [("plain", IspParams::default()), ("post", post.clone())] {
            let mut isp = SoftIsp::new(format, params).unwrap().with_threads(threads);
            let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
                isp.process(
                    raw,
                    stride,
                    Scale::Full,
                    OutputBuffers::Rgb24 {
                        data: &mut rgb,
                        stride: 3 * W,
                    },
                )
                .unwrap();
            });
            report.push(format!("{name} rgb24, {threads} threads: {n}"));
            let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
                isp.process(
                    raw,
                    stride,
                    Scale::Full,
                    OutputBuffers::Nv12 {
                        y: &mut y,
                        y_stride: W,
                        uv: &mut uv,
                        uv_stride: W,
                    },
                )
                .unwrap();
            });
            report.push(format!("{name} nv12, {threads} threads: {n}"));
            let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
                isp.process(
                    raw,
                    stride,
                    Scale::Full,
                    OutputBuffers::I420 {
                        y: &mut y,
                        y_stride: W,
                        u: &mut u,
                        u_stride: W / 2,
                        v: &mut v,
                        v_stride: W / 2,
                    },
                )
                .unwrap();
            });
            report.push(format!("{name} i420, {threads} threads: {n}"));
        }
    }
    let bad: Vec<&String> = report.iter().filter(|r| !r.ends_with(": 0")).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn a_warm_frame_with_statistics_allocates_nothing() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    count_this_thread();
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let stride = format.min_stride();
    let raw: Vec<u8> = (0..stride * H).map(|i| (i * 31 % 251) as u8).collect();
    let params = IspParams {
        stats: Some(StatsConfig {
            focus: true,
            ..StatsConfig::default()
        }),
        ..IspParams::default()
    };
    let mut rgb = vec![0u8; 3 * W * H];
    let (ow, oh) = (W / 2, H / 2);
    let mut small = vec![0u8; 3 * ow * oh];
    let mut report = Vec::new();
    for threads in [1, 4] {
        let new = || {
            SoftIsp::new(format, params.clone())
                .unwrap()
                .with_threads(threads)
        };
        let mut isp = new();
        let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
            let out = OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: 3 * W,
            };
            let stats = isp.process(raw, stride, Scale::Full, out).unwrap().unwrap();
            isp.recycle_stats(stats);
        });
        report.push(format!("process, {threads} threads: {n}"));

        // Settled: statistics only on every other frame.
        let mut isp = new();
        let mut on = false;
        let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
            on = !on;
            isp.set_statistics(on);
            let out = OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: 3 * W,
            };
            if let Some(stats) = isp.process(raw, stride, Scale::Full, out).unwrap() {
                isp.recycle_stats(stats);
            }
        });
        report.push(format!("every other frame, {threads} threads: {n}"));

        // The overview of a frame cut into windows, and statistics alone.
        let mut isp = new();
        let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
            let out = OutputBuffers::Rgb24 {
                data: &mut small,
                stride: 3 * ow,
            };
            let stats = isp.process_binned(raw, stride, 2, out).unwrap().unwrap();
            isp.recycle_stats(stats);
        });
        report.push(format!("binned, {threads} threads: {n}"));
        let mut isp = new();
        let n = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
            let stats = isp.statistics(raw, stride).unwrap().unwrap();
            isp.recycle_stats(stats);
        });
        report.push(format!("statistics alone, {threads} threads: {n}"));
    }
    let bad: Vec<&String> = report.iter().filter(|r| !r.ends_with(": 0")).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

/// Parameters changing on every frame (white balance, digital gain, lens shading, tone curve),
/// written into the same buffers and swapped in: the tables are rebuilt in place.
#[test]
fn changing_parameters_allocate_nothing() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    count_this_thread();
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let stride = format.min_stride();
    let raw: Vec<u8> = (0..stride * H).map(|i| (i * 31 % 251) as u8).collect();
    let mut rgb = vec![0u8; 3 * W * H];
    let mut report = Vec::new();
    for arithmetic in [Arithmetic::Int, Arithmetic::IntPolyTone, Arithmetic::Half] {
        let base = IspParams {
            arithmetic,
            lens_shading: Some(LensShading::radial(16, 12, 0.4)),
            tone: Some(ToneCurve::Points {
                points: vec![[0.0, 0.0], [0.25, 0.5], [1.0, 1.0]],
            }),
            white_balance: Some(WhiteBalance {
                r: 1.5,
                g: 1.0,
                b: 1.8,
            }),
            ..IspParams::default()
        };
        let mut isp = SoftIsp::new(format, base.clone()).unwrap();
        let mut params = base.clone();
        let mut n = 0u32;
        let count = per_frame(&mut isp, &raw, stride, |isp, raw, stride| {
            n += 1;
            let k = (n % 7) as f32;
            if let Some(wb) = &mut params.white_balance {
                wb.r = 1.5 + 0.01 * k;
            }
            params.digital_gain = 1.0 + 0.02 * k;
            if let Some(ls) = &mut params.lens_shading {
                ls.r.clone_from(&base.lens_shading.as_ref().unwrap().r);
                ls.r[5] *= 1.0 + 0.05 * k;
            }
            if let Some(ToneCurve::Points { points }) = &mut params.tone {
                points[1][1] = 0.45 + 0.01 * k;
            }
            isp.swap_params(&mut params).unwrap();
            let out = OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: 3 * W,
            };
            isp.process(raw, stride, Scale::Full, out).unwrap();
        });
        report.push(format!("{arithmetic:?} ({:?}): {count}", isp.arithmetic()));
    }
    let bad: Vec<&String> = report.iter().filter(|r| !r.ends_with(": 0")).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}
