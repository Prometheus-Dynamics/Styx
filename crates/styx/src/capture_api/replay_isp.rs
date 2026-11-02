//! Raw recordings replayed as processed frames (`ReplaySourceConfig::soft_isp`): each raw
//! Bayer frame goes through the software ISP with the camera's tuning and the frame's
//! recorded state, so a pipeline sees frames comparable to the live camera's.
//!
//! The recorded exposure and gains are in the raw already; the white balance is the recorded
//! one (`NativeAaa::colour_gains`; AWB and AE do not run), and the colour matrix, lens shading
//! and tone curve are the tuning's for it. The frame keeps its recorded metadata (timestamp,
//! `NativeFrameMeta` and its 3A record); only its format becomes NV12.

use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_pipeline::{SensorInfo, SensorValues, SoftLoop};
use styx_sensor::SensorDescription;
use styx_softisp::{OutputBuffers, RawFormat, Scale};

use crate::replay::{ReplayError, ReplayIsp};

/// The built-in sensor descriptions (`styx_sensor::BUILTIN_DESCRIPTIONS`), compiled by the
/// build script: decoded without the TOML parser's code for them in the binary.
const COMPILED: &[(&str, &[u8])] = &[("ov9782", styx_sensor::include_description!("ov9782"))];

/// The software ISP of a replay, set up on its first raw frame.
pub(crate) struct ReplayProcessor {
    config: ReplayIsp,
    /// The loop, for frames of this format.
    isp: Option<(FourCc, u32, u32, SoftLoop)>,
    /// The NV12 frames' buffers (reused once consumers drop the frames) and planes.
    out: Option<(BufferPool, SmallVec<[PlaneLayout; 3]>)>,
}

/// Output buffers kept for reuse: more than the frames a consumer holds, per plane.
const KEEP_OUTPUTS: usize = 8;

fn err(e: impl std::fmt::Display) -> ReplayError {
    ReplayError::Frame(format!("replay ISP: {e}"))
}

impl ReplayProcessor {
    pub(crate) fn new(config: ReplayIsp) -> Self {
        Self {
            config,
            isp: None,
            out: None,
        }
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
            // Tight NV12 planes, each in a buffer of its own.
            let (w, h) = (w as usize, h as usize);
            let plane = |len| PlaneLayout {
                offset: 0,
                len,
                stride: w,
            };
            let layouts: SmallVec<[PlaneLayout; 3]> =
                smallvec::smallvec![plane(w * h), plane(w * h / 2)];
            let size = w * h;
            self.out = Some((
                BufferPool::with_limits(2 * layouts.len(), size, KEEP_OUTPUTS * layouts.len()),
                layouts,
            ));
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
        let c = soft.controller().controls_mut();
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
        let layout = frame.layout_slice()[0];
        let planes = frame.planes();
        let data = planes.first().ok_or_else(|| err("no plane"))?.data();
        let (pool, layouts) = self.out.as_ref().expect("set up with the loop");
        let [ly, luv] = [layouts[0], layouts[1]];
        let mut buffers: SmallVec<[BufferLease; 3]> = layouts
            .iter()
            .map(|l| {
                let mut b = pool.lease();
                b.resize(l.offset + l.len);
                b
            })
            .collect();
        let [by, buv] = buffers.as_mut_slice() else {
            return Err(err("NV12 has two planes"));
        };
        let processed = soft
            .process(
                data,
                layout.stride,
                &sensor,
                Scale::Full,
                OutputBuffers::Nv12 {
                    y: &mut by.as_mut_slice()[ly.offset..][..ly.len],
                    y_stride: ly.stride,
                    uv: &mut buv.as_mut_slice()[luv.offset..][..luv.len],
                    uv_stride: luv.stride,
                },
            )
            .map_err(err)?;
        soft.recycle(processed);
        drop(planes);
        let meta = FrameMeta {
            format: MediaFormat::new(FourCc::NV12, format.resolution, ColorSpace::Srgb),
            ..frame.meta().clone()
        };
        Ok(FrameLease::multi_plane(meta, buffers, layouts.clone()))
    }

    /// The loop for `raw` frames of the configured sensor.
    fn open(&self, raw: RawFormat) -> Result<SoftLoop, ReplayError> {
        let name = &self.config.sensor;
        let (_, bytes) = COMPILED
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| err(format!("no built-in description of sensor {name}")))?;
        let desc = SensorDescription::from_compiled(bytes).map_err(err)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in description is compiled in, and decodes to what its TOML says.
    #[test]
    fn the_compiled_descriptions_are_the_builtin_ones() {
        for (name, toml) in styx_sensor::BUILTIN_DESCRIPTIONS {
            let (_, bytes) = COMPILED
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("{name} is not compiled in"));
            assert_eq!(
                SensorDescription::from_compiled(bytes).unwrap(),
                SensorDescription::from_toml_str(toml, name).unwrap()
            );
        }
    }

    /// A replayed raw frame becomes an NV12 frame in a reused buffer: once warm, processing
    /// one allocates nothing on the replay's thread (the software ISP's helpers are counted by
    /// its own tests).
    #[test]
    fn a_replayed_frame_allocates_nothing_when_warm() {
        let (w, h) = (640u32, 400u32);
        let format = MediaFormat::new(
            FourCc::new(*b"BYR2"),
            Resolution::new(w, h).unwrap(),
            ColorSpace::Unknown,
        );
        let bytes: Vec<u8> = (0..w * h)
            .flat_map(|i| ((200 + i % 400) as u16 * 64).to_le_bytes())
            .collect();
        let raw = |i: u32| {
            let mut f =
                FrameLease::from_visible_bytes(format, u64::from(i) * 33_000_000, &bytes).unwrap();
            f.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
                sequence: i,
                exposure_ns: 8_000_000,
                analog_gain: 2.0,
                digital_gain: 1.0,
                frame_duration_ns: 33_333_333,
                ..Default::default()
            }));
            f
        };
        let mut isp = ReplayProcessor::new(ReplayIsp {
            sensor: "ov9782".into(),
            threads: 1,
        });
        for i in 0..20 {
            let out = isp.process(raw(i)).unwrap();
            assert_eq!(out.meta().format.code, FourCc::NV12);
        }
        let mut counted = 0;
        for i in 20..80 {
            let input = raw(i);
            let (out, n) = crate::test_alloc::allocations(|| isp.process(input).unwrap());
            assert_eq!(out.meta().timestamp, u64::from(i) * 33_000_000);
            let ((), dropped) = crate::test_alloc::allocations(|| drop(out));
            counted += n + dropped;
        }
        assert_eq!(counted, 0, "allocations over 60 replayed frames");
    }
}
