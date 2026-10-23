//! Simulated cameras (feature `sim`): native cameras whose sensor, bridge and capture queue are
//! simulated, listed by [`probe_all`](crate::probe_all) like the device's and served by a
//! [`CameraService`](crate::ipc::CameraService) unchanged. The sensor driver, its control
//! schedule (the description's exposure and gain ranges, clamping, delays, frame rate limits
//! per mode), flips, the software ISP with the 3A loop and every consumer above run as on the
//! device; frames are rendered from a test chart or a raw recording (feature `replay-mcap` or
//! `replay-styxrec`) at each frame's exposure and gain. Faults are injected on request: lost
//! frames, timing jitter, corrupted frames, capture errors, a sensor that disappears and comes
//! back. See `docs/simulation.md`.
//!
//! Cameras come from `STYX_SIM_CAMERAS` or `/etc/styx/sim-cameras` (one [`SimSpec`] per entry,
//! e.g. `ov9782,scene=chart` or `ov9782,recording=/data/clip.mcap,drop=0.01`), keyed
//! `sim:<sensor>:<n>`, or from the program with [`device`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use styx_native::SensorLibrary;
use styx_native::discover::discover_key;
pub use styx_native::sim::{
    ChartScene, FlatScene, SIM_ENV, SIM_FILE, SceneFrame, SimFaults, SimScene, SimSource, SimSpec,
    parse_specs, register, specs,
};

use crate::ProbedDevice;

/// A simulated camera for this process (registered, numbered after the others of its sensor),
/// probed as a native camera: open it with the planner, a capture request or a camera service.
pub fn device(spec: SimSpec) -> Result<ProbedDevice, String> {
    install_recording_loader();
    let spec = register(spec);
    let info = discover_key(&spec.key(), &SensorLibrary::system()).map_err(|e| e.to_string())?;
    Ok(crate::capture_api::native_backend::probed_device(&info))
}

/// [`device`] from a spec's text (see [`SimSpec::parse`]).
pub fn device_from(text: &str) -> Result<ProbedDevice, String> {
    device(SimSpec::parse(text)?)
}

/// Lets simulated cameras play recordings (the replay readers this build has).
pub(crate) fn install_recording_loader() {
    #[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
    styx_native::sim::set_recording_loader(|path| {
        RecordingScene::open(path).map(|s| Arc::new(s) as Arc<dyn SimScene>)
    });
}

/// A raw recording as a scene: each recorded frame with its exposure, gain and black level
/// taken out (the light that made it), looped; frame `n` of a stream is recorded frame `n`.
#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
struct RecordingScene {
    path: std::path::PathBuf,
    /// Black level of the recorded sensor, as a fraction of full scale.
    black: f32,
    state: Mutex<RecordingState>,
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
struct RecordingState {
    frames: Option<crate::replay::RecordingFrames>,
    /// The index of the next frame `frames` gives.
    next: u64,
    /// Frames in the recording, once known (a full pass).
    len: Option<u64>,
    last: Option<(u64, Arc<SceneFrame>)>,
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
impl RecordingScene {
    fn open(path: &Path) -> Result<Self, String> {
        let (header, frames) = crate::replay::open_recording(path).map_err(|e| e.to_string())?;
        if styx_softisp::RawFormat::from_fourcc(
            header.format.code,
            header.format.resolution.width.get(),
            header.format.resolution.height.get(),
        )
        .is_none()
        {
            return Err(format!(
                "{} frames are not raw Bayer (a simulated camera plays raw recordings)",
                header.format.code
            ));
        }
        // The recorded sensor's black level, from its description (the first word of the
        // recorded camera's name, e.g. `ov9782 (kernel driver i2c 10-0060)`).
        let sensor = header
            .device
            .display
            .split_whitespace()
            .next()
            .unwrap_or("");
        let black = SensorLibrary::system()
            .find(sensor)
            .ok()
            .and_then(|d| d.pixel_array.black_level)
            .map_or(0.0, |b| b.value as f32 / ((1u32 << b.bits) - 1) as f32);
        Ok(Self {
            path: path.to_path_buf(),
            black,
            state: Mutex::new(RecordingState {
                frames: Some(frames),
                next: 0,
                len: None,
                last: None,
            }),
        })
    }

