//! Backend-agnostic control helpers.

#[cfg(feature = "v4l2")]
use crate::capture_api::ControlApplyKind;
#[cfg(feature = "v4l2")]
use crate::prelude::CaptureError;
#[cfg(feature = "v4l2")]
use styx_capture::prelude::*;
#[cfg(feature = "v4l2")]
use styx_kernel::v4l2::{self as kv4l2, Controls as _, VideoDevice};

#[cfg(feature = "v4l2")]
pub(crate) fn apply_v4l2_controls(
    path: &str,
    controls: &[(ControlId, ControlValue)],
) -> Result<(), CaptureError> {
    let dev = VideoDevice::open(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    for (id, value) in controls {
        let v = to_v4l_value(value)?;
        dev.set_control(id.0, v).map_err(|e| {
            let inactive = || v4l2_inactive(&dev, *id);
            set_error(e.is_access_denied(), inactive, e.to_string())
        })?;
    }
    Ok(())
}

/// What a refused control write is: `EACCES` on a control the device reports inactive is that
/// state, any other refusal (`EACCES` also means read only) a failure.
#[cfg(feature = "v4l2")]
fn set_error(denied: bool, inactive: impl FnOnce() -> bool, message: String) -> CaptureError {
    if denied && inactive() {
        return CaptureError::classified_control_apply(ControlApplyKind::Inactive, message);
    }
    CaptureError::control_apply(message)
}

#[cfg(feature = "v4l2")]
fn v4l2_inactive(dev: &VideoDevice, id: ControlId) -> bool {
    dev.query_control(id.0)
        .is_ok_and(|info| info.flags.contains(kv4l2::ControlFlags::INACTIVE))
}

/// Whether the control takes no value now because another control holds it.
#[cfg(feature = "v4l2")]
pub(crate) fn read_v4l2_inactive(path: &str, id: ControlId) -> bool {
    VideoDevice::open(path).is_ok_and(|dev| v4l2_inactive(&dev, id))
}

/// The value to write for a Styx control value.
///
/// Values go through the 64-bit member of the control union, sign-extended: the kernel reads
/// its low 32 bits for 32-bit controls (integer, boolean, menu) and all of it for 64-bit ones.
#[cfg(feature = "v4l2")]
pub(crate) fn to_v4l_value(value: &ControlValue) -> Result<kv4l2::ControlValue, CaptureError> {
    let v: i64 = match value {
        ControlValue::None => 0,
        ControlValue::Bool(v) => i64::from(*v),
        ControlValue::Int(v) => i64::from(*v),
        ControlValue::Uint(v) => i64::from(*v),
        ControlValue::Float(v) => v.round() as i64,
        ControlValue::Rect(_) | ControlValue::Rects(_) => {
            return Err(CaptureError::control_apply(
                "rectangle controls are not supported by V4L2 controls",
            ));
        }
    };
    Ok(kv4l2::ControlValue::Integer64(v))
}

#[cfg(feature = "v4l2")]
pub(crate) fn read_v4l2_control(path: &str, id: ControlId) -> Result<ControlValue, CaptureError> {
    let dev = VideoDevice::open(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let apply_err = |e: styx_kernel::Error| CaptureError::control_apply(e.to_string());
    let info = dev.query_control(id.0).map_err(apply_err)?;
    let value = || {
        dev.get_controls_for(kv4l2::ControlWhich::Current, std::slice::from_ref(&info))
            .map(|mut values| values.remove(0).as_i64().unwrap_or_default())
            .map_err(apply_err)
    };
    match info.control_type {
        kv4l2::ControlType::Integer | kv4l2::ControlType::Menu | kv4l2::ControlType::Integer64 => {
            Ok(ControlValue::Int(value()? as i32))
        }
        kv4l2::ControlType::Boolean => Ok(ControlValue::Bool(value()? == 1)),
        _ => Err(CaptureError::control_apply("cannot handle control type")),
    }
}

#[cfg(all(test, feature = "v4l2"))]
mod tests {
    #[test]
    fn only_eacces_on_an_inactive_control_is_that_state() {
        use super::*;
        let kind = |denied, inactive: bool| match set_error(denied, || inactive, "no".into()) {
            CaptureError::ControlApply { kind, .. } => kind,
            other => panic!("{other}"),
        };
        assert_eq!(kind(true, true), ControlApplyKind::Inactive);
        assert_eq!(kind(true, false), ControlApplyKind::Other);
        assert_eq!(kind(false, true), ControlApplyKind::Other);
    }

    use super::*;

    #[test]
    fn values_sign_extend_into_the_64_bit_member() {
        let v = |c| to_v4l_value(&c).unwrap();
        assert_eq!(v(ControlValue::Int(-3)), kv4l2::ControlValue::Integer64(-3));
        assert_eq!(
            v(ControlValue::Bool(true)),
            kv4l2::ControlValue::Integer64(1)
        );
        assert_eq!(v(ControlValue::None), kv4l2::ControlValue::Integer64(0));
        assert_eq!(
            v(ControlValue::Uint(u32::MAX)),
            kv4l2::ControlValue::Integer64(i64::from(u32::MAX))
        );
        assert_eq!(
            v(ControlValue::Float(2.6)),
            kv4l2::ControlValue::Integer64(3)
        );
    }
}
