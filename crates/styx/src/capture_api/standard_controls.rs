//! Standard controls for in-process callers: the camera service's `StandardControl` mapping,
//! shared, so `set_exposure_us`, `set_gain`, ... work on every backend in one set of units.
//!
//! A backend's control is found by its snake-case name (native and virtual cameras, e.g.
//! `exposure_time_us`), its libcamera name (`ExposureTime`) or, on V4L2 and UVC, its control id
//! (converted: V4L2 exposure counts 100 µs, stops are thousandths of a stop). The service
//! ([`crate::ipc`]) uses the same functions for its clients.

use std::sync::Arc;

use styx_core::prelude::*;

use super::control_plane::{ControlPlane, apply_control_landing, read_control_from_plane};
use super::{CaptureError, CaptureHandle};
use crate::BackendKind;
use crate::ipc::AfMode;
use crate::ipc::{AppliedControl, ControlRefusal, StandardControl};

/// How a standard control's units become the backend's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scale {
    Same,
    /// Microseconds to V4L2's 100 µs.
    Per100Us,
    /// Stops to V4L2's thousandths of a stop.
    Milli,
    /// AE on/off to V4L2's exposure menu: 3 aperture priority (auto), 1 manual.
    AeMenu,
    /// AF mode to V4L2's continuous autofocus on/off.
    AfAuto,
}

/// V4L2 (and UVC) control ids of the standard controls, with their units.
pub(crate) fn v4l2_cid(control: StandardControl) -> Option<(u32, Scale)> {
    Some(match control {
        StandardControl::ExposureUs => (0x009a_0902, Scale::Per100Us),
        StandardControl::Gain => (0x0098_0913, Scale::Same),
        StandardControl::AeEnable => (0x009a_0901, Scale::AeMenu),
        StandardControl::ExposureValue => (0x009a_0913, Scale::Milli),
        StandardControl::AwbEnable => (0x0098_090c, Scale::Same),
        StandardControl::ColourTemperature => (0x0098_091a, Scale::Same),
        StandardControl::RedGain => (0x0098_090e, Scale::Same),
        StandardControl::BlueGain => (0x0098_090f, Scale::Same),
        StandardControl::AfMode => (0x009a_090c, Scale::AfAuto),
        StandardControl::LensPosition => (0x009a_090a, Scale::Same),
        StandardControl::FrameRate | StandardControl::AfTrigger => return None,
    })
}

/// The backend's control for `control`, if the camera has one, and how its units convert.
pub(crate) fn find_standard(
    metas: &[ControlMeta],
    backend: BackendKind,
    control: StandardControl,
) -> Option<(ControlMeta, Scale)> {
    let named = |name: &str| metas.iter().find(|m| m.name == name).cloned();
    if let Some(meta) = named(control.name()) {
        return Some((meta, Scale::Same));
    }
    if let Some(meta) = control.libcamera_name().and_then(named) {
        return Some((meta, Scale::Same));
    }
    if matches!(backend, BackendKind::V4l2 | BackendKind::Uvc)
        && let Some((cid, scale)) = v4l2_cid(control)
    {
        return metas
            .iter()
            .find(|m| m.id.0 == cid)
            .map(|m| (m.clone(), scale));
    }
    None
}

pub(crate) fn number(v: &ControlValue) -> Option<f64> {
    match v {
        ControlValue::Bool(b) => Some(f64::from(u8::from(*b))),
        ControlValue::Int(i) => Some(f64::from(*i)),
        ControlValue::Uint(u) => Some(f64::from(*u)),
        ControlValue::Float(f) => Some(f64::from(*f)),
        _ => None,
    }
}

