//! Raw recordings replayed as processed frames (`ReplaySourceConfig::soft_isp`): each raw
//! Bayer frame goes through the software ISP with the camera's tuning and the frame's
//! recorded state, so a pipeline sees frames comparable to the live camera's.
//!
//! The recorded exposure and gains are in the raw already; the white balance is the recorded
//! one (`NativeAaa::colour_gains`; AWB and AE do not run), and the colour matrix, lens shading
//! and tone curve are the tuning's for it. The frame keeps its recorded metadata (timestamp,
//! `NativeFrameMeta` and its 3A record); only its format becomes NV12.

use styx_capture::prelude::*;
use styx_pipeline::styx_algo::Controls;
use styx_pipeline::{SensorInfo, SensorValues, SoftLoop};
use styx_sensor::{BUILTIN_DESCRIPTIONS, SensorDescription};
use styx_softisp::{OutputBuffers, RawFormat, Scale};

use crate::replay::{ReplayError, ReplayIsp};

/// The software ISP of a replay, set up on its first raw frame.
pub(crate) struct ReplayProcessor {
    config: ReplayIsp,
    /// The loop, for frames of this format.
    isp: Option<(FourCc, u32, u32, SoftLoop)>,
}

fn err(e: impl std::fmt::Display) -> ReplayError {
    ReplayError::Frame(format!("replay ISP: {e}"))
}

impl ReplayProcessor {
    pub(crate) fn new(config: ReplayIsp) -> Self {
        Self { config, isp: None }
    }

    /// `frame` processed (NV12), or as it is when it is not raw Bayer.
    pub(crate) fn process(&mut self, frame: FrameLease) -> Result<FrameLease, ReplayError> {
        let format = frame.meta().format;
        let (w, h) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let Some(raw) = RawFormat::from_fourcc(format.code, w, h) else {
            return Ok(frame);
        };
        if self
            .isp
            .as_ref()
            .is_none_or(|(c, iw, ih, _)| (*c, *iw, *ih) != (format.code, w, h))
        {
            self.isp = Some((format.code, w, h, self.open(raw)?));
        }
        let (.., soft) = self.isp.as_mut().expect("set up above");
        let native = frame.meta().native().copied().unwrap_or_default();
        let sensor = SensorValues {
            frame: u64::from(native.sequence),
            exposure: std::time::Duration::from_nanos(native.exposure_ns.max(1)),
            analogue_gain: f64::from(native.analog_gain).max(1.0),
            digital_gain: f64::from(native.digital_gain).max(1.0),
            frame_duration: std::time::Duration::from_nanos(native.frame_duration_ns),
            verified: native.verified,
        };
        // The recorded state, not the loop's: no AE, the recorded white balance.
        let mut c: Controls = soft.controller().controls().clone();
        c.ae_enable = false;
        c.exposure = Some(sensor.exposure);
        c.analogue_gain = Some(sensor.analogue_gain);
        if let Some(a) = native.aaa {
            let g = f64::from(a.colour_gains[1]).max(1e-6);
            c.awb_enable = false;
            c.colour_gains = Some((
                f64::from(a.colour_gains[0]) / g,
                f64::from(a.colour_gains[2]) / g,
            ));
        }
        soft.controller().set_controls(c);
        let layout = frame.layout_slice()[0];
        let planes = frame.planes();
        let data = planes.first().ok_or_else(|| err("no plane"))?.data();
        let (w, h) = (w as usize, h as usize);
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        let (y, uv) = nv12.split_at_mut(w * h);
        soft.process(
            data,
            layout.stride,
            &sensor,
            Scale::Full,
            OutputBuffers::Nv12 {
                y,
                y_stride: w,
                uv,
                uv_stride: w,
            },
        )
        .map_err(err)?;
        drop(planes);
        let mut out = FrameLease::from_visible_bytes(
            MediaFormat::new(FourCc::NV12, format.resolution, ColorSpace::Srgb),
            frame.meta().timestamp,
            &nv12,
        )
        .map_err(err)?;
        let processed = out.meta().format;
        *out.meta_mut() = FrameMeta {
            format: processed,
            ..frame.meta().clone()
        };
        Ok(out)
    }

    /// The loop for `raw` frames of the configured sensor.
    fn open(&self, raw: RawFormat) -> Result<SoftLoop, ReplayError> {
        let name = &self.config.sensor;
        let (_, toml) = BUILTIN_DESCRIPTIONS
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| err(format!("no built-in description of sensor {name}")))?;
        let desc = SensorDescription::from_toml_str(toml, name).map_err(err)?;
        let mode = format!("{}x{}", raw.width, raw.height);
        let mut info = ["raw10", "raw12", "raw8"]
            .iter()
            .find_map(|f| SensorInfo::from_description(&desc, &mode, f).ok())
            .ok_or_else(|| err(format!("{name} has no {mode} mode")))?;
        // The recording's samples: its bit depth (16 for the PiSP's raw frames) and order.
        info.bits = raw.packing.bit_depth();
        info.cfa = raw.pattern;
        let (tuning, _) = styx_pipeline::tuning::find_tuning(&desc);
        let mut soft =
            SoftLoop::new(info, raw.packing, &tuning, self.config.threads.max(1)).map_err(err)?;
        soft.start().map_err(err)?;
        Ok(soft)
    }
}
