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
    /// A sensor driven by Styx itself (the native stack).
    Native(NativeFrameMeta),
}

impl BackendFrameMeta {
    pub fn as_v4l2(&self) -> Option<&V4l2FrameMeta> {
        match self {
            Self::V4l2(meta) => Some(meta),
            _ => None,
        }
    }

    pub fn as_libcamera(&self) -> Option<&LibcameraFrameMeta> {
        match self {
            Self::Libcamera(meta) => Some(meta),
            _ => None,
        }
    }

    pub fn as_native(&self) -> Option<&NativeFrameMeta> {
        match self {
            Self::Native(meta) => Some(meta),
            _ => None,
        }
    }

    /// Driver frame sequence number, when the backend reports one.
    pub fn sequence(&self) -> u32 {
        match self {
            Self::V4l2(meta) => meta.sequence,
            Self::Libcamera(meta) => meta.sequence,
            Self::Native(meta) => meta.sequence,
        }
    }
}

/// Per-frame metadata of a sensor Styx drives itself: the buffer, and the exposure, gain and
/// frame timing that produced the frame (predicted from the control schedule, or read back from
/// the frame's embedded data when `verified`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeFrameMeta {
    /// Frame sequence number from the receiver (0 at stream start).
    pub sequence: u32,
    /// Payload bytes.
    pub bytes_used: u32,
    /// The receiver flagged the frame as corrupted.
    pub error: bool,
    /// Exposure time in nanoseconds.
    pub exposure_ns: u64,
    /// Analogue gain.
    pub analog_gain: f32,
    /// Digital gain (1 without one).
    pub digital_gain: f32,
    /// Frame duration in nanoseconds.
    pub frame_duration_ns: u64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// The values were read back from the frame rather than predicted.
    pub verified: bool,
}

impl NativeFrameMeta {
    /// Total gain.
    pub fn gain(&self) -> f32 {
        self.analog_gain * self.digital_gain
    }
}

impl PartialEq for NativeFrameMeta {
    fn eq(&self, o: &Self) -> bool {
        (
            self.sequence,
            self.bytes_used,
            self.error,
            self.exposure_ns,
            self.analog_gain.to_bits(),
            self.digital_gain.to_bits(),
            self.frame_duration_ns,
            self.frame_length,
            self.verified,
        ) == (
            o.sequence,
            o.bytes_used,
            o.error,
            o.exposure_ns,
            o.analog_gain.to_bits(),
            o.digital_gain.to_bits(),
            o.frame_duration_ns,
            o.frame_length,
            o.verified,
        )
    }
}

impl Eq for NativeFrameMeta {}

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
    /// When this frame is a region-of-interest view: the region in full-frame coordinates.
    pub crop: Option<crate::requirements::FrameRect>,
    /// Clock `timestamp` is expressed in, when the backend reports it.
    pub clock: Option<TimestampClock>,
    /// An inter-coded packet (H.264/H.265 P-frame): decoding it needs the packets before it, back
    /// to the last keyframe. `false` for raw frames, JPEG and keyframes.
    pub delta: bool,
}

/// Clock a frame's `timestamp` is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TimestampClock {
    /// `CLOCK_MONOTONIC`: steady, stops during suspend (V4L2 buffers, `std::time::Instant`).
    Monotonic,
    /// `CLOCK_BOOTTIME`: steady, keeps counting during suspend (libcamera, Android sensors).
    Boottime,
    /// `CLOCK_REALTIME`: wall-clock nanoseconds since the Unix epoch; can jump.
    Realtime,
    /// Nanoseconds since the capture stream started (file, network and synthetic sources).
    StreamRelative,
}

impl TimestampClock {
    /// Current time on this clock in nanoseconds; `None` for stream-relative time or when the
    /// clock is unavailable on this platform.
    pub fn now_ns(self) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let clock = match self {
                Self::Monotonic => libc::CLOCK_MONOTONIC,
                Self::Boottime => libc::CLOCK_BOOTTIME,
                Self::Realtime => libc::CLOCK_REALTIME,
                Self::StreamRelative => return None,
            };
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is valid writable storage for clock_gettime.
            if unsafe { libc::clock_gettime(clock, &mut now) } != 0 {
                return None;
            }
            (now.tv_sec as u64)
                .checked_mul(1_000_000_000)?
                .checked_add(now.tv_nsec as u64)
        }
        #[cfg(not(target_os = "linux"))]
        {
            match self {
                Self::Realtime => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_nanos() as u64),
                _ => None,
            }
        }
    }

    /// Time elapsed since `timestamp_ns` on this clock, or `None` if the clock is unavailable
    /// or the timestamp lies in the future.
    pub fn elapsed_since(self, timestamp_ns: u64) -> Option<std::time::Duration> {
        self.now_ns()?
            .checked_sub(timestamp_ns)
            .map(std::time::Duration::from_nanos)
    }
}

/// Which clock capture backends should stamp frames with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClockSource {
    /// Keep each backend's own clock (libcamera: boottime, V4L2: usually monotonic, files and
    /// network streams: stream-relative). `FrameMeta::clock` says which one.
    #[default]
    Native,
    Monotonic,
    Boottime,
    Realtime,
}

