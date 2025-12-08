//! Backend-agnostic control helpers.

#[cfg(feature = "v4l2")]
use crate::prelude::CaptureError;
#[cfg(feature = "v4l2")]
use styx_capture::prelude::*;

#[cfg(feature = "v4l2")]
use v4l::control::Control;

#[cfg(feature = "v4l2")]
pub(crate) fn apply_v4l2_controls(
    path: &str,
    controls: &[(ControlId, ControlValue)],
) -> Result<(), CaptureError> {
    let dev = v4l::Device::with_path(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    for (id, value) in controls {
        let v = to_v4l_value(value)?;
        let ctrl = Control { id: id.0, value: v };
        dev.set_control(ctrl)
            .map_err(|e| CaptureError::ControlApply(e.to_string()))?;
    }
    Ok(())
}

#[cfg(feature = "v4l2")]
pub(crate) fn to_v4l_value(value: &ControlValue) -> Result<v4l::control::Value, CaptureError> {
    use v4l::control::Value;
    let val = match value {
        ControlValue::None => Value::None,
        ControlValue::Bool(v) => Value::Boolean(*v),
        ControlValue::Int(v) => Value::Integer(*v as i64),
        ControlValue::Uint(v) => Value::Integer(*v as i64),
        ControlValue::Float(v) => Value::Integer(v.round() as i64),
    };
    Ok(val)
}

#[cfg(feature = "v4l2")]
pub(crate) fn read_v4l2_control(path: &str, id: ControlId) -> Result<ControlValue, CaptureError> {
    let dev = v4l::Device::with_path(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let ctrl = dev
        .control(id.0)
        .map_err(|e| CaptureError::ControlApply(e.to_string()))?;
    match ctrl.value {
        v4l::control::Value::Integer(v) => Ok(ControlValue::Int(v as i32)),
        v4l::control::Value::Boolean(v) => Ok(ControlValue::Bool(v)),
        v4l::control::Value::None => Ok(ControlValue::None),
        _ => Err(CaptureError::ControlUnsupported),
    }
}
