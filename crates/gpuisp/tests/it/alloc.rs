//! A GPU ISP frame's Rust allocations, counted once the stream has warmed up (the driver's own
//! are not the Rust allocator's): from memory, from a buffer of the ISP's device
//! (`Input::Buffer`), with statistics handed back, and with the parameters changing every frame
//! as the 3A loop changes them. Skipped without a Vulkan device.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use crate::common::{mosaic, pack};
#[cfg(feature = "daedalus")]
use styx_gpuisp::{ExternalBuffer, GpuContext};
use styx_gpuisp::{GpuIsp, Input};
use styx_softisp::*;

struct Counting;

thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
    static TRACING: Cell<bool> = const { Cell::new(false) };
}

/// `STYX_GPU_ALLOC_TRACE=1`: the call stack of each allocation counted.
static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note() {
    let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    if TRACE.load(std::sync::atomic::Ordering::Relaxed)
        && TRACING.try_with(|t| !t.replace(true)).unwrap_or(false)
    {
        eprintln!(
            "allocation:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        TRACING.with(|t| t.set(false));
    }
}

// SAFETY: forwards to the system allocator unchanged.
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

#[test]
fn a_frame_from_memory_allocates_nothing_when_warm() {
    let (w, h) = (128usize, 96usize);
    let m = mosaic(w, h, CfaPattern::Rggb, |x, y| {
        [
            (x % 1024) as u16,
            ((x + y) % 1024) as u16,
            (y % 1024) as u16,
        ]
    });
    let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        tone: Some(ToneCurve::Srgb),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    };
    let Ok(mut isp) = GpuIsp::new(format, params) else {
        eprintln!("no Vulkan device: skipped");
        return;
    };
    let mut out = vec![0u8; w * h * 3];
    let mut run = || {
        let buffers = OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        };
        isp.process_input(Input::Bytes(&raw), stride, Scale::Full, buffers)
            .unwrap();
    };
    for _ in 0..20 {
        run();
    }
    const FRAMES: u64 = 100;
    let before = COUNT.with(Cell::get);
    for _ in 0..FRAMES {
        run();
    }
    let per_frame = (COUNT.with(Cell::get) - before) as f64 / FRAMES as f64;
    assert_eq!(per_frame, 0.0, "allocations per GPU ISP frame");
}

/// Allocations per call of `run` on this thread, after 20 calls of warm-up.
fn per_frame(mut run: impl FnMut()) -> f64 {
    for _ in 0..20 {
        run();
    }
    const FRAMES: u64 = 100;
    let before = COUNT.with(Cell::get);
    TRACE.store(
        std::env::var_os("STYX_GPU_ALLOC_TRACE").is_some_and(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    for _ in 0..FRAMES {
        run();
    }
    TRACE.store(false, std::sync::atomic::Ordering::Relaxed);
    (COUNT.with(Cell::get) - before) as f64 / FRAMES as f64
}

fn frame(w: usize, h: usize) -> (Vec<u8>, usize, RawFormat) {
    let m = mosaic(w, h, CfaPattern::Bggr, |x, y| {
        [
            (x * 5 % 1024) as u16,
            ((x + y) % 1024) as u16,
            (y * 3 % 1024) as u16,
        ]
    });
    let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    (raw, stride, format)
}

/// Parameters as the 3A loop sets them: white balance, gain, lens shading and tone, with the
/// statistics on.
fn three_a() -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        lens_shading: Some(LensShading::radial(16, 12, 0.4)),
        tone: Some(ToneCurve::Points {
            points: vec![[0.0, 0.0], [0.25, 0.5], [1.0, 1.0]],
        }),
        white_balance: Some(WhiteBalance {
            r: 1.5,
            g: 1.0,
            b: 1.8,
        }),
        stats: Some(StatsConfig::default()),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    }
}

/// Moves `params` on to frame `n`'s values in place.
fn step(params: &mut IspParams, base: &IspParams, n: u32) {
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
}