impl ClockSource {
    pub fn clock(self) -> Option<TimestampClock> {
        match self {
            Self::Native => None,
            Self::Monotonic => Some(TimestampClock::Monotonic),
            Self::Boottime => Some(TimestampClock::Boottime),
            Self::Realtime => Some(TimestampClock::Realtime),
        }
    }
    /// Conversion from a backend's `native` clock to this source; `None` for `Native` or when
    /// the clocks cannot be related (stream-relative time).
    pub fn conversion_from(self, native: TimestampClock) -> Option<ClockConversion> {
        ClockConversion::new(native, self.clock()?)
    }

    /// Timestamp for a frame arriving now on a source without its own clock: the configured
    /// clock's current time, or `stream_elapsed` as stream-relative time for `Native`.
    pub fn stamp_now(self, stream_elapsed: std::time::Duration) -> (u64, TimestampClock) {
        self.clock()
            .and_then(|clock| Some((clock.now_ns()?, clock)))
            .unwrap_or((
                stream_elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
                TimestampClock::StreamRelative,
            ))
    }
}

/// A fixed offset between two system clocks, sampled once so related timestamps (e.g. a frame
/// and its pyramid companion) convert identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockConversion {
    target: TimestampClock,
    offset_ns: i128,
}

impl ClockConversion {
    /// Conversion from `from` to `to`; `None` if either is stream-relative or unavailable.
    pub fn new(from: TimestampClock, to: TimestampClock) -> Option<Self> {
        let offset_ns = if from == to {
            0
        } else {
            i128::from(to.now_ns()?) - i128::from(from.now_ns()?)
        };
        Some(Self {
            target: to,
            offset_ns,
        })
    }

    pub fn target(&self) -> TimestampClock {
        self.target
    }

    pub fn apply(&self, timestamp_ns: u64) -> u64 {
        (i128::from(timestamp_ns) + self.offset_ns).clamp(0, i128::from(u64::MAX)) as u64
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
            crop: None,
            clock: None,
            delta: false,
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

    pub fn native(&self) -> Option<&NativeFrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_native)
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
    pub fn with_sensor_latency(self, clock: TimestampClock) -> Self {
        let timestamp = self.timestamp;
        self.with_sensor_latency_from(clock, timestamp)
    }

    /// Like [`FrameMeta::with_sensor_latency`] for backends whose sensor timestamp differs from
    /// the frame's buffer timestamp (e.g. libcamera `SensorTimestamp` vs. ISP completion time).
    pub fn with_sensor_latency_from(mut self, clock: TimestampClock, sensor_ns: u64) -> Self {
        self.timing.sensor_to_capture = clock.elapsed_since(sensor_ns);
        self
    }

    pub fn with_clock(mut self, clock: TimestampClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Record `native` as the timestamp's clock, converting the timestamp first when a
    /// conversion is given.
    pub fn in_clock(mut self, native: TimestampClock, conversion: Option<ClockConversion>) -> Self {
        match conversion {
            Some(conversion) => {
                self.timestamp = conversion.apply(self.timestamp);
                self.clock = Some(conversion.target());
            }
            None => self.clock = Some(native),
        }
        self
    }

    /// `timestamp` converted to `target`, when this frame's clock is known and both are system
    /// clocks. The offset is sampled now, so repeated calls can differ by a few nanoseconds.
    pub fn timestamp_in(&self, target: TimestampClock) -> Option<u64> {
        let conversion = ClockConversion::new(self.clock?, target)?;
        Some(conversion.apply(self.timestamp))
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
        // Stages that keep the input timestamp (decoders, transforms) keep its clock too.
        if self.clock.is_none() && self.timestamp == input.timestamp {
            self.clock = input.clock;
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

    #[test]
    fn stages_keep_the_clock_only_with_the_same_timestamp() {
        let input = meta().with_clock(TimestampClock::Boottime);
        let mut decoded = FrameMeta {
            timestamp: input.timestamp,
            ..meta()
        };
        decoded.inherit_capture_context(&input);
        assert_eq!(decoded.clock, Some(TimestampClock::Boottime));
        let mut retimed = FrameMeta {
            timestamp: input.timestamp + 1,
            ..meta()
        };
        retimed.inherit_capture_context(&input);
        assert_eq!(retimed.clock, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clock_conversion_round_trips_and_is_shared() {
        let now = TimestampClock::Monotonic.now_ns().unwrap();
        let meta = meta().with_clock(TimestampClock::Monotonic);
        let meta = FrameMeta {
            timestamp: now,
            ..meta
        };
        let in_boot = meta.timestamp_in(TimestampClock::Boottime).unwrap();
        let boot_now = TimestampClock::Boottime.now_ns().unwrap();
        assert!(boot_now.abs_diff(in_boot) < 50_000_000);
        let conversion =
            ClockConversion::new(TimestampClock::Boottime, TimestampClock::Realtime).unwrap();
        assert_eq!(conversion.apply(10) - conversion.apply(0), 10);
        assert!(
            ClockConversion::new(TimestampClock::StreamRelative, TimestampClock::Monotonic)
                .is_none()
        );
        assert!(meta.timestamp_in(TimestampClock::Monotonic) == Some(now));
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