/// A standard control's value in the backend's units.
pub(crate) fn to_backend(scale: Scale, v: &ControlValue) -> ControlValue {
    let Some(n) = number(v) else {
        return v.clone();
    };
    match scale {
        Scale::Same => v.clone(),
        Scale::Per100Us => ControlValue::Int((n / 100.0).round() as i32),
        Scale::Milli => ControlValue::Int((n * 1000.0).round() as i32),
        Scale::AeMenu => ControlValue::Int(if n != 0.0 { 3 } else { 1 }),
        Scale::AfAuto => ControlValue::Bool(n != 0.0),
    }
}

/// [`to_backend`] for the control `meta` describes: automatic exposure on is the first
/// automatic mode its menu has (aperture priority, full auto, shutter priority; webcams often
/// offer only aperture priority, some cameras only full auto).
pub(crate) fn to_backend_for(meta: &ControlMeta, scale: Scale, v: &ControlValue) -> ControlValue {
    if scale == Scale::AeMenu && number(v).is_some_and(|n| n != 0.0) {
        let has = |m: i32| meta.validate(&ControlValue::Int(m));
        if let Some(mode) = [3, 0, 2].into_iter().find(|&m| has(m)) {
            return ControlValue::Int(mode);
        }
    }
    to_backend(scale, v)
}

/// A backend value in the standard control's units.
pub(crate) fn from_backend(scale: Scale, v: ControlValue) -> ControlValue {
    let Some(n) = number(&v) else {
        return v;
    };
    match scale {
        Scale::Same => v,
        Scale::Per100Us => ControlValue::Uint((n * 100.0).max(0.0) as u32),
        Scale::Milli => ControlValue::Float((n / 1000.0) as f32),
        Scale::AeMenu => ControlValue::Bool(n != 1.0),
        Scale::AfAuto => ControlValue::Int(if n != 0.0 { 2 } else { 0 }),
    }
}

/// `meta` in the standard control's units and type (`scale`), as a camera service lists it:
/// V4L2's exposure in 100 µs as µs, its exposure menu as automatic exposure on or off, its
/// thousandths of a stop as stops. `Scale::Same` leaves it as it is.
pub(crate) fn standard_meta(meta: &ControlMeta, scale: Scale) -> ControlMeta {
    let conv = |v: &ControlValue| from_backend(scale, v.clone());
    let scaled = |kind: ControlKind| ControlMeta {
        kind,
        min: conv(&meta.min),
        max: conv(&meta.max),
        default: conv(&meta.default),
        step: meta.step.as_ref().map(conv),
        menu: None,
        ..meta.clone()
    };
    match scale {
        Scale::Same => meta.clone(),
        Scale::Per100Us => scaled(ControlKind::Uint),
        Scale::Milli => scaled(ControlKind::Float),
        Scale::AeMenu => {
            let has = |m: i32| meta.validate(&ControlValue::Int(m));
            ControlMeta {
                kind: ControlKind::Bool,
                min: ControlValue::Bool(!has(1)),
                max: ControlValue::Bool([3, 0, 2].into_iter().any(has)),
                default: conv(&meta.default),
                step: None,
                menu: None,
                ..meta.clone()
            }
        }
        Scale::AfAuto => ControlMeta {
            kind: ControlKind::Int,
            min: ControlValue::Int(0),
            max: ControlValue::Int(2),
            default: conv(&meta.default),
            step: None,
            menu: None,
            ..meta.clone()
        },
    }
}

