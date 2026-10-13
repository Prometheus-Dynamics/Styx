use std::collections::HashMap;

use libcamera::control::ControlList as LcControlList;
use styx_core::controls::{ControlId, ControlValue};

use crate::capture_api::CaptureError;
use crate::capture_api::LIBCAMERA_FRAME_DURATION_LIMITS;

use super::lcraw::{self, RawValue};
use super::util::{classify_libcamera_control_apply_message, to_lc_value};

/// libcamera's exposure and gain controls. The AE algorithm overwrites `ExposureTime` and
/// `AnalogueGain` every frame unless their mode says Manual (`ExposureTimeMode`,
/// `AnalogueGainMode`; the enums are Auto 0 and Manual 1 in libcamera 0.5 to 0.7). `AeEnable`
/// is libcamera's older switch for both: false sets both modes to manual, true to auto.
pub(super) const EXPOSURE_TIME: ControlId =
    ControlId(libcamera::controls::ControlId::ExposureTime as u32);
pub(super) const ANALOGUE_GAIN: ControlId =
    ControlId(libcamera::controls::ControlId::AnalogueGain as u32);
pub(super) const AE_ENABLE: ControlId = ControlId(libcamera::controls::ControlId::AeEnable as u32);
const EXPOSURE_TIME_MODE: u32 = libcamera::controls::ControlId::ExposureTimeMode as u32;
const ANALOGUE_GAIN_MODE: u32 = libcamera::controls::ControlId::AnalogueGainMode as u32;
/// The `ExposureTimeMode` / `AnalogueGainMode` value that takes the control from AE.
const MODE_MANUAL: i32 = 1;

/// Which of the exposure and gain controls a request takes from the application, not AE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ManualModes {
    pub exposure: bool,
    pub gain: bool,
}

impl ManualModes {
    /// The modes for a request: a held `ExposureTime` (or `AnalogueGain`) is manual, and so is
    /// everything when `AeEnable` is false. Nothing is written for an AE-owned control, so a
    /// request with AE on carries no mode at all.
    pub(super) fn new(exposure_held: bool, gain_held: bool, ae_enable_off: bool) -> Self {
        Self {
            exposure: exposure_held || ae_enable_off,
            gain: gain_held || ae_enable_off,
        }
    }

    /// The modes implied by the controls Styx holds for a request.
    pub(super) fn from_controls<'a>(
        controls: impl IntoIterator<Item = (ControlId, &'a ControlValue)>,
    ) -> Self {
        let (mut exposure, mut gain, mut ae_off) = (false, false, false);
        for (id, value) in controls {
            if id == EXPOSURE_TIME {
                exposure = true;
            } else if id == ANALOGUE_GAIN {
                gain = true;
            } else if id == AE_ENABLE && matches!(value, ControlValue::Bool(false)) {
                ae_off = true;
            }
        }
        Self::new(exposure, gain, ae_off)
    }

    /// Write the manual modes into `list`, allocating nothing on the Rust heap.
    pub(super) fn write(self, list: &mut LcControlList) {
        if self.exposure {
            lcraw::set(list, EXPOSURE_TIME_MODE, RawValue::Int32(&[MODE_MANUAL]));
        }
        if self.gain {
            lcraw::set(list, ANALOGUE_GAIN_MODE, RawValue::Int32(&[MODE_MANUAL]));
        }
    }
}

/// Build the control list passed to `Camera::start`.
///
/// The list takes its id map from a request's controls (libcamera's global `controls::controls`).
/// A list made with `ControlList::new()` has no id map, and libcamera serializes such a list as
/// V4L2 controls: when the IPA runs isolated in `raspberrypi_ipa_proxy`, its
/// `IPADataSerializer<ControlList>::deserialize` then hits `LOG(Fatal)` ("A list of V4L2 controls
/// requires a ControlInfoMap") and the proxy aborts.
pub(super) fn build_libcamera_controls(
    controls: &[(ControlId, ControlValue)],
    template: Option<&libcamera::request::Request>,
) -> Result<libcamera::utils::UniquePtr<LcControlList>, CaptureError> {
    let mut list = template
        .and_then(|req| req.controls().id_map())
        .and_then(LcControlList::from_id_map)
        .ok_or_else(|| CaptureError::Backend("libcamera control id map unavailable".into()))?;
    for (id, value) in controls {
        let v = to_lc_value(value)?;
        list.set_raw(id.0, v)
            .map_err(|e| classify_libcamera_control_apply_message(e.to_string()))?;
    }
    Ok(list)
}

