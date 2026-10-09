use std::collections::HashMap;

use libcamera::control::ControlList as LcControlList;
use styx_core::controls::{ControlId, ControlValue};

use crate::capture_api::CaptureError;
use crate::capture_api::LIBCAMERA_FRAME_DURATION_LIMITS;

use super::lcraw::{self, RawValue};
use super::util::{classify_libcamera_control_apply_message, to_lc_value};

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
                control_state.insert(id, val);
            }
            None if is_duration => *frame_duration = None,
            None => {
                control_state.remove(&id);
            }
        }
    }
}
