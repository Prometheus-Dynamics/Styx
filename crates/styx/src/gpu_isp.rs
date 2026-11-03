//! The GPU ISP for native cameras without a PiSP (feature `gpu-isp`): processed modes of the
//! software path run on a Vulkan GPU (`styx-gpuisp`, same pictures and statistics as the
//! software ISP's integer arithmetic) when the process finds one, unless the device is one
//! measured slower than the CPU at it ([`SLOWER_THAN_CPU`]). `STYX_GPU_ISP=0` keeps them on the
//! CPU, `STYX_GPU_ISP=1` takes the GPU on any device; `STYX_GPUISP_DEVICE` picks the device
//! (index or part of its name; a software rasteriser such as llvmpipe only when named).

use std::sync::OnceLock;

use styx_pipeline::styx_gpuisp::{DeviceInfo, DeviceSelect, GpuContext};

fn disabled() -> bool {
    matches!(
        std::env::var("STYX_GPU_ISP").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

fn forced() -> bool {
    matches!(
        std::env::var("STYX_GPU_ISP").as_deref(),
        Ok("1" | "on" | "true" | "yes")
    )
}

/// Devices (start of the name) where the GPU ISP is slower than the software ISP on the
/// board's CPU, used only when asked for (`STYX_GPU_ISP=1`). V3D 7.1 (Raspberry Pi 5 / CM5,
/// 2026-11-02, live 1280x800 NV12): 28.3 fps, about 35 ms a frame and 133 ms latency, against
/// the CPU's 60 fps at 12.3 ms.
const SLOWER_THAN_CPU: &[&str] = &["V3D"];

/// Whether the GPU ISP runs on `name` by default.
fn preferred(name: &str, forced: bool) -> bool {
    forced || !SLOWER_THAN_CPU.iter().any(|p| name.starts_with(p))
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
            Ok(c) if !preferred(&c.info().name, forced()) => {
                let i = c.info();
                crate::trace::info!(device = %i.name, "GPU ISP slower than the CPU here: off (STYX_GPU_ISP=1 takes it)");
                None
            }
            Ok(c) => {
                let i = c.info();
                crate::trace::info!(device = %i.name, driver = %i.driver, "GPU ISP available");
                Some(c)
            }
            Err(e) => {
                crate::trace::debug!(error = %e, "no GPU ISP");
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

#[cfg(test)]
mod tests {
    #[test]
    fn slower_devices_take_the_gpu_only_when_asked() {
        assert!(!super::preferred("V3D 7.1.10.2", false));
        assert!(super::preferred("V3D 7.1.10.2", true));
        assert!(super::preferred(
            "AMD Radeon RX 6800 XT (RADV NAVI21)",
            false
        ));
    }
}
