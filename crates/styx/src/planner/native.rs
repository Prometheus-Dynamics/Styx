//! Native cameras in the planner: raw Bayer formats, the ISP a camera's processed modes run on
//! (its `isp` property) and their capture step, priced from the CM5 measurements in `cost`.

use styx_core::prelude::FourCc;

use super::cost::{self, StepCost, megapixels};
use super::routes::describe;
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
fn binned(backend: &ProbedBackend, mode: &Mode) -> bool {
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