/// Queue `req` with the controls Styx keeps applied (and the frame duration), written into its
/// control list in place: nothing is allocated per request.
/// The controls `Camera::start` gets (`None`: none) and the frame duration (µs) every request
/// carries: the requested controls, and the frame duration limits pinned to `interval`.
#[allow(clippy::type_complexity)]
pub(super) fn start_controls(
    controls: &[(ControlId, ControlValue)],
    template: Option<&libcamera::request::Request>,
    interval: Option<crate::prelude::Interval>,
) -> Result<
    (
        Option<libcamera::utils::UniquePtr<LcControlList>>,
        Option<i64>,
    ),
    CaptureError,
> {
    let mut list = build_libcamera_controls(controls, template)?;
    ManualModes::from_controls(controls.iter().map(|(id, value)| (*id, value))).write(&mut list);
    let frame_duration = interval.map(|interval| {
        let num = interval.numerator.get() as u64;
        let den = interval.denominator.get() as u64;
        let duration_us = num.saturating_mul(1_000_000).saturating_div(den.max(1));
        duration_us.clamp(1, i64::MAX as u64) as i64
    });
    if let Some(duration) = frame_duration {
        lcraw::set(
            &mut list,
            LIBCAMERA_FRAME_DURATION_LIMITS.0,
            RawValue::Int64(&[duration, duration]),
        );
    }
    Ok(((!list.is_empty()).then_some(list), frame_duration))
}

pub(super) fn queue_with_controls(
    cam: &libcamera::camera::ActiveCamera<'_>,
    mut req: libcamera::request::Request,
    controls: &HashMap<ControlId, ControlValue>,
    frame_duration: Option<i64>,
) -> Result<(), libcamera::request::Request> {
    {
        let list = req.controls_mut();
        for (id, val) in controls {
            lcraw::set_styx(list, id.0, val);
        }
        // Every request carries the modes: a manual exposure or gain holds only while its
        // mode is Manual, so the modes are not sent once.
        ManualModes::from_controls(controls.iter().map(|(id, value)| (*id, value))).write(list);
        if let Some(duration) = frame_duration {
            lcraw::set(
                list,
                LIBCAMERA_FRAME_DURATION_LIMITS.0,
                RawValue::Int64(&[duration, duration]),
            );
        }
    }
    cam.queue_request(req).map_err(|(req, _)| req)
}

#[derive(Debug, Default)]
pub struct PendingControlState {
    pub(super) updates: HashMap<ControlId, Option<ControlValue>>,
}

impl PendingControlState {
    pub(super) fn get(&self, id: &ControlId) -> Option<Option<ControlValue>> {
        self.updates.get(id).cloned()
    }
}

impl std::ops::Deref for PendingControlState {
    type Target = HashMap<ControlId, Option<ControlValue>>;

    fn deref(&self) -> &Self::Target {
        &self.updates
    }
}

impl std::ops::DerefMut for PendingControlState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.updates
    }
}

pub enum ControlMessage {
    Wake,
    Get(
        ControlId,
        std::sync::mpsc::Sender<Result<ControlValue, CaptureError>>,
    ),
    /// Stop streaming but keep the camera configured, with its buffers; acknowledged once no
    /// more frames will be delivered.
    Pause(std::sync::mpsc::Sender<()>),
    /// Stream again after [`ControlMessage::Pause`].
    Resume,
}

