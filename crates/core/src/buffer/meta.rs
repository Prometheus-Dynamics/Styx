use std::{fmt, time::Instant};

use crate::format::MediaFormat;

/// Runtime-visible residency for a frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameResidency {
    HostOwned,
    HostExternal,
    Dmabuf,
    GpuTexture,
    CompressedPacket,
}

impl fmt::Display for FrameResidency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostOwned => write!(f, "host_owned"),
            Self::HostExternal => write!(f, "host_external"),
            Self::Dmabuf => write!(f, "dmabuf"),
            Self::GpuTexture => write!(f, "gpu_texture"),
            Self::CompressedPacket => write!(f, "compressed_packet"),
        }
    }
}

/// Mutability contract visible to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FrameMutability {
    #[default]
    Mutable,
    ReadOnly,
}

/// Reason a frame changed residency or had to be materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidencyTransitionReason {
    Capture,
    Decode,
    Encode,
    FrameHook,
    ImageHook,
    PackedTransform,
    ImageMaterialize,
    FileReplay,
    NetcamIngress,
    BackendFallbackCopy,
    Unknown,
}

impl fmt::Display for ResidencyTransitionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture => write!(f, "capture"),
            Self::Decode => write!(f, "decode"),
            Self::Encode => write!(f, "encode"),
            Self::FrameHook => write!(f, "frame_hook"),
            Self::ImageHook => write!(f, "image_hook"),
            Self::PackedTransform => write!(f, "packed_transform"),
            Self::ImageMaterialize => write!(f, "image_materialize"),
            Self::FileReplay => write!(f, "file_replay"),
            Self::NetcamIngress => write!(f, "netcam_ingress"),
            Self::BackendFallbackCopy => write!(f, "backend_fallback_copy"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Diagnostic record describing the last residency transition on a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResidencyTransition {
    pub from: FrameResidency,
    pub to: FrameResidency,
    pub reason: ResidencyTransitionReason,
    pub copied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendFrameMeta {
    V4l2(V4l2FrameMeta),
    Libcamera(LibcameraFrameMeta),
}

impl BackendFrameMeta {
    pub fn as_v4l2(&self) -> Option<&V4l2FrameMeta> {
        match self {
            Self::V4l2(meta) => Some(meta),
            Self::Libcamera(_) => None,
        }
    }

    pub fn as_libcamera(&self) -> Option<&LibcameraFrameMeta> {
        match self {
            Self::Libcamera(meta) => Some(meta),
            Self::V4l2(_) => None,
        }
    }

    /// Driver frame sequence number, when the backend reports one.
    pub fn sequence(&self) -> u32 {
        match self {
            Self::V4l2(meta) => meta.sequence,
            Self::Libcamera(meta) => meta.sequence,
        }
    }
}

/// libcamera per-frame metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibcameraFrameMeta {
    /// Frame sequence number from the capture device; gaps indicate dropped frames.
    pub sequence: u32,
    /// Where the capture buffer memory came from, e.g. `dma-heap` or `libcamera-allocator`.
    pub buffer_memory: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V4l2FrameMeta {
    pub sequence: u32,
    pub bytes_used: u32,
    pub field: u32,
    pub flags: u32,
    pub zero_copy: bool,
}

/// Metadata associated with a frame.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub format: MediaFormat,
    pub timestamp: u64,
    pub backend: Option<BackendFrameMeta>,
    pub capture_instant: Option<Instant>,
    pub residency: Option<FrameResidency>,
    pub mutability: FrameMutability,
    pub last_transition: Option<ResidencyTransition>,
    /// Where this frame's time went: capture latency and per-stage processing durations.
    pub timing: FrameTiming,
}

/// Clock a backend's frame timestamps are taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampClock {
    /// `CLOCK_MONOTONIC` (V4L2 buffers flagged `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC`).
    Monotonic,
    /// `CLOCK_BOOTTIME` (libcamera sensor timestamps).
    Boottime,
}

impl TimestampClock {
    /// Time elapsed since `timestamp_ns` on this clock, or `None` if the clock is unavailable
    /// or the timestamp lies in the future.
    pub fn elapsed_since(self, timestamp_ns: u64) -> Option<std::time::Duration> {
        #[cfg(target_os = "linux")]
        {
            let clock = match self {
                Self::Monotonic => libc::CLOCK_MONOTONIC,
                Self::Boottime => libc::CLOCK_BOOTTIME,
            };
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is valid writable storage for clock_gettime.
            if unsafe { libc::clock_gettime(clock, &mut now) } != 0 {
                return None;
            }
            let now_ns = (now.tv_sec as u64)
                .checked_mul(1_000_000_000)?
                .checked_add(now.tv_nsec as u64)?;
            now_ns
                .checked_sub(timestamp_ns)
                .map(std::time::Duration::from_nanos)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = timestamp_ns;
            None
        }
    }
}

/// Per-frame latency breakdown. Backends fill `sensor_to_capture`; pipelines fill the stage
/// durations. Stages a frame did not pass through stay `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameTiming {
    /// Sensor timestamp (typically start of exposure or of readout) → frame handed to Styx.
    pub sensor_to_capture: Option<std::time::Duration>,
    pub decode: Option<std::time::Duration>,
    pub transform: Option<std::time::Duration>,
    pub hook: Option<std::time::Duration>,
    pub encode: Option<std::time::Duration>,
}

