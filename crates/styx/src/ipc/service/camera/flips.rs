//! A native camera's flips as service controls (`hflip`, `vflip`, `rotation` 0/180): the
//! sensor flips only while stopped (a colour sensor's Bayer order changes with them), so the
//! service sets them by restarting the capture for every client, as it does a frame rate.

use parking_lot::MutexGuard;
use std::sync::atomic::Ordering;

use styx_core::prelude::*;

use super::{Camera, State, restart};
use crate::capture_api::standard_controls::number;
use crate::ipc::controls::{
    AppliedControl, ControlCaller, ControlDescriptor, ControlEvent, ControlPolicy, ControlRefusal,
    SERVICE_HFLIP, SERVICE_ROTATION, SERVICE_VFLIP,
};
use crate::ipc::service::{Counters, ServiceConfig};
use crate::prelude::StyxConfig;

/// Whether `id` is one of the flip controls.
pub(super) fn is_flip(id: ControlId) -> bool {
    [SERVICE_HFLIP, SERVICE_VFLIP, SERVICE_ROTATION].contains(&id)
}

/// The control's value for these flips.
fn value_of(id: ControlId, (h, v): (bool, bool)) -> ControlValue {
    match id {
        SERVICE_HFLIP => ControlValue::Bool(h),
        SERVICE_VFLIP => ControlValue::Bool(v),
        _ => ControlValue::Int(if h && v { 180 } else { 0 }),
    }
}

fn name_of(id: ControlId) -> &'static str {
    match id {
        SERVICE_HFLIP => "hflip",
        SERVICE_VFLIP => "vflip",
        _ => "rotation",
    }
}

/// The flips `value` for control `id` asks for, from `now`.
fn wanted(
    id: ControlId,
    value: &ControlValue,
    now: (bool, bool),
) -> Result<(bool, bool), ControlRefusal> {
    let n = number(value).ok_or_else(|| ControlRefusal::Invalid(format!("{value:?}")))?;
    Ok(match id {
        SERVICE_HFLIP => (n != 0.0, now.1),
        SERVICE_VFLIP => (now.0, n != 0.0),
        _ => {
            let turned = n.rem_euclid(360.0);
            if turned != 0.0 && turned != 180.0 {
                return Err(ControlRefusal::Invalid(format!(
                    "rotation {n}: 0 or 180 degrees (the sensor cannot turn by 90)"
                )));
            }
            (turned == 180.0, turned == 180.0)
        }
    })
}

impl Camera {
    /// The flips the camera's sensor has (the `flips` property of a native camera).
    fn flips_offered(&self) -> (bool, bool) {
        let flips = self
            .device
            .backends
            .first()
            .and_then(|b| b.properties.iter().find(|(k, _)| k == "flips"))
            .map_or("", |(_, v)| v.as_str());
        (flips.contains('h'), flips.contains('v'))
    }

    /// The camera with its flip controls starting from the service's capture settings.
    pub(in crate::ipc::service) fn with_flips(self, capture: Option<&StyxConfig>) -> Self {
        if let Some(n) = capture.map(|c| &c.backends.native)
            && (n.hflip || n.vflip)
        {
            self.set_flips(Some((n.hflip, n.vflip)));
        }
        self
    }

    /// The flips in effect (or that the next capture starts with).
    fn flips_now(&self) -> (bool, bool) {
        self.flips().unwrap_or((false, false))
    }