/// `value` as `meta`'s type, within its range and on its steps; whether it was clamped.
pub(crate) fn fit(
    meta: &ControlMeta,
    value: ControlValue,
) -> Result<(ControlValue, bool), ControlRefusal> {
    let invalid = |why: String| Err(ControlRefusal::Invalid(format!("{}: {why}", meta.name)));
    if matches!(meta.kind, ControlKind::Rectangle) || meta.menu.is_some() {
        return if meta.validate(&value) {
            Ok((value, false))
        } else {
            invalid(format!("{value:?} is not one of its values"))
        };
    }
    let Some(n) = number(&value) else {
        return match meta.kind {
            ControlKind::None | ControlKind::Unknown => Ok((value, false)),
            _ => invalid(format!("{value:?} is not a number")),
        };
    };
    let bound = |v: &ControlValue| number(v);
    let (lo, hi) = (bound(&meta.min), bound(&meta.max));
    let mut x = n;
    if let (Some(lo), Some(hi)) = (lo, hi)
        && lo <= hi
    {
        x = x.clamp(lo, hi);
        if let Some(step) = meta.step.as_ref().and_then(bound)
            && step > 0.0
            && !matches!(meta.kind, ControlKind::Float)
        {
            x = (lo + ((x - lo) / step).round() * step).min(hi);
        }
    }
    let fitted = match meta.kind {
        ControlKind::Bool => ControlValue::Bool(x != 0.0),
        ControlKind::Int | ControlKind::Menu | ControlKind::IntMenu => {
            ControlValue::Int(x.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
        }
        ControlKind::Uint => ControlValue::Uint(x.round().clamp(0.0, f64::from(u32::MAX)) as u32),
        ControlKind::Float => ControlValue::Float(x as f32),
        _ => value.clone(),
    };
    let clamped = number(&fitted).is_some_and(|f| (f - n).abs() > 1e-6 * n.abs().max(1.0));
    Ok((fitted, clamped))
}

/// The longest exposure, in µs, a native camera allows for the exposure control `id` at the frame
/// length in effect (the sensor's limit at that frame length, see `ControlHandle::exposure_range`);
/// `None` for other controls and backends.
#[cfg(feature = "native")]
fn native_exposure_limit_us(plane: &ControlPlane, id: ControlId) -> Option<f64> {
    if id != super::native_backend::controls::EXPOSURE_TIME_US {
        return None;
    }
    match plane {
        ControlPlane::Native { controls, .. } => controls
            .exposure_range()
            .map(|(_, max)| max.as_secs_f64() * 1e6),
        // A reconnecting capture (every planned capture): the running one's.
        ControlPlane::Supervised(shared) => {
            native_exposure_limit_us(&shared.current_control().ok()?, id)
        }
        _ => None,
    }
}

#[cfg(not(feature = "native"))]
fn native_exposure_limit_us(_plane: &ControlPlane, _id: ControlId) -> Option<f64> {
    None
}

/// An exposure in µs held to `max_us`: the value in effect, and whether it was held.
pub(crate) fn clamp_exposure_us(value: ControlValue, max_us: f64) -> (ControlValue, bool) {
    match number(&value) {
        Some(n) if n > max_us => (ControlValue::Uint(max_us.floor().max(0.0) as u32), true),
        _ => (value, false),
    }
}

/// Whether the backend reads back what was just set (others apply it with a later frame).
pub(crate) fn reads_back(backend: BackendKind) -> bool {
    matches!(
        backend,
        BackendKind::V4l2 | BackendKind::Uvc | BackendKind::Virtual
    )
}

/// The standard controls of one capture, for in-process callers. Cheap to clone and to send to
/// another thread (it holds the capture's control plane and the controls it lists), so a
/// consumer can change exposure or white balance off its frame thread.
///
/// Every setter takes the standard control's units (see [`StandardControl`]) and answers with
/// what is in effect now ([`AppliedControl`]): the value clamped to the control's range, and the
/// first frame that uses it on frame-exact backends (`NativeFrameMeta::sequence`).
///
/// That `frame` is a prediction on every sensor, not a read-back: the sensor's delay for the
/// control from the frame in progress (`pipeline.md`, controls, has the semantics). `None` where
/// no frame is fixed: AE, EV, AWB and colour settings on a processed mode, and any backend that
/// is not frame-exact.
///
/// Native processed modes (the PiSP or software ISP with the 3A loop): setting exposure or gain
/// fixes AE at that value and answers the frame it is predicted to take effect on;
/// [`Self::set_ae`] (`true`) hands exposure back to AE and answers `None`. A frame rate or
/// frame duration cannot change on a processed mode: [`Self::set_fps`] then returns an error, and
/// the capture has to be restarted at the new rate.
#[derive(Debug, Clone)]
pub struct StandardControls {
    plane: ControlPlane,
    backend: BackendKind,
    metas: Arc<[ControlMeta]>,
}

impl StandardControls {
    pub(crate) fn new(plane: ControlPlane, backend: BackendKind, metas: &[ControlMeta]) -> Self {
        Self {
            plane,
            backend,
            metas: metas.into(),
        }
    }

    /// Set a standard control. Errors: the camera has no such control (a frame rate it cannot
    /// change while streaming needs a restart), the control is read only, the value is not a
    /// usable number, or the backend refused it.
    pub fn set(
        &self,
        control: StandardControl,
        value: ControlValue,
    ) -> Result<AppliedControl, CaptureError> {
        let (meta, scale) = self.find(control)?;
        if meta.access == Access::ReadOnly {
            return Err(CaptureError::control_apply(format!(
                "{} is read only",
                meta.name
            )));
        }
        let (fitted, clamped) = fit(&meta, to_backend_for(&meta, scale, &value))
            .map_err(|refusal| CaptureError::control_apply(refusal.to_string()))?;
        // A native camera's longest exposure depends on the frame length in effect (the frame
        // less the sensor's margin): what is set above it is held to it, and reported as such.
        let (fitted, clamped) = match native_exposure_limit_us(&self.plane, meta.id) {
            Some(max_us) => {
                let (held, held_now) = clamp_exposure_us(fitted, max_us);
                (held, clamped || held_now)
            }
            None => (fitted, clamped),
        };
        let frame = apply_control_landing(&self.plane, meta.id, fitted.clone())?;
        let in_effect = match reads_back(self.backend)
            .then(|| read_control_from_plane(&self.plane, meta.id).ok())
            .flatten()
        {
            Some(now) => from_backend(scale, now),
            None => from_backend(scale, fitted),
        };
        Ok(AppliedControl {
            id: meta.id,
            requested: value,
            value: in_effect,
            clamped,
            frame,
            deferred: false,
            restarted: false,
        })
    }

    /// A standard control's value now, in its units.
    pub fn get(&self, control: StandardControl) -> Result<ControlValue, CaptureError> {
        let (meta, scale) = self.find(control)?;
        read_control_from_plane(&self.plane, meta.id).map(|v| from_backend(scale, v))
    }

    /// Exposure time in microseconds (turn automatic exposure off for it to hold). `frame` is
    /// the predicted first frame at this exposure (the sensor's delay from the frame in progress),
    /// `None` on a processed mode only when set to 0 (back to AE).
    pub fn set_exposure_us(&self, us: u32) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::ExposureUs, ControlValue::Uint(us))
    }

    /// Total gain as a ratio (1.0: none). `frame` is the predicted first frame at this gain,
    /// as [`Self::set_exposure_us`].
    pub fn set_gain(&self, gain: f32) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::Gain, ControlValue::Float(gain))
    }

    /// Automatic exposure on or off. `frame` is always `None`: AE chooses the frames' exposures.
    pub fn set_ae(&self, on: bool) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::AeEnable, ControlValue::Bool(on))
    }

    /// Exposure compensation in stops. `frame` is always `None`: AE applies it every frame.
    pub fn set_ev(&self, stops: f32) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::ExposureValue, ControlValue::Float(stops))
    }

    /// Frame rate through the camera's own control (raw modes on native cameras). Where the
    /// camera has none, or cannot change it while streaming, this is an error: restart the
    /// capture at the new rate.
    pub fn set_fps(&self, fps: f32) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::FrameRate, ControlValue::Float(fps))
    }

    /// Automatic white balance on or off.
    pub fn set_awb(&self, on: bool) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::AwbEnable, ControlValue::Bool(on))
    }

    /// White balance colour temperature in kelvin (used while AWB is off).
    pub fn set_colour_temperature(&self, kelvin: u32) -> Result<AppliedControl, CaptureError> {
        self.set(
            StandardControl::ColourTemperature,
            ControlValue::Uint(kelvin),
        )
    }

    /// Manual red and blue gains, relative to green (used while AWB is off).
    pub fn set_colour_gains(
        &self,
        red: f32,
        blue: f32,
    ) -> Result<(AppliedControl, AppliedControl), CaptureError> {
        Ok((
            self.set(StandardControl::RedGain, ControlValue::Float(red))?,
            self.set(StandardControl::BlueGain, ControlValue::Float(blue))?,
        ))
    }

    /// What drives the focus lens.
    pub fn set_af_mode(&self, mode: AfMode) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::AfMode, ControlValue::Int(mode as i32))
    }

    /// Start an autofocus scan ([`AfMode::Auto`]).
    pub fn trigger_af(&self) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::AfTrigger, ControlValue::Int(0))
    }

    /// Cancel an autofocus scan.
    pub fn cancel_af(&self) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::AfTrigger, ControlValue::Int(1))
    }

    /// Lens position in dioptres (0: infinity), in [`AfMode::Manual`].
    pub fn set_lens_position(&self, dioptres: f32) -> Result<AppliedControl, CaptureError> {
        self.set(StandardControl::LensPosition, ControlValue::Float(dioptres))
    }

    fn find(&self, control: StandardControl) -> Result<(ControlMeta, Scale), CaptureError> {
        find_standard(&self.metas, self.backend, control).ok_or_else(|| {
            CaptureError::control_apply(format!(
                "this camera has no {} control{}",
                control.name(),
                match control {
                    StandardControl::FrameRate => {
                        " (its frame rate cannot change while streaming: restart the capture)"
                    }
                    _ => "",
                }
            ))
        })
    }
}