    /// Recorded frame `index` (looped) as the light that made it.
    fn light(&self, index: u64) -> Option<Arc<SceneFrame>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let index = s.len.map_or(index, |n| index % n.max(1));
        if let Some((i, f)) = &s.last
            && *i == index
        {
            return Some(Arc::clone(f));
        }
        if index < s.next || s.frames.is_none() {
            s.frames = crate::replay::open_recording(&self.path)
                .ok()
                .map(|(_, f)| f);
            s.next = 0;
        }
        loop {
            let frame = match s.frames.as_mut()?.next() {
                Some(Ok(f)) => f,
                Some(Err(_)) => return s.last.as_ref().map(|(_, f)| Arc::clone(f)),
                None => {
                    // The end: the length is known; play from the start.
                    if s.next == 0 {
                        return None;
                    }
                    s.len = Some(s.next);
                    drop(s);
                    return self.light(index);
                }
            };
            let i = s.next;
            s.next += 1;
            if i == index {
                let light = Arc::new(self.to_light(&frame)?);
                s.last = Some((i, Arc::clone(&light)));
                return Some(light);
            }
        }
    }

    fn to_light(&self, frame: &styx_core::prelude::FrameLease) -> Option<SceneFrame> {
        let meta = frame.meta();
        let (w, h) = (
            meta.format.resolution.width.get(),
            meta.format.resolution.height.get(),
        );
        let raw = styx_softisp::RawFormat::from_fourcc(meta.format.code, w, h)?;
        let native = meta.native().copied().unwrap_or_default();
        let exposure = if native.exposure_ns > 0 {
            native.exposure_ns as f32 * 1e-9
        } else {
            0.010
        };
        let gain = (native.analog_gain * native.digital_gain).max(1.0);
        let full = ((1u32 << raw.packing.bit_depth()) - 1) as f32;
        let stride = frame.layout_slice()[0].stride;
        let planes = frame.planes();
        let data = planes.first()?.data();
        let mut row = vec![0u16; w as usize];
        let mut light = Vec::with_capacity((w * h) as usize);
        let k = 1.0 / ((1.0 - self.black).max(1e-3) * exposure * gain);
        for y in 0..h as usize {
            let line = data.get(y * stride..)?;
            styx_pipeline::rawrec::unpack_row(raw.packing, line, w as usize, &mut row);
            light.extend(
                row.iter()
                    .map(|v| ((f32::from(*v) / full - self.black).max(0.0)) * k),
            );
        }
        let order = match raw.pattern {
            styx_softisp::CfaPattern::Bggr => styx_native::styx_sensor::ColorFilter::Bggr,
            styx_softisp::CfaPattern::Gbrg => styx_native::styx_sensor::ColorFilter::Gbrg,
            styx_softisp::CfaPattern::Grbg => styx_native::styx_sensor::ColorFilter::Grbg,
            styx_softisp::CfaPattern::Rggb => styx_native::styx_sensor::ColorFilter::Rggb,
        };
        Some(SceneFrame::Mosaic {
            width: w,
            height: h,
            order,
            light,
        })
    }
}

#[cfg(any(feature = "replay-mcap", feature = "replay-styxrec"))]
impl SimScene for RecordingScene {
    fn frame(&self, index: u64, width: u32, height: u32, _field: [f32; 4]) -> Arc<SceneFrame> {
        self.light(index).unwrap_or_else(|| {
            Arc::new(SceneFrame::Rgb {
                width,
                height,
                light: Arc::new(vec![[0.0; 3]; (width * height) as usize]),
                spot: None,
            })
        })
    }
}
