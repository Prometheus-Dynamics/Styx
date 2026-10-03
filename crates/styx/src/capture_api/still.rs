//! Still capture on a running capture: [`CaptureHandle::capture_still`].
//!
//! On a processed native camera the stream keeps running: the raw frame (or, for an
//! exposure bracket, one raw frame per exposure, each landing on a frame the control schedule
//! names) is copied out of the stream and reprocessed on a thread of its own at full quality
//! (the PiSP back end on its second node group with stronger denoise and no temporal denoise,
//! or the software ISP with the Malvar-He-Cutler demosaic), optionally written as a DNG with
//! the tuning's colour calibration. Other captures hand over the next frame they deliver.
//! See `docs/stills-and-dng.md`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_capture::prelude::*;
use styx_codec::Codec;

use super::handle::CaptureHandle;
use super::request::CaptureError;

/// What a still is delivered as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillFormat {
    /// JPEG (`MJPG`) at a quality of 1-100, encoded by the codec registry's RGB → MJPG
    /// encoder (mozjpeg, turbojpeg, FFmpeg), [`StillRequest::encoder`], or the `image`
    /// crate's (feature `image`).
    Jpeg {
        /// Quality, 1-100.
        quality: u8,
    },
    /// NV12 (full-range BT.601).
    Nv12,
    /// 8-bit RGB (`RG24`).
    Rgb24,
    /// The raw frame: 16-bit little-endian samples at the sensor's bit depth (`BG10`,
    /// `RG12`, ...). With a DNG this is usually not needed.
    Raw,
}

/// Which exposure the still is taken at.
#[derive(Clone, Debug, PartialEq)]
pub enum StillExposure {
    /// What AE is using (with [`StillRequest::settle`]: once AE has locked).
    Current,
    /// This exposure time and analogue gain (AE is handed back afterwards). The still is the
    /// first frame they land on.
    Fixed {
        /// Exposure time.
        exposure: Duration,
        /// Analogue gain.
        gain: f64,
    },
    /// One still per exposure value offset (stops) from AE's current total exposure, each on
    /// the frame its exposure lands on (consecutive frames where the sensor allows).
    Bracket(Vec<f64>),
}

/// A still request. [`Default`]: a JPEG at quality 90 of the next frame at AE's exposure.
#[derive(Clone)]
pub struct StillRequest {
    /// The delivered image's format.
    pub format: StillFormat,
    /// Also write a DNG of the raw frame (native cameras).
    pub dng: bool,
    /// The exposure.
    pub exposure: StillExposure,
    /// With [`StillExposure::Current`]: wait until AE is locked (converged).
    pub settle: bool,
    /// Spatial and colour denoise strength relative to the stream's (2: twice the
    /// thresholds; the preview's temporal denoise is not available to a single frame).
    pub denoise: f64,
    /// Embed an sRGB preview (at most 640 pixels wide) in the DNG.
    pub dng_preview: bool,
    /// Put the lens shading table into the DNG (`GainMap` opcodes).
    pub dng_lens_shading: bool,
    /// How long to wait for the still(s).
    pub timeout: Duration,
    /// The JPEG encoder (RG24 → MJPG) to use instead of the registry's.
    pub encoder: Option<Arc<dyn Codec>>,
}

impl std::fmt::Debug for StillRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StillRequest")
            .field("format", &self.format)
            .field("dng", &self.dng)
            .field("exposure", &self.exposure)
            .field("settle", &self.settle)
            .field("denoise", &self.denoise)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Default for StillRequest {
    fn default() -> Self {
        Self {
            format: StillFormat::Jpeg { quality: 90 },
            dng: false,
            exposure: StillExposure::Current,
            settle: false,
            denoise: 2.0,
            dng_preview: true,
            dng_lens_shading: true,
            timeout: Duration::from_secs(5),
            encoder: None,
        }
    }
}

impl StillRequest {
    /// A JPEG still.
    pub fn jpeg(quality: u8) -> Self {
        Self {
            format: StillFormat::Jpeg {
                quality: quality.clamp(1, 100),
            },
            ..Self::default()
        }
    }

    /// A still in `format`.
    pub fn format(format: StillFormat) -> Self {
        Self {
            format,
            ..Self::default()
        }
    }

    /// Also write a DNG.
    pub fn with_dng(mut self, dng: bool) -> Self {
        self.dng = dng;
        self
    }

    /// At a fixed exposure time and gain.
    pub fn fixed(mut self, exposure: Duration, gain: f64) -> Self {
        self.exposure = StillExposure::Fixed { exposure, gain };
        self
    }

    /// One still per exposure value offset (stops).
    pub fn bracket(mut self, ev: impl IntoIterator<Item = f64>) -> Self {
        self.exposure = StillExposure::Bracket(ev.into_iter().collect());
        self
    }

    /// Wait for AE to lock first.
    pub fn settle(mut self, settle: bool) -> Self {
        self.settle = settle;
        self
    }

    /// Denoise strength relative to the stream's.
    pub fn denoise(mut self, scale: f64) -> Self {
        self.denoise = scale.max(0.0);
        self
    }