    /// Set a flip control: restarts the capture for every client with the sensor flipped.
    pub(super) fn set_flip(
        &self,
        state: &mut MutexGuard<'_, State>,
        id: ControlId,
        value: ControlValue,
        caller: &ControlCaller,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<AppliedControl, ControlRefusal> {
        let (has_h, has_v) = self.flips_offered();
        let has = match id {
            SERVICE_HFLIP => has_h,
            SERVICE_VFLIP => has_v,
            _ => has_h && has_v,
        };
        if !has {
            return Err(ControlRefusal::Unsupported(format!(
                "this camera has no {} control",
                name_of(id)
            )));
        }
        if let Some(client) = caller.client
            && state.clients.iter().any(|c| c.id == client && c.low)
        {
            return Err(ControlRefusal::NotPermitted(
                "a low-priority client does not restart the capture to flip it".into(),
            ));
        }
        if !config.controls.restarts() {
            return Err(ControlRefusal::NotPermitted(
                "the sensor flips only while stopped, and the service does not restart the \
                 capture for every client (ControlPolicy::no_restart)"
                    .into(),
            ));
        }
        let now = self.flips_now();
        let flips = wanted(id, &value, now)?;
        let previous = self.flips();
        self.set_flips(Some(flips));
        let mut applied = AppliedControl {
            id,
            requested: value,
            value: value_of(id, flips),
            clamped: false,
            frame: None,
            deferred: state.clients.is_empty(),
            restarted: false,
        };
        if flips != now && !state.clients.is_empty() {
            if let Err(reason) = self.restart_clients(state, config) {
                self.set_flips(previous);
                let _ = self.restart_clients(state, config);
                return Err(ControlRefusal::Failed(format!(
                    "{} for every client: {reason}",
                    name_of(id)
                )));
            }
            counters.restarts.fetch_add(1, Ordering::Relaxed);
            applied.restarted = true;
        }
        for changed in [SERVICE_HFLIP, SERVICE_VFLIP, SERVICE_ROTATION] {
            if value_of(changed, flips) != value_of(changed, now) || changed == id {
                let event = ControlEvent {
                    id: changed,
                    standard: None,
                    value: value_of(changed, flips),
                    frame: None,
                    by: None,
                };
                self.broadcast(state, &event);
            }
        }
        Ok(applied)
    }

    /// Plan every client again and restart the capture for them.
    fn restart_clients(
        &self,
        state: &mut MutexGuard<'_, State>,
        config: &ServiceConfig,
    ) -> Result<(), String> {
        let requests: Vec<_> = state
            .clients
            .iter()
            .map(|c| (c.request.clone(), c.low))
            .collect();
        let planned = self
            .plan_clients(&requests, state.fps_override, None, config)
            .map_err(|err| super::describe(&err))?;
        restart(state, planned, config).map(drop)
    }

    /// A flip control's value now.
    pub(super) fn flip_value(&self, id: ControlId) -> ControlValue {
        value_of(id, self.flips_now())
    }

    /// The flip controls of a camera whose sensor has flips, when the service restarts
    /// captures.
    pub(super) fn flip_descriptors(
        &self,
        caller: &ControlCaller,
        policy: &ControlPolicy,
    ) -> Vec<ControlDescriptor> {
        let (has_h, has_v) = self.flips_offered();
        if !policy.restarts() {
            return Vec::new();
        }
        let flips = self.flips_now();
        [
            (SERVICE_HFLIP, has_h),
            (SERVICE_VFLIP, has_v),
            (SERVICE_ROTATION, has_h && has_v),
        ]
        .into_iter()
        .filter(|(_, has)| *has)
        .map(|(id, _)| {
            let rotation = id == SERVICE_ROTATION;
            let (kind, min, max, default, step) = if rotation {
                (
                    ControlKind::Int,
                    ControlValue::Int(0),
                    ControlValue::Int(180),
                    ControlValue::Int(0),
                    Some(ControlValue::Int(180)),
                )
            } else {
                (
                    ControlKind::Bool,
                    ControlValue::Bool(false),
                    ControlValue::Bool(true),
                    ControlValue::Bool(false),
                    None,
                )
            };
            ControlDescriptor {
                meta: ControlMeta {
                    id,
                    name: name_of(id).into(),
                    kind,
                    access: Access::ReadWrite,
                    min,
                    max,
                    default,
                    step,
                    menu: None,
                    metadata: ControlMetadata::default(),
                },
                current: Some(value_of(id, flips)),
                standard: None,
                writable: policy.check(caller, id, None).is_ok(),
                effective_min: None,
                effective_max: None,
            }
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flip_controls_ask_for_flips() {
        let b = ControlValue::Bool;
        assert_eq!(
            wanted(SERVICE_HFLIP, &b(true), (false, true)).unwrap(),
            (true, true)
        );
        assert_eq!(
            wanted(SERVICE_VFLIP, &b(false), (true, true)).unwrap(),
            (true, false)
        );
        let r = |deg| wanted(SERVICE_ROTATION, &ControlValue::Int(deg), (true, false));
        assert_eq!(r(180).unwrap(), (true, true));
        assert_eq!(r(-180).unwrap(), (true, true));
        assert_eq!(r(0).unwrap(), (false, false));
        assert!(r(90).is_err());
        assert_eq!(
            value_of(SERVICE_ROTATION, (true, true)),
            ControlValue::Int(180)
        );
        assert_eq!(
            value_of(SERVICE_ROTATION, (true, false)),
            ControlValue::Int(0)
        );
    }
}