impl CaptureHandle {
    /// The standard controls of this capture (see [`StandardControls`]).
    pub fn standard_controls(&self) -> StandardControls {
        StandardControls::new(self.control.clone(), self.backend, self.control_metas())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_api::{CaptureRequest, make_virtual_device_with_controls};
    use crate::prelude::{Interval, Mode};

    fn control(
        id: u32,
        name: &str,
        kind: ControlKind,
        min: ControlValue,
        max: ControlValue,
    ) -> ControlMeta {
        let default = min.clone();
        ControlMeta {
            id: ControlId(id),
            name: name.into(),
            kind,
            access: Access::ReadWrite,
            min,
            max,
            default,
            step: None,
            menu: None,
            metadata: ControlMetadata::default(),
        }
    }

    /// A virtual camera listing native-named controls (what a native camera's descriptor does).
    fn camera() -> CaptureHandle {
        let mode = Mode::with_interval(
            MediaFormat::srgb(FourCc::RG24, 64, 32).unwrap(),
            Interval::from_fps(30).unwrap(),
        );
        let metas = vec![
            control(
                1,
                "exposure_time_us",
                ControlKind::Uint,
                ControlValue::Uint(100),
                ControlValue::Uint(100_000),
            ),
            control(
                2,
                "gain",
                ControlKind::Float,
                ControlValue::Float(1.0),
                ControlValue::Float(16.0),
            ),
            control(
                3,
                "ae_enable",
                ControlKind::Bool,
                ControlValue::Bool(false),
                ControlValue::Bool(true),
            ),
            control(
                4,
                "colour_temperature",
                ControlKind::Uint,
                ControlValue::Uint(2000),
                ControlValue::Uint(10_000),
            ),
            control(
                5,
                "red_gain",
                ControlKind::Float,
                ControlValue::Float(0.0),
                ControlValue::Float(4.0),
            ),
            control(
                6,
                "blue_gain",
                ControlKind::Float,
                ControlValue::Float(0.0),
                ControlValue::Float(4.0),
            ),
        ];
        let device = make_virtual_device_with_controls("standard", [mode], metas);
        CaptureRequest::new(&device).start().unwrap()
    }

    #[test]
    fn an_exposure_above_the_sensors_limit_is_held_to_it() {
        // 640x400 at 240 fps on the OV9782: the frame less the sensor's 25-line margin allows
        // 3931 µs, and a request for 10 ms is in effect at that.
        assert_eq!(
            clamp_exposure_us(ControlValue::Uint(10_000), 3931.4),
            (ControlValue::Uint(3931), true)
        );
        assert_eq!(
            clamp_exposure_us(ControlValue::Uint(3931), 3931.4),
            (ControlValue::Uint(3931), false)
        );
        assert_eq!(
            clamp_exposure_us(ControlValue::Uint(2_000), 3931.4),
            (ControlValue::Uint(2_000), false)
        );
    }

    #[test]
    fn setters_apply_clamp_and_read_back() {
        let capture = camera();
        let controls = capture.standard_controls();
        let applied = controls.set_exposure_us(5_000).unwrap();
        assert_eq!(applied.id, ControlId(1));
        assert_eq!(applied.value, ControlValue::Uint(5_000));
        assert_eq!(applied.requested, ControlValue::Uint(5_000));
        assert!(!applied.clamped && !applied.deferred && !applied.restarted);
        assert_eq!(
            controls.get(StandardControl::ExposureUs).unwrap(),
            ControlValue::Uint(5_000)
        );

        // Out of range: clamped, and the answer says what is in effect.
        let high = controls.set_exposure_us(999_999).unwrap();
        assert!(high.clamped);
        assert_eq!(high.value, ControlValue::Uint(100_000));
        assert_eq!(
            capture.standard_controls().set_gain(2.5).unwrap().value,
            ControlValue::Float(2.5)
        );
        assert_eq!(
            controls.set_colour_temperature(4_000).unwrap().value,
            ControlValue::Uint(4_000)
        );
        assert_eq!(
            controls.set_ae(true).unwrap().value,
            ControlValue::Bool(true)
        );
        let (red, blue) = controls.set_colour_gains(1.5, 1.2).unwrap();
        assert_eq!((red.id, blue.id), (ControlId(5), ControlId(6)));
    }

    #[test]
    fn the_same_setters_go_through_the_capture_and_its_frames() {
        let capture = camera();
        capture.standard_controls().set_gain(4.0).unwrap();
        // A clone of the controls, used from another thread, sees the same capture.
        let remote = capture.standard_controls();
        std::thread::spawn(move || remote.set_exposure_us(7_000).unwrap())
            .join()
            .unwrap();
        assert_eq!(
            capture.get_control(ControlId(1)).unwrap(),
            ControlValue::Uint(7_000)
        );
        assert_eq!(
            capture.get_control(ControlId(2)).unwrap(),
            ControlValue::Float(4.0)
        );
    }

    #[test]
    fn missing_and_unusable_controls_are_errors_not_silence() {
        let capture = camera();
        let controls = capture.standard_controls();
        // A frame rate the camera has no control for needs a restart: an error, not a no-op.
        let fps = controls.set_fps(30.0).unwrap_err().to_string();
        assert!(
            fps.contains("frame_rate") && fps.contains("restart"),
            "{fps}"
        );
        // No lens, no AF.
        assert!(controls.set_lens_position(1.0).is_err());
        assert!(controls.trigger_af().is_err());
        // A value that is not a number for an integer control is refused.
        assert!(
            controls
                .set(StandardControl::ExposureUs, ControlValue::None)
                .is_err()
        );
        assert!(controls.get(StandardControl::Gain).is_ok());
        assert!(controls.get(StandardControl::AwbEnable).is_err());
    }

    #[test]
    fn v4l2_ids_convert_to_their_units() {
        let metas = [control(
            0x009a_0902,
            "Exposure Time, Absolute",
            ControlKind::Int,
            ControlValue::Int(1),
            ControlValue::Int(10_000),
        )];
        let (meta, scale) = find_standard(&metas, BackendKind::V4l2, StandardControl::ExposureUs)
            .expect("the V4L2 exposure by id");
        assert_eq!(meta.id, ControlId(0x009a_0902));
        assert_eq!(scale, Scale::Per100Us);
        // 5 ms in microseconds is 50 units of 100 µs; read back, 50 units is 5 ms again.
        assert_eq!(
            to_backend(scale, &ControlValue::Uint(5_000)),
            ControlValue::Int(50)
        );
        assert_eq!(
            from_backend(scale, ControlValue::Int(50)),
            ControlValue::Uint(5_000)
        );
        // Only by id on V4L2 and UVC: a virtual camera does not answer to it.
        assert!(find_standard(&metas, BackendKind::Virtual, StandardControl::ExposureUs).is_none());
    }

    /// A UVC camera's exposure menu as V4L2 lists it: only 1 (manual) and 3 (aperture
    /// priority), the other indexes empty names. Automatic exposure on is 3, a value it takes;
    /// a camera with only full auto gets 0; the gaps and values past the range are refused.
    #[test]
    fn sparse_exposure_menus_take_their_own_values() {
        let mut ae = control(
            0x009a_0901,
            "Auto Exposure",
            ControlKind::Menu,
            ControlValue::Uint(0),
            ControlValue::Uint(3),
        );
        ae.default = ControlValue::Uint(3);
        ae.menu = Some(vec![
            String::new(),
            "Manual Mode".into(),
            String::new(),
            "Aperture Priority Mode".into(),
        ]);
        for (v, ok) in [(0, false), (1, true), (2, false), (3, true), (4, false)] {
            assert_eq!(ae.validate(&ControlValue::Int(v)), ok, "{v}");
        }
        let on = to_backend_for(&ae, Scale::AeMenu, &ControlValue::Bool(true));
        assert_eq!(fit(&ae, on).unwrap(), (ControlValue::Int(3), false));
        let off = to_backend_for(&ae, Scale::AeMenu, &ControlValue::Bool(false));
        assert_eq!(fit(&ae, off).unwrap(), (ControlValue::Int(1), false));
        let mut full_auto = ae.clone();
        full_auto.menu = Some(vec!["Auto Mode".into(), "Manual Mode".into()]);
        full_auto.max = ControlValue::Uint(1);
        assert_eq!(
            to_backend_for(&full_auto, Scale::AeMenu, &ControlValue::Bool(true)),
            ControlValue::Int(0)
        );
        // Listed as automatic exposure on or off.
        let listed = standard_meta(&ae, Scale::AeMenu);
        assert_eq!(listed.kind, ControlKind::Bool);
        assert!(listed.menu.is_none());
        assert_eq!(
            (listed.min, listed.max, listed.default),
            (
                ControlValue::Bool(false),
                ControlValue::Bool(true),
                ControlValue::Bool(true)
            )
        );
    }

    /// V4L2's exposure in 100 µs is listed in microseconds.
    #[test]
    fn standard_meta_is_in_the_standard_units() {
        let mut exposure = control(
            0x009a_0902,
            "Exposure Time, Absolute",
            ControlKind::Int,
            ControlValue::Int(1),
            ControlValue::Int(10_000),
        );
        exposure.default = ControlValue::Int(166);
        exposure.step = Some(ControlValue::Uint(1));
        let listed = standard_meta(&exposure, Scale::Per100Us);
        assert_eq!(listed.kind, ControlKind::Uint);
        assert_eq!(
            (listed.min, listed.max, listed.default, listed.step),
            (
                ControlValue::Uint(100),
                ControlValue::Uint(1_000_000),
                ControlValue::Uint(16_600),
                Some(ControlValue::Uint(100))
            )
        );
        assert_eq!(listed.id, exposure.id);
        assert!(matches!(
            standard_meta(&exposure, Scale::Same).kind,
            ControlKind::Int
        ));
    }
}