    /// How long to wait.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The JPEG encoder to use.
    pub fn encoder(mut self, encoder: Arc<dyn Codec>) -> Self {
        self.encoder = Some(encoder);
        self
    }
}

/// A still image, tightly packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StillImage {
    /// `MJPG`, `NV12`, `RG24` or a 16-bit Bayer code (`BG10`, ...).
    pub format: FourCc,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// The bytes (a JPEG file for `MJPG`).
    pub data: Vec<u8>,
}

/// What produced a still.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StillMeta {
    /// Frame sequence.
    pub sequence: u64,
    /// Capture timestamp (`CLOCK_MONOTONIC`), nanoseconds.
    pub timestamp_ns: u64,
    /// Exposure time.
    pub exposure: Duration,
    /// Analogue gain.
    pub analogue_gain: f64,
    /// The sensor's digital gain.
    pub sensor_digital_gain: f64,
    /// The ISP's digital gain (green; on top of the sensor's).
    pub isp_digital_gain: f64,
    /// White balance gains (red, green, blue).
    pub colour_gains: [f64; 3],
    /// AWB's colour temperature (kelvin).
    pub colour_temperature: f64,
    /// Estimated illuminance (lux).
    pub lux: f64,
    /// The bracket's exposure offset (stops; 0 otherwise).
    pub ev: f64,
    /// The frame the exposure was to land on (fixed and bracketed exposures).
    pub target_frame: Option<u64>,
    /// The still is that frame and was produced with the asked exposure.
    pub landed: bool,
    /// The exposure and gain were read back from the frame (embedded data).
    pub verified: bool,
    /// What processed it: `pisp`, `software` or `stream` (the capture's own frame).
    pub isp: &'static str,
    /// Time reprocessing (and encoding) took.
    pub process_time: Duration,
}

/// One still.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StillShot {
    /// The image, unless the request asked for none (raw format without one).
    pub image: Option<StillImage>,
    /// The DNG file, if asked for.
    pub dng: Option<Vec<u8>>,
    /// What produced it.
    pub meta: StillMeta,
}

impl StillShot {
    /// Writes the image (a `.jpg` for JPEG; raw bytes otherwise).
    pub fn save_image(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match &self.image {
            Some(i) => std::fs::write(path, &i.data),
            None => Err(std::io::Error::other("the still has no image")),
        }
    }

    /// Writes the DNG.
    pub fn save_dng(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match &self.dng {
            Some(d) => std::fs::write(path, d),
            None => Err(std::io::Error::other("the still has no DNG")),
        }
    }
}

/// The result of [`CaptureHandle::capture_still`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StillCapture {
    /// One shot, or one per bracket exposure in the request's order.
    pub shots: Vec<StillShot>,
    /// From the request to the last shot's files being ready.
    pub latency: Duration,
}

impl CaptureHandle {
    /// Takes a still (or an exposure bracket) without stopping the stream where the camera
    /// allows: on processed native cameras the preview keeps its rate and the raw frame is
    /// reprocessed at full quality on another thread (see the [module docs](self)); other
    /// captures hand over the next frame they deliver (taken from the capture's queue, so the
    /// consumer misses that frame) at the running mode, converted to the format asked for.
    /// Blocks until the still is ready or [`StillRequest::timeout`].
    pub fn capture_still(&self, request: &StillRequest) -> Result<StillCapture, CaptureError> {
        let started = Instant::now();
        #[cfg(feature = "native")]
        if let Some(lc) = native_loop(&self.control)? {
            let (tx, rx) = std::sync::mpsc::channel();
            lc.submit_still(super::native_isp::StillJob {
                request: request.clone(),
                reply: tx,
                requested: started,
            });
            return match rx.recv_timeout(request.timeout + Duration::from_millis(500)) {
                Ok(r) => r,
                Err(_) => Err(CaptureError::Backend(
                    "still: no answer from the capture (stopped or timed out)".into(),
                )),
            };
        }
        super::still_output::from_stream(self, request, started)
    }
}

/// The 3A loop of a processed native capture (through a supervised capture's current one).
#[cfg(feature = "native")]
fn native_loop(
    plane: &super::ControlPlane,
) -> Result<Option<Arc<super::native_isp::LoopControls>>, CaptureError> {
    use super::ControlPlane;
    Ok(match plane {
        ControlPlane::Native {
            processed: Some(lc),
            ..
        } => Some(Arc::clone(lc)),
        ControlPlane::Supervised(shared) => native_loop(&shared.current_control()?)?,
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_build() {
        let r = StillRequest::jpeg(200)
            .with_dng(true)
            .bracket([-1.0, 0.0, 1.0]);
        assert_eq!(r.format, StillFormat::Jpeg { quality: 100 });
        assert!(r.dng);
        assert_eq!(r.exposure, StillExposure::Bracket(vec![-1.0, 0.0, 1.0]));
        let f = StillRequest::format(StillFormat::Raw).fixed(Duration::from_millis(10), 2.0);
        assert!(format!("{f:?}").contains("Fixed"));
        assert_eq!(StillRequest::default().denoise(-1.0).denoise, 0.0);
    }
}
