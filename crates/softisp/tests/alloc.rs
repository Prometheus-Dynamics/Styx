//! Allocations of a warmed-up software ISP frame, counted across its helper threads. In a binary of
//! its own: the count is process-wide, so nothing else may run beside this test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use styx_softisp::*;

struct Counting;

static COUNT: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNT.fetch_add(1, Ordering::Relaxed);
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
