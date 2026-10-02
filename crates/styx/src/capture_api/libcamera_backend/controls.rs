use std::collections::HashMap;

use libcamera::control::ControlList as LcControlList;
use styx_core::controls::{ControlId, ControlValue};

use crate::capture_api::CaptureError;
use crate::capture_api::LIBCAMERA_FRAME_DURATION_LIMITS;

use super::util::{classify_libcamera_control_apply_message, from_lc_value, to_lc_value};

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

pub(super) fn queue_with_controls(
    cam: &libcamera::camera::ActiveCamera<'_>,
    mut req: libcamera::request::Request,
    controls: &HashMap<ControlId, ControlValue>,
    frame_duration: Option<i64>,
) -> Result<(), libcamera::request::Request> {
    {
        let list = req.controls_mut();
        for (id, val) in controls {
            if let Ok(lc_val) = to_lc_value(val) {
                let _ = list.set_raw(id.0, lc_val);
            }
        }
        if let Some(duration) = frame_duration {
            let _ = list.set_raw(
                LIBCAMERA_FRAME_DURATION_LIMITS.0,
                libcamera::control_value::ControlValue::from([duration, duration]),
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

/// Timing from a completed request's metadata.
pub(super) struct RequestTiming {
    /// `SensorTimestamp`: start of exposure, `CLOCK_BOOTTIME`. Buffer timestamps on Raspberry Pi
    /// mark ISP completion instead.
    pub sensor_timestamp: Option<u64>,
    /// `FrameDuration` in nanoseconds.
    pub frame_duration_ns: Option<u64>,
}

/// Record a completed request's metadata as control readback and pick out its timing.
pub(super) fn read_request_metadata(
    req: &libcamera::request::Request,
    readback: &mut HashMap<ControlId, ControlValue>,
) -> RequestTiming {
    let mut timing = RequestTiming {
        sensor_timestamp: None,
        frame_duration_ns: None,
    };
    for (id, val) in req.metadata() {
        if let libcamera::control_value::ControlValue::Int64(v) = &val {
            let first = v.first().and_then(|v| u64::try_from(*v).ok());
            if id == libcamera::controls::ControlId::SensorTimestamp as u32 {
                timing.sensor_timestamp = first;
            } else if id == libcamera::controls::ControlId::FrameDuration as u32 {
                timing.frame_duration_ns = first.map(|us| us * 1_000);
            }
        }
        if let Some(val) = from_lc_value(&val) {
            readback.insert(ControlId(id), val);
        }
    }
    timing
}
