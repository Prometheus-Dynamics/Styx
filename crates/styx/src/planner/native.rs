//! Native cameras in the planner: raw Bayer formats, the ISP a camera's processed modes run on
//! (its `isp` property) and their capture step, priced from the CM5 measurements in `cost`.
//! Also which modes of a camera behind an ISP are its raw sensor stream.

use styx_core::prelude::FourCc;

use super::cost::{self, StepCost, megapixels};
use super::routes::{describe, has_isp_second_output};
use super::{PlanStep, StepExecution, StepKind};
use crate::BackendKind;
use crate::prelude::{Mode, ProbedBackend};

/// Raw Bayer formats, 8-bit and the V4L2 packed / 16-bit ones sensors deliver.
pub(crate) fn raw_bayer(code: FourCc) -> bool {
    code.is_bayer_raw()
        || matches!(
            &code.to_u32().to_le_bytes(),
            b"pBAA"
                | b"pGAA"
                | b"pgAA"
                | b"pRAA"
                | b"pBCC"
                | b"pGCC"
                | b"pgCC"
                | b"pRCC"
                | b"BG10"
                | b"GB10"
                | b"BA10"
                | b"RG10"
                | b"BG12"
                | b"GB12"
                | b"BA12"
                | b"RG12"
                | b"BG16"
                | b"GB16"
                | b"GR16"
                | b"RG16"
                | b"BYR2"
                | b"BA81"
        )
}

/// Whether `code` is the sensor's raw stream rather than an ISP's processed output: raw Bayer on
/// any camera, and GREY/R8 on a Raspberry Pi camera behind libcamera. libcamera offers 8-bit
/// grey there only on its raw role, for sensors its camera helper calls mono, and that includes
/// colour sensors aliased to a mono one (the OV9782 as the OV9281): the frames are then the
/// Bayer mosaic, not grey.
pub(crate) fn raw_sensor_stream(backend: &ProbedBackend, code: FourCc) -> bool {
    raw_bayer(code) || (has_isp_second_output(backend) && matches!(code, FourCc::GREY | FourCc::R8))
}

/// Whether `backend` has a processed planar YUV mode, whose Y plane is grey without a copy.
pub(crate) fn has_processed_luma(backend: &ProbedBackend) -> bool {
    backend.descriptor.modes.iter().any(|m| {
        m.format.code.layout_info().planes.subsampling.is_some()
            && !raw_sensor_stream(backend, m.format.code)
    })
}

/// The ISP a native camera's processed modes run on (its `isp` property).
pub(crate) fn native_isp(backend: &ProbedBackend) -> Option<&str> {
    (backend.kind == BackendKind::Native)
        .then(|| {
            backend
                .properties
                .iter()
                .find(|(k, _)| k == "isp")
                .map(|(_, v)| v.as_str())
        })
        .flatten()
}

/// Whether processed `mode` is a binned one: no raw mode of its size, one of twice it.
pub(crate) fn binned(backend: &ProbedBackend, mode: &Mode) -> bool {
    let (w, h) = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    let raw = |w: u32, h: u32| {
        backend.descriptor.modes.iter().any(|m| {
            raw_bayer(m.format.code)
                && (
                    m.format.resolution.width.get(),
                    m.format.resolution.height.get(),
                ) == (w, h)
        })
    };
    !raw(w, h) && raw(2 * w, 2 * h)
}

/// The capture step of a processed (`NV12` / `RG24`) native mode: the sensor plus the PiSP or
/// the software ISP and the 3A loop. `None` for raw modes and other backends.
pub(crate) fn processed_capture_step(
    backend: &ProbedBackend,
    mode: &Mode,
    fps: Option<f32>,
) -> Option<PlanStep> {
    if backend.kind != BackendKind::Native || raw_bayer(mode.format.code) {
        return None;
    }
    let mp = megapixels(
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    let sensor = cost::native_capture_latency_ms(fps);
    let (execution, cost, how) = match native_isp(backend) {
        Some("pisp") => (
            StepExecution::Hardware,
            StepCost::offloaded(
                sensor + cost::PISP_PROCESS_LATENCY_MS,
                cost::PISP_PROCESS_CPU_MS,
            ),
            "PiSP front end statistics and back end, raw frames as dma-bufs, 3A in Styx",
        ),
        _ if gpu_isp().is_some() => {
            // Binned modes read the whole raw frame too (half the GPU work, same copies in).
            let raw_mp = if binned(backend, mode) { 4.0 * mp } else { mp };
            (
                StepExecution::Hardware,
                StepCost::offloaded(
                    sensor + cost::GPUISP_LATENCY_MS_PER_MP * raw_mp + cost::ALGORITHMS_MS,
                    cost::GPUISP_CPU_MS_PER_MP * raw_mp + cost::ALGORITHMS_MS,
                ),
                "GPU ISP (Vulkan) and 3A in Styx",
            )
        }
        _ => {
            let threads = cost::default_softisp_threads();
            let extra = if threads > 1 {
                cost::SOFTISP_THREADS_CPU_MS
            } else {
                0.0
            };
            // A binned mode reads a raw frame of four times its size, with no demosaic.
            let isp = if binned(backend, mode) {
                cost::SOFTISP_BINNED_MS_PER_RAW_MP * 4.0 * mp
            } else {
                cost::SOFTISP_MS_PER_MP * mp
            };
            let factor = cost::softisp_latency_ms_per_mp(threads) / cost::SOFTISP_MS_PER_MP;
            let cpu = isp + cost::ALGORITHMS_MS + extra;
            let latency = isp * factor + cost::ALGORITHMS_MS;
            (
                StepExecution::Cpu,
                StepCost::offloaded(sensor + latency, cpu),
                "software ISP and 3A in Styx",
            )
        }
    };
    Some(PlanStep {
        kind: StepKind::Capture,
        execution,
        detail: format!("{} ({how})", describe(backend, mode)),
        cost,
    })
}

/// Whether the software path runs on a GPU (feature `gpu-isp`, a Vulkan device found).
pub(crate) fn gpu_isp_present() -> bool {
    gpu_isp().is_some()
}

/// The GPU the software path runs on (feature `gpu-isp`, a Vulkan device found), if any.
fn gpu_isp() -> Option<String> {
    #[cfg(feature = "gpu-isp")]
    {
        crate::gpu_isp::device().map(|d| d.name)
    }
    #[cfg(not(feature = "gpu-isp"))]
    {
        None
    }
}
