//! The GPU ISP for native cameras without a PiSP (feature `gpu-isp`): processed modes of the
//! software path run on a Vulkan GPU (`styx-gpuisp`, same pictures and statistics as the
//! software ISP's integer arithmetic) when the process finds one. `STYX_GPU_ISP=0` keeps them
//! on the CPU; `STYX_GPUISP_DEVICE` picks the device (index or part of its name; a software
//! rasteriser such as llvmpipe only when named).

use std::sync::OnceLock;

use styx_pipeline::styx_gpuisp::{DeviceInfo, DeviceSelect, GpuContext};

fn disabled() -> bool {
    matches!(
        std::env::var("STYX_GPU_ISP").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// The process's GPU ISP device, opened on first use (`None`: no Vulkan, no suitable
/// device, or disabled).
pub(crate) fn context() -> Option<GpuContext> {
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    CTX.get_or_init(|| {
        if disabled() {
            return None;
        }
        match GpuContext::open(DeviceSelect::Auto) {
            Ok(c) => {
                let i = c.info();
                tracing::info!(device = %i.name, driver = %i.driver, "GPU ISP available");
                Some(c)
            }
            Err(e) => {
                tracing::debug!(error = %e, "no GPU ISP");
                None
            }
        }
    })
    .clone()
}

/// The device processed native modes run on, if any (for the planner).
pub(crate) fn device() -> Option<DeviceInfo> {
    context().map(|c| c.info().clone())
}