impl FrameTiming {
    /// Sum of the recorded processing stages.
    pub fn processing(&self) -> std::time::Duration {
        [self.decode, self.transform, self.hook, self.encode]
            .into_iter()
            .flatten()
            .sum()
    }

    /// Keep `self`'s values and fill gaps from `earlier` (e.g. timing from before a decoder
    /// replaced the frame's metadata).
    pub fn merged_with(self, earlier: FrameTiming) -> FrameTiming {
        FrameTiming {
            sensor_to_capture: self.sensor_to_capture.or(earlier.sensor_to_capture),
            decode: self.decode.or(earlier.decode),
            transform: self.transform.or(earlier.transform),
            hook: self.hook.or(earlier.hook),
            encode: self.encode.or(earlier.encode),
        }
    }
}

/// Latency summary for a frame at the moment [`FrameMeta::latency`] is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLatency {
    /// Sensor → capture backend delivery.
    pub sensor_to_capture: Option<std::time::Duration>,
    /// Time since the backend delivered the frame (queues, processing, consumer).
    pub since_capture: Option<std::time::Duration>,
    /// Recorded pipeline processing time (decode + transform + hook + encode).
    pub processing: std::time::Duration,
    /// Sensor → now, when both parts are known ("glass to app").
    pub total: Option<std::time::Duration>,
}

impl FrameMeta {
    pub fn new(format: MediaFormat, timestamp: u64) -> Self {
        Self {
            format,
            timestamp,
            backend: None,
            capture_instant: None,
            residency: None,
            mutability: FrameMutability::Mutable,
            last_transition: None,
            timing: FrameTiming::default(),
        }
    }

    pub fn with_backend(mut self, backend: BackendFrameMeta) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn backend(&self) -> Option<&BackendFrameMeta> {
        self.backend.as_ref()
    }

    pub fn v4l2(&self) -> Option<&V4l2FrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_v4l2)
    }

    pub fn libcamera(&self) -> Option<&LibcameraFrameMeta> {
        self.backend
            .as_ref()
            .and_then(BackendFrameMeta::as_libcamera)
    }

    /// Driver frame sequence number, when the backend reports one.
    pub fn sequence(&self) -> Option<u32> {
        self.backend.as_ref().map(BackendFrameMeta::sequence)
    }

    pub fn with_capture_instant(mut self, capture_instant: Instant) -> Self {
        self.capture_instant = Some(capture_instant);
        self
    }

    pub fn capture_instant(&self) -> Option<Instant> {
        self.capture_instant
    }

    /// Record how long ago the sensor captured this frame, using `clock` for `timestamp`.
    pub fn with_sensor_latency(mut self, clock: TimestampClock) -> Self {
        self.timing.sensor_to_capture = clock.elapsed_since(self.timestamp);
        self
    }

    /// Latency summary as of now.
    pub fn latency(&self) -> FrameLatency {
        let since_capture = self.capture_instant.map(|at| at.elapsed());
        FrameLatency {
            sensor_to_capture: self.timing.sensor_to_capture,
            since_capture,
            processing: self.timing.processing(),
            total: self
                .timing
                .sensor_to_capture
                .zip(since_capture)
                .map(|(a, b)| a + b),
        }
    }

    /// Carry capture context (backend metadata, capture instant, timing) from the frame a
    /// stage consumed into the metadata of the frame it produced, without overwriting values
    /// the stage set itself.
    pub fn inherit_capture_context(&mut self, input: &FrameMeta) {
        if self.backend.is_none() {
            self.backend = input.backend.clone();
        }
        if self.capture_instant.is_none() {
            self.capture_instant = input.capture_instant;
        }
        self.timing = self.timing.merged_with(input.timing);
    }

    pub fn with_residency(mut self, residency: FrameResidency) -> Self {
        self.residency = Some(residency);
        self
    }

    pub fn residency(&self) -> Option<FrameResidency> {
        self.residency
    }

    pub fn with_mutability(mut self, mutability: FrameMutability) -> Self {
        self.mutability = mutability;
        self
    }

    pub fn mutability(&self) -> FrameMutability {
        self.mutability
    }

    pub fn with_transition(mut self, transition: ResidencyTransition) -> Self {
        self.last_transition = Some(transition);
        self
    }

    pub fn last_transition(&self) -> Option<ResidencyTransition> {
        self.last_transition
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
    use std::time::Duration;

    fn meta() -> FrameMeta {
        let res = Resolution::new(2, 2).unwrap();
        FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), 0)
    }

    #[test]
    fn inherit_keeps_stage_values_and_fills_gaps() {
        let mut input = meta().with_capture_instant(Instant::now());
        input.timing.sensor_to_capture = Some(Duration::from_millis(9));
        input.timing.decode = Some(Duration::from_millis(1));
        let mut output = meta();
        output.timing.decode = Some(Duration::from_millis(2));
        output.inherit_capture_context(&input);
        assert_eq!(
            output.timing.sensor_to_capture,
            Some(Duration::from_millis(9))
        );
        assert_eq!(output.timing.decode, Some(Duration::from_millis(2)));
        assert!(output.capture_instant.is_some());
        let latency = output.latency();
        assert_eq!(latency.processing, Duration::from_millis(2));
        assert!(latency.total.unwrap() >= Duration::from_millis(9));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn monotonic_elapsed_is_measured() {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        let ts = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
        let elapsed = TimestampClock::Monotonic.elapsed_since(ts).unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(TimestampClock::Monotonic.elapsed_since(u64::MAX).is_none());
    }
}
