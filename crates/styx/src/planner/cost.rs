//! Cost estimates for planning, from measurements on a Raspberry Pi CM5 (Cortex-A76 @ 2.4 GHz).
//!
//! Absolute numbers differ by machine, but ratios between paths hold well enough to rank plans.
//! Per-megapixel constants are scaled by the frame's pixel count.

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
/// lens shading, demosaic, CCM, tone curve, NV12/RGB out plus statistics, fp16 arithmetic,
/// from cached capture buffers: 2.75 (RGB24) - 3.05 (NV12) ms per 1280x800 frame on one core
/// (CM5, native/perf-soft2).
pub(crate) const SOFTISP_MS_PER_MP: f32 = 2.9;
/// Binned (half-size) processed modes: each 2x2 quad becomes a pixel, priced per megapixel
/// of the raw frame read (1.25 ms for a 1280x800 frame).
pub(crate) const SOFTISP_BINNED_MS_PER_RAW_MP: f32 = 1.2;
/// CPU the software ISP's helper threads add per frame (wake-ups, band edges, the cores
/// sharing memory bandwidth; 4 threads at 1280x800: 0.5 ms with NV12, 1.3 ms with RGB24).
pub(crate) const SOFTISP_THREADS_CPU_MS: f32 = 0.9;

/// GPU ISP (styx-gpuisp, feature `gpu-isp`): the software ISP's pipeline as Vulkan compute
/// shaders. Host CPU per raw megapixel (copying the raw frame into a mapped buffer, the
/// output out of one, the lens shading and tone tables, one submission and fence wait):
/// 0.47 ms per 1280x800 NV12 frame for the whole loop (ISP, settings 0.2 ms, statistics
/// conversion, 3A at 15 Hz) on a Ryzen host with an RX 6800 XT, against 1.77 ms for the
/// software ISP on one of its cores (`native-pipeline gpu-bench`, 30 and 120 fps alike).
pub(crate) const GPUISP_CPU_MS_PER_MP: f32 = 0.3;
/// GPU ISP time from submission to the output being in host memory per raw megapixel (RX
/// 6800 XT: 0.52 ms for NV12 1280x800, of it 0.21 ms on the GPU; 0.74 ms for RGB24).
pub(crate) const GPUISP_LATENCY_MS_PER_MP: f32 = 0.6;

/// Threads the native backend's software ISP uses by default: [`softisp_threads_for`] the
/// cores this process may run on.
pub(crate) fn default_softisp_threads() -> usize {
    softisp_threads_for(std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// The software ISP's default thread count on `cores` cores: half of them, at most 4 (2 on the
/// CM5). More hangs the dev box's CM5 at 2.4 GHz: three ISP threads at 120 fps, or four (even
/// replaying a recording, no camera streaming) stop it within seconds without a kernel message
/// and the hardware watchdog reboots it, where two threads at 120 fps, or four at 1.8 GHz, ran
/// for minutes (`docs/native-stack/pipeline.md`, "All four cores").
pub(crate) fn softisp_threads_for(cores: usize) -> usize {
    (cores / 2).clamp(1, 4)
}

/// Software ISP time per megapixel on `threads` threads (row bands spread over the cores:
/// 1.9 / 1.0 ms for 1280x800 NV12 with statistics on 2 / 4 A76 cores).
pub(crate) fn softisp_latency_ms_per_mp(threads: usize) -> f32 {
    let n = threads.max(1) as f32;
    if n <= 1.0 {
        SOFTISP_MS_PER_MP
    } else {
        SOFTISP_MS_PER_MP / n * 1.3
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

/// Weight of a millisecond of latency against a millisecond of host CPU in [`score`].
pub(crate) const LATENCY_WEIGHT: f32 = 0.5;

/// A route's cost per frame, lower is better: host CPU time plus half the time it adds between
/// sensor and consumer. A hardware block (ISP, hardware decoder) that takes a few milliseconds
/// but almost no CPU beats doing the same work on the CPU; between two CPU routes the faster
/// wins; the capture's own latency counts, so of two modes of one camera the one delivering
/// sooner wins when CPU is equal.
pub(crate) fn score(total: StepCost) -> f32 {
    total.cpu_ms + LATENCY_WEIGHT * total.latency_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_software_isp_uses_half_the_cores_by_default() {
        // Three or four threads on a 4-core CM5 hung the board (see `softisp_threads_for`).
        assert_eq!(softisp_threads_for(4), 2);
        assert_eq!(softisp_threads_for(1), 1);
        assert_eq!(softisp_threads_for(2), 1);
        assert_eq!(softisp_threads_for(6), 3);
        assert_eq!(softisp_threads_for(16), 4);
        assert!(default_softisp_threads() >= 1);
    }

    #[test]
    fn hardware_blocks_beat_the_cpu_doing_the_same_work() {
        // A hardware decode adding 3 ms but taking 0.3 ms of CPU, against 2 ms on the CPU.
        let software = StepCost::cpu(2.0);
        let hardware = StepCost::offloaded(3.0, 0.3);
        assert!(score(hardware) < score(software));
        // Between CPU routes, the faster.
        assert!(score(StepCost::cpu(1.0)) < score(StepCost::cpu(1.5)));
    }

    #[test]
    fn uvc_latency_scales_with_frame_interval() {
        assert!(uvc_capture_latency_ms(Some(30.0)) < uvc_capture_latency_ms(Some(10.0)));
        assert!((uvc_capture_latency_ms(Some(25.0)) - 32.0).abs() < 0.1);
    }
}