#[test]
fn changing_parameters_and_statistics_allocate_nothing_when_warm() {
    let (w, h) = (128usize, 96usize);
    let (raw, stride, format) = frame(w, h);
    let base = three_a();
    let Ok(mut isp) = GpuIsp::new(format, base.clone()) else {
        eprintln!("no Vulkan device: skipped");
        return;
    };
    let mut params = base.clone();
    let mut out = vec![0u8; w * h * 3];
    let mut n = 0;
    let count = per_frame(|| {
        n += 1;
        step(&mut params, &base, n);
        isp.swap_params(&mut params).unwrap();
        let buffers = OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        };
        let stats = isp
            .process_input(Input::Bytes(&raw), stride, Scale::Full, buffers)
            .unwrap()
            .expect("statistics asked for");
        assert_eq!(stats.zones.len(), 16 * 12);
        isp.recycle_stats(stats);
    });
    assert_eq!(count, 0.0, "allocations per GPU ISP frame");
}

/// With the output stages on (denoise and sharpening, parameters changing every frame), a
/// frame allocates nothing either, to the output format and at the half size.
#[test]
fn output_stages_allocate_nothing_when_warm() {
    let (w, h) = (128usize, 96usize);
    let (raw, stride, format) = frame(w, h);
    let mut base = three_a();
    base.denoise = Some(Denoise {
        luma: Some(LumaDenoise {
            constant: 400.0,
            slope: 20.0,
            strength: 1.0,
        }),
        chroma: Some(ChromaDenoise {
            threshold: 3000.0,
            strength: 1.0,
        }),
    });
    base.sharpen = Some(Sharpen {
        amount: 0.8,
        threshold: 2.0,
        limit: 20.0,
    });
    let Ok(mut isp) = GpuIsp::new(format, base.clone()) else {
        eprintln!("no Vulkan device: skipped");
        return;
    };
    let mut params = base.clone();
    let mut rgb = vec![0u8; w * h * 3];
    let mut luma = vec![0u8; w / 2 * h / 2];
    let mut n = 0;
    let count = per_frame(|| {
        n += 1;
        step(&mut params, &base, n);
        isp.swap_params(&mut params).unwrap();
        let stats = isp
            .process_input(
                Input::Bytes(&raw),
                stride,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: w * 3,
                },
            )
            .unwrap()
            .expect("statistics asked for");
        isp.recycle_stats(stats);
        let half = isp
            .process(
                &raw,
                stride,
                Scale::Half,
                OutputBuffers::Luma {
                    data: &mut luma,
                    stride: w / 2,
                },
            )
            .unwrap()
            .expect("statistics asked for");
        isp.recycle_stats(half);
    });
    assert_eq!(
        count, 0.0,
        "allocations per GPU ISP frame with the output stages"
    );
}

/// From a buffer of Daedalus's device, as its imported frames come (`Input::Buffer`),
/// submitted on Daedalus's queue through its `RawSubmitter`: nothing (was 17 a frame in
/// wgpu-core's submission through `submit_raw`).
#[cfg(feature = "daedalus")]
#[test]
fn a_frame_from_a_device_buffer_allocates_nothing_when_warm() {
    use daedalus_gpu::GpuStorage;
    let Some(gpu) = crate::common::daedalus_gpu() else {
        return;
    };
    let Ok(ctx) = GpuContext::from_daedalus(&gpu) else {
        eprintln!("Daedalus's device unsupported: skipped");
        return;
    };
    let (w, h) = (128usize, 96usize);
    let (raw, stride, format) = frame(w, h);
    let mut isp = GpuIsp::with_context(&ctx, format, three_a()).unwrap();
    let storage = GpuStorage::with_data(&gpu, &raw, Some("raw")).unwrap();
    // wgpu submits `write_buffer` data only with its own next submit; RawSubmitter's is not.
    gpu.flush_uploads().unwrap();
    // SAFETY: Daedalus's buffer on Daedalus's device, alive and unwritten for the calls.
    let input = unsafe {
        let raw_vk = gpu.raw_vulkan().unwrap();
        let b = storage.raw_vk_buffer().unwrap();
        ExternalBuffer::new(raw_vk.device.handle(), b.buffer, 0, raw.len())
    };
    let mut out = vec![0u8; w * h * 3];
    let count = per_frame(|| {
        let buffers = OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        };
        let stats = isp
            .process_input(Input::Buffer(input), stride, Scale::Full, buffers)
            .unwrap()
            .expect("statistics asked for");
        isp.recycle_stats(stats);
    });
    assert_eq!(
        count, 0.0,
        "allocations per GPU ISP frame from a device buffer"
    );
}
