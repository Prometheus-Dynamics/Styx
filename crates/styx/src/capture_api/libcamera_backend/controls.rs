use std::collections::HashMap;

use libcamera::control::ControlList as LcControlList;
use styx_core::controls::{ControlId, ControlValue};

use crate::capture_api::CaptureError;

use super::util::{classify_libcamera_control_apply_message, to_lc_value};

pub(super) fn build_libcamera_controls(
    controls: &[(ControlId, ControlValue)],
) -> Result<libcamera::utils::UniquePtr<LcControlList>, CaptureError> {
    let mut list = LcControlList::new();
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
                30,
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
}
