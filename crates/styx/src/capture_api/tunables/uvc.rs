//! The userspace UVC backend's settings (feature `uvc`).

use super::StyxConfig;

/// How the userspace UVC backend (`BackendKind::Uvc`) runs a camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct UvcConfig {
    /// Detach `uvcvideo` from the camera's video interfaces when it holds them, and give them
    /// back when the capture ends (its `/dev/video*` nodes go away meanwhile). Off by default:
    /// a camera `uvcvideo` holds is then refused. Also `STYX_UVC_DETACH=1`.
    pub detach_kernel_driver: bool,
    /// URBs in flight (5 as `uvcvideo`).
    pub urbs: usize,
    /// Isochronous packets per URB (32 = 4 ms at high speed).
    pub packets_per_urb: usize,
    /// Deliver damaged frames (lost packets, short frames) instead of dropping them.
    pub deliver_damaged: bool,
}

impl Default for UvcConfig {
    fn default() -> Self {
        Self {
            detach_kernel_driver: false,
            urbs: 5,
            packets_per_urb: 32,
            deliver_damaged: false,
        }
    }
}

impl UvcConfig {
    /// Whether to detach the kernel driver: the setting or `STYX_UVC_DETACH=1`.
    pub fn detach(&self) -> bool {
        self.detach_kernel_driver
            || std::env::var_os("STYX_UVC_DETACH").is_some_and(|v| !v.is_empty() && v != "0")
    }
}

impl StyxConfig {
    /// Let the userspace UVC backend detach `uvcvideo` from a camera it opens (see
    /// [`UvcConfig::detach_kernel_driver`]).
    pub fn uvc_detach_kernel_driver(mut self, detach: bool) -> Self {
        self.backends.uvc.detach_kernel_driver = detach;
        self
    }

    /// The userspace UVC backend's settings.
    pub fn uvc_config(&self) -> UvcConfig {
        self.backends.uvc
    }
}
