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
/// A sensor Styx drives itself: the frame lands in memory one readout (about one frame
/// period) after exposure, with no encode or transfer on top.
pub(crate) fn native_capture_latency_ms(fps: Option<f32>) -> f32 {
    fps.filter(|fps| *fps > 0.0)
        .map_or(16.7, |fps| 1000.0 / fps + 0.5)
}
/// Native camera through the PiSP (CM5, OV9782 1280x800, NV12 through the Styx API): the
/// front end's raw frame and statistics are dequeued at the end of the readout, then the back
/// end job takes 0.85 ms (the 3A loop runs meanwhile); the host spends 0.25 ms per frame in all
/// (0.12 ms of it the driver writing the back end config to the hardware).
pub(crate) const PISP_PROCESS_LATENCY_MS: f32 = 0.9;
pub(crate) const PISP_PROCESS_CPU_MS: f32 = 0.3;
/// Software ISP (styx-softisp, CPU time on A76 cores): unpack, black level, white balance,
/// lens shading, demosaic, CCM, tone curve, NV12/RGB out plus statistics, from the receiver's
/// uncached buffer: 5.6 ms per 1280x800 frame on one core (CM5, native/perf-soft).
pub(crate) const SOFTISP_MS_PER_MP: f32 = 5.5;
/// CPU the software ISP's helper threads add per frame (wake-ups, band edges; 4 threads).
pub(crate) const SOFTISP_THREADS_CPU_MS: f32 = 0.4;

/// Threads the native backend's software ISP uses by default: one per core, at most 4.
pub(crate) fn default_softisp_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
}

/// Software ISP time per megapixel on `threads` threads (row bands spread over the cores:
/// 2.8 / 1.9 / 1.5 ms for 1280x800 on 2 / 3 / 4 A76 cores).
pub(crate) fn softisp_latency_ms_per_mp(threads: usize) -> f32 {
    let n = threads.max(1) as f32;
    if n <= 1.0 {
        SOFTISP_MS_PER_MP
    } else {
        SOFTISP_MS_PER_MP / n * 1.05
    }
}
/// The 3A algorithms per frame (AE, AWB, CCM, contrast; Raspberry Pi tuning).
pub(crate) const ALGORITHMS_MS: f32 = 0.3;
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
/// libx264 low latency (`superfast`, `zerolatency`) on a Cortex-A76: 14.5 ms per 720p frame.
pub(crate) const SW_H26X_ENCODE_MS_PER_MP: f32 = 16.0;
/// JPEG encoding of YUV/RGB frames on a Cortex-A76.
pub(crate) const SW_JPEG_ENCODE_MS_PER_MP: f32 = 6.0;
/// Hardware encoders (V4L2 mem2mem, VA-API): CPU time to hand frames over and collect packets.
pub(crate) const HW_ENCODE_CPU_MS_PER_MP: f32 = 0.5;
pub(crate) const HW_ENCODE_LATENCY_MS_PER_MP: f32 = 4.0;

/// Share of a full JPEG decode left when decoding at 1/`denom` size: scaling skips IDCT work
/// but not entropy decoding (CM5: ½ saves ~15%, ⅛ ~40%).
pub(crate) fn scaled_decode_factor(denom: u8) -> f32 {
    match denom {
        0 | 1 => 1.0,
        2 => 0.85,
        4 => 0.7,
        _ => 0.6,
    }
}

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

/// Frames buffered between capture and consumer for `priority` unless overridden. The capture
/// queue drops its oldest frame when full, so latency gets the newest frame only.
pub(crate) fn queue_depth(priority: Priority, overridden: Option<usize>) -> usize {
    overridden.unwrap_or(match priority {
        Priority::Latency => 1,
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