/// Apply control updates set through the control plane: writable controls go into
/// `control_state` (sent with every request), frame duration limits into `frame_duration`.
pub(super) fn apply_control_updates(
    updates: impl IntoIterator<Item = (ControlId, Option<ControlValue>)>,
    writable: &std::collections::HashSet<ControlId>,
    control_state: &mut std::collections::HashMap<ControlId, ControlValue>,
    frame_duration: &mut Option<i64>,
) {
    for (id, val) in updates {
        if !writable.contains(&id) {
            continue;
        }
        let is_duration = id == crate::capture_api::LIBCAMERA_FRAME_DURATION_LIMITS;
        match val {
            Some(ControlValue::Int(v)) if is_duration => *frame_duration = Some(v as i64),
            Some(_) if is_duration => {}
            Some(val) => {
                // AE back on hands exposure and gain to AE: the values held for them go too.
                if id == AE_ENABLE && matches!(val, ControlValue::Bool(true)) {
                    control_state.remove(&EXPOSURE_TIME);
                    control_state.remove(&ANALOGUE_GAIN);
                }
                control_state.insert(id, val);
            }
            None if is_duration => *frame_duration = None,
            None => {
                control_state.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libcamera::control::ControlList;

    fn int(v: i32) -> ControlValue {
        ControlValue::Int(v)
    }

    /// The modes a request carries, read back from the list it was written into.
    fn written_modes(controls: &[(ControlId, ControlValue)]) -> (Option<i32>, Option<i32>) {
        let mut list = ControlList::new();
        for (id, value) in controls {
            lcraw::set_styx(&mut list, id.0, value);
        }
        ManualModes::from_controls(controls.iter().map(|(id, value)| (*id, value)))
            .write(&mut list);
        let mut readback = HashMap::new();
        lcraw::record_metadata(lcraw::entries(&list), &mut readback);
        let get = |id: u32| match readback.get(&ControlId(id)) {
            Some(ControlValue::Int(v)) => Some(*v),
            _ => None,
        };
        (get(EXPOSURE_TIME_MODE), get(ANALOGUE_GAIN_MODE))
    }

    /// A manual exposure sets its mode to Manual in the request; a manual gain its own; with
    /// neither, the request has no mode at all (AE owns both).
    #[test]
    fn manual_values_put_their_mode_in_the_request() {
        assert_eq!(written_modes(&[]), (None, None));
        assert_eq!(
            written_modes(&[(EXPOSURE_TIME, int(8_000))]),
            (Some(MODE_MANUAL), None)
        );
        assert_eq!(
            written_modes(&[(ANALOGUE_GAIN, ControlValue::Float(7.75))]),
            (None, Some(MODE_MANUAL))
        );
        assert_eq!(
            written_modes(&[
                (EXPOSURE_TIME, int(8_000)),
                (ANALOGUE_GAIN, ControlValue::Float(7.75)),
            ]),
            (Some(MODE_MANUAL), Some(MODE_MANUAL))
        );
    }

    /// `AeEnable` false takes both controls from AE; true leaves them to it.
    #[test]
    fn ae_enable_off_sets_both_modes_and_on_sets_none() {
        assert_eq!(
            written_modes(&[(AE_ENABLE, ControlValue::Bool(false))]),
            (Some(MODE_MANUAL), Some(MODE_MANUAL))
        );
        assert_eq!(
            written_modes(&[(AE_ENABLE, ControlValue::Bool(true))]),
            (None, None)
        );
    }

    /// `set_ae(true)` drops the held exposure and gain, so the requests hand them back to AE;
    /// a value set after it holds again.
    #[test]
    fn ae_on_releases_held_exposure_and_gain() {
        let writable: std::collections::HashSet<ControlId> =
            [EXPOSURE_TIME, ANALOGUE_GAIN, AE_ENABLE]
                .into_iter()
                .collect();
        let mut state = HashMap::new();
        let mut duration = None;
        apply_control_updates(
            [
                (EXPOSURE_TIME, Some(int(8_000))),
                (ANALOGUE_GAIN, Some(ControlValue::Float(7.75))),
            ],
            &writable,
            &mut state,
            &mut duration,
        );
        assert_eq!(state.len(), 2);
        apply_control_updates(
            [(AE_ENABLE, Some(ControlValue::Bool(true)))],
            &writable,
            &mut state,
            &mut duration,
        );
        assert_eq!(state.get(&AE_ENABLE), Some(&ControlValue::Bool(true)));
        assert!(!state.contains_key(&EXPOSURE_TIME));
        assert!(!state.contains_key(&ANALOGUE_GAIN));
        let held = ManualModes::from_controls(state.iter().map(|(id, v)| (*id, v)));
        assert_eq!(held, ManualModes::default());

        apply_control_updates(
            [(EXPOSURE_TIME, Some(int(5_000)))],
            &writable,
            &mut state,
            &mut duration,
        );
        let held = ManualModes::from_controls(state.iter().map(|(id, v)| (*id, v)));
        assert_eq!(
            held,
            ManualModes {
                exposure: true,
                gain: false
            }
        );
    }
}
