//! Cost estimates for planning, from measurements on a Raspberry Pi CM5 (Cortex-A76 @ 2.4 GHz).
//!
//! Absolute numbers differ by machine, but ratios between paths hold well enough to rank plans.
//! Per-megapixel constants are scaled by the frame's pixel count.

use styx_core::prelude::Priority;

/// Estimated cost of one plan step, in milliseconds per frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StepCost {
    /// Time added to the sensor-to-consumer path.
    pub latency_ms: f32,
    /// CPU time spent on the host.
    pub cpu_ms: f32,
}

impl StepCost {
    pub const ZERO: StepCost = StepCost {
        latency_ms: 0.0,
        cpu_ms: 0.0,
    };

    pub(crate) fn cpu(ms: f32) -> Self {
        Self {
            latency_ms: ms,
            cpu_ms: ms,
        }
    }

    pub(crate) fn offloaded(latency_ms: f32, cpu_ms: f32) -> Self {
        Self { latency_ms, cpu_ms }
    }
}

impl std::ops::Add for StepCost {
    type Output = StepCost;
    fn add(self, rhs: StepCost) -> StepCost {
        StepCost {
            latency_ms: self.latency_ms + rhs.latency_ms,
            cpu_ms: self.cpu_ms + rhs.cpu_ms,
        }
    }
}

pub(crate) fn megapixels(width: u32, height: u32) -> f32 {
    (width as f32 * height as f32) / 1_000_000.0
}

/// libcamera/PiSP: SensorTimestamp to frame delivery (measured 8.0 ms at 1280x800).
pub(crate) const ISP_CAPTURE_LATENCY_MS: f32 = 8.0;
/// USB cameras expose, encode and transfer a whole frame before delivery; measured at about
/// 0.8 frame intervals (C270: 32 ms at 24.7 fps).
pub(crate) fn uvc_capture_latency_ms(fps: Option<f32>) -> f32 {
    fps.filter(|fps| *fps > 0.0)
        .map_or(33.0, |fps| (0.8 * 1000.0 / fps).max(8.0))
}
/// turbojpeg luma decode, single thread (C270 720p: 1.59 ms).
pub(crate) const MJPEG_LUMA_MS_PER_MP: f32 = 1.75;
/// With restart markers, split across four A76 cores (C270 720p: 0.88 ms).
pub(crate) const MJPEG_LUMA_PARALLEL_MS_PER_MP: f32 = 0.95;
/// Hardware JPEG/H.26x decode: CPU cost of submitting and mapping, plus added latency.
pub(crate) const HW_DECODE_CPU_MS_PER_MP: f32 = 0.3;
pub(crate) const HW_DECODE_LATENCY_MS_PER_MP: f32 = 1.5;
/// FFmpeg software luma decode of MJPEG (C270 720p: 3.0 ms).
pub(crate) const FFMPEG_SW_MJPEG_MS_PER_MP: f32 = 3.3;
/// Software H.264/H.265 decode is far costlier than JPEG.
pub(crate) const SW_VIDEO_DECODE_MS_PER_MP: f32 = 8.0;
/// Extracting luma from packed YUYV (one strided copy).
pub(crate) const YUYV_LUMA_MS_PER_MP: f32 = 0.35;
/// Converting RGB to luma.
pub(crate) const RGB_LUMA_MS_PER_MP: f32 = 1.2;
/// 2x2 box pyramid level, per megapixel of the level's source (1280x800 → 640x400: 0.16 ms).
pub(crate) const BOX_LEVEL_MS_PER_MP: f32 = 0.16;
/// Copying a frame to realign its rows.
pub(crate) const REALIGN_MS_PER_MP: f32 = 0.3;

/// Score a plan for `priority`; lower is better.
pub(crate) fn score(total: StepCost, priority: Priority) -> f32 {
    match priority {
        Priority::Latency => total.latency_ms + 0.25 * total.cpu_ms,
        Priority::Throughput => total.cpu_ms + 0.05 * total.latency_ms,
        Priority::Power => total.cpu_ms,
    }
}

/// Decode threads per frame for `priority` unless overridden.
pub(crate) fn decode_threads(priority: Priority, overridden: Option<usize>) -> usize {
    overridden.unwrap_or(match priority {
        // 0 = automatic (up to four cores when the JPEG has restart markers).
        Priority::Latency => 0,
        Priority::Throughput | Priority::Power => 1,
    })
}

/// Frames buffered between capture and consumer for `priority` unless overridden.
pub(crate) fn queue_depth(priority: Priority, overridden: Option<usize>) -> usize {
    overridden.unwrap_or(match priority {
        Priority::Latency => 2,
        Priority::Throughput => 4,
        Priority::Power => 3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_rank_offload_differently() {
        let software = StepCost::cpu(2.0);
        let hardware = StepCost::offloaded(3.0, 0.3);
        assert!(score(software, Priority::Latency) < score(hardware, Priority::Latency));
        assert!(score(hardware, Priority::Throughput) < score(software, Priority::Throughput));
        assert!(score(hardware, Priority::Power) < score(software, Priority::Power));
    }

    #[test]
    fn uvc_latency_scales_with_frame_interval() {
        assert!(uvc_capture_latency_ms(Some(30.0)) < uvc_capture_latency_ms(Some(10.0)));
        assert!((uvc_capture_latency_ms(Some(25.0)) - 32.0).abs() < 0.1);
    }
}
