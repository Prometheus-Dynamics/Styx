//! A camera's controls for its clients: finding the backend's control for a request, checking
//! and clamping the value, applying it to the shared capture (or keeping it for when the
//! capture starts), restarting the capture for a frame rate the camera cannot change while
//! streaming, and telling subscribed clients.

use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;

use styx_core::prelude::*;

use super::{Camera, State, restart};
use crate::BackendKind;
use crate::capture_api::{CaptureError, CaptureHandle};
use crate::ipc::controls::{
    AppliedControl, ControlCaller, ControlDescriptor, ControlEvent, ControlPolicy, ControlRefusal,
    ControlTarget, SERVICE_FRAME_RATE, StandardControl,
};
use crate::ipc::service::{Counters, ServiceConfig};
use crate::ipc::socket;

/// How a standard control's units become the backend's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scale {
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

/// The backend's control for a request.
struct Resolved {
    id: ControlId,
    meta: Option<ControlMeta>,
    standard: Option<StandardControl>,
    scale: Scale,
}

/// V4L2 (and UVC) control ids of the standard controls, with their units.
fn v4l2_cid(control: StandardControl) -> Option<(u32, Scale)> {
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

/// The backend's control for `control`, if the camera has one.
fn find_standard(
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

/// The standard control a backend control answers, if any.
fn standard_of(
    metas: &[ControlMeta],
    backend: BackendKind,
    id: ControlId,
) -> Option<StandardControl> {
    StandardControl::ALL
        .into_iter()
        .find(|s| find_standard(metas, backend, *s).is_some_and(|(m, _)| m.id == id))
}

fn resolve(
    metas: &[ControlMeta],
    backend: BackendKind,
    target: ControlTarget,
) -> Result<Resolved, ControlRefusal> {
    match target {
        ControlTarget::Id(id) => {
            // A backend id takes the backend's own units.
            Ok(Resolved {
                id,
                meta: metas.iter().find(|m| m.id == id).cloned(),
                standard: standard_of(metas, backend, id),
                scale: Scale::Same,
            })
        }
        ControlTarget::Standard(s) => match find_standard(metas, backend, s) {
            Some((meta, scale)) => Ok(Resolved {
                id: meta.id,
                meta: Some(meta),
                standard: Some(s),
                scale,
            }),
            None if s == StandardControl::FrameRate => Ok(Resolved {
                id: SERVICE_FRAME_RATE,
                meta: None,
                standard: Some(s),
                scale: Scale::Same,
            }),
            None => Err(ControlRefusal::Unsupported(format!(
                "this camera has no {} control",
                s.name()
            ))),
        },
    }
}

fn number(v: &ControlValue) -> Option<f64> {
    match v {
        ControlValue::Bool(b) => Some(f64::from(u8::from(*b))),
        ControlValue::Int(i) => Some(f64::from(*i)),
        ControlValue::Uint(u) => Some(f64::from(*u)),
        ControlValue::Float(f) => Some(f64::from(*f)),
        _ => None,
    }
}

/// A standard control's value in the backend's units.
fn to_backend(scale: Scale, v: &ControlValue) -> ControlValue {
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

/// A backend value in the standard control's units.
fn from_backend(scale: Scale, v: ControlValue) -> ControlValue {
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

/// `value` as `meta`'s type, within its range and on its steps; whether it was clamped.
fn fit(meta: &ControlMeta, value: ControlValue) -> Result<(ControlValue, bool), ControlRefusal> {
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

fn refusal(err: CaptureError) -> ControlRefusal {
    match err {
        CaptureError::ControlUnsupported => {
            ControlRefusal::Unsupported("the camera does not support it".into())
        }
        err => ControlRefusal::Failed(err.to_string()),
    }
}

/// Whether the backend reads back what was just set (others apply it with a later frame).
fn reads_back(backend: BackendKind) -> bool {
    matches!(
        backend,
        BackendKind::V4l2 | BackendKind::Uvc | BackendKind::Virtual
    )
}

impl State {
    fn capture(&self) -> Option<&CaptureHandle> {
        self.running.as_ref().map(|r| r.session.capture())
    }

    fn remember(&mut self, id: ControlId, value: ControlValue) {
        match self.controls.iter_mut().find(|(i, _)| *i == id) {
            Some(slot) => slot.1 = value,
            None => self.controls.push((id, value)),
        }
    }

    fn remembered(&self, id: ControlId) -> Option<ControlValue> {
        self.controls
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, v)| v.clone())
    }
}

impl Camera {
    /// The controls the camera lists (the running capture's, else its backend's), and the
    /// backend.
    fn metas(&self, state: &State) -> (Vec<ControlMeta>, BackendKind) {
        match state.capture() {
            Some(capture) => (capture.control_metas().to_vec(), capture.backend),
            None => self.device.backends.first().map_or_else(
                || (Vec::new(), BackendKind::Virtual),
                |b| (b.descriptor.controls.clone(), b.kind),
            ),
        }
    }

    /// The client with this token, and whether it is the owner (connected longest).
    pub(in crate::ipc::service) fn client_of(&self, token: u64) -> Option<(u64, bool)> {
        let state = self.state.lock();
        let i = state.clients.iter().position(|c| c.token == token)?;
        Some((state.clients[i].id, i == 0))
    }

    /// Change a control for `caller`, as `policy` allows.
    pub(in crate::ipc::service) fn set_control(
        &self,
        target: ControlTarget,
        value: ControlValue,
        caller: &ControlCaller,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<AppliedControl, ControlRefusal> {
        let mut state = self.state.lock();
        let (metas, backend) = self.metas(&state);
        let r = resolve(&metas, backend, target)?;
        if let Some(meta) = &r.meta
            && meta.access == Access::ReadOnly
        {
            return Err(ControlRefusal::ReadOnly(meta.name.clone()));
        }
        config.controls.check(caller, r.id, r.standard)?;
        if r.id == SERVICE_FRAME_RATE {
            return self.restart_at(&mut state, value, &config.controls, config, counters);
        }
        let backend_value = to_backend(r.scale, &value);
        let (fitted, clamped) = match &r.meta {
            Some(meta) => fit(meta, backend_value)?,
            None => (backend_value, false),
        };
        let mut applied = AppliedControl {
            id: r.id,
            requested: value.clone(),
            value: from_backend(r.scale, fitted.clone()),
            clamped,
            frame: None,
            deferred: false,
            restarted: false,
        };
        match state.capture() {
            Some(capture) => match capture.set_control_landing(r.id, fitted.clone()) {
                Ok(frame) => {
                    applied.frame = frame;
                    if reads_back(backend)
                        && let Ok(now) = capture.get_control(r.id)
                    {
                        applied.value = from_backend(r.scale, now);
                    }
                }
                // A frame rate the camera cannot change while streaming: restart at it.
                Err(_) if r.standard == Some(StandardControl::FrameRate) => {
                    return self.restart_at(&mut state, value, &config.controls, config, counters);
                }
                Err(err) => return Err(refusal(err)),
            },
            None => applied.deferred = true,
        }
        // A trigger is an action, not a setting to apply again after a restart.
        if r.standard != Some(StandardControl::AfTrigger) {
            state.remember(r.id, fitted);
        }
        self.broadcast(
            &mut state,
            &ControlEvent {
                id: r.id,
                standard: r.standard,
                value: applied.value.clone(),
                frame: applied.frame,
                by: caller.client,
            },
        );
        Ok(applied)
    }

    /// Plan every client's frames at `value` frames per second and restart the capture.
    fn restart_at(
        &self,
        state: &mut State,
        value: ControlValue,
        policy: &ControlPolicy,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<AppliedControl, ControlRefusal> {
        let fps = number(&value)
            .filter(|f| f.is_finite() && *f >= 0.5)
            .ok_or_else(|| ControlRefusal::Invalid(format!("frame rate {value:?}")))?;
        let rounded = (fps.round() as u32).clamp(1, 1000);
        if !policy.restarts() {
            return Err(ControlRefusal::NotPermitted(
                "the camera cannot change its frame rate while streaming, and the service does \
                 not restart the capture for every client (ControlPolicy::no_restart)"
                    .into(),
            ));
        }
        let previous = state.fps_override.replace(rounded);
        let mut applied = AppliedControl {
            id: SERVICE_FRAME_RATE,
            requested: value,
            value: ControlValue::Float(rounded as f32),
            clamped: f64::from(rounded) != fps,
            frame: None,
            deferred: state.clients.is_empty(),
            restarted: false,
        };
        if !state.clients.is_empty() {
            let requests: Vec<_> = state.clients.iter().map(|c| c.request.clone()).collect();
            let started = self
                .plan_for(&requests, Some(rounded), config)
                .map_err(|err| super::describe(&err))
                .and_then(|plan| restart(state, plan, requests.len(), config));
            if let Err(reason) = started {
                state.fps_override = previous;
                if let Ok(plan) = self.plan_for(&requests, previous, config) {
                    let _ = restart(state, plan, requests.len(), config);
                }
                return Err(ControlRefusal::Failed(format!(
                    "{rounded} fps for every client: {reason}"
                )));
            }
            counters.restarts.fetch_add(1, Ordering::Relaxed);
            applied.restarted = true;
            if let Some(fps) = state
                .running
                .as_ref()
                .and_then(|r| r.plan.consumers.first())
                .and_then(|c| c.delivered().fps)
            {
                applied.value = ControlValue::Float(fps);
            }
        }
        let event = ControlEvent {
            id: SERVICE_FRAME_RATE,
            standard: Some(StandardControl::FrameRate),
            value: applied.value.clone(),
            frame: None,
            by: None,
        };
        self.broadcast(state, &event);
        Ok(applied)
    }

    /// A control's value now (the standard control's units for a standard control).
    pub(in crate::ipc::service) fn get_control(
        &self,
        target: ControlTarget,
    ) -> Result<(ControlId, ControlValue), ControlRefusal> {
        let state = self.state.lock();
        let (metas, backend) = self.metas(&state);
        let r = resolve(&metas, backend, target)?;
        if r.id == SERVICE_FRAME_RATE {
            return self
                .frame_rate(&state)
                .map(|fps| (r.id, ControlValue::Float(fps)))
                .ok_or_else(|| ControlRefusal::Unsupported("no frame rate set yet".into()));
        }
        let value = match state.capture() {
            Some(capture) => capture.get_control(r.id).map_err(refusal),
            None => state
                .remembered(r.id)
                .or_else(|| r.meta.as_ref().map(|m| m.default.clone()))
                .ok_or_else(|| ControlRefusal::Unsupported("the camera is not streaming".into())),
        }?;
        Ok((r.id, from_backend(r.scale, value)))
    }

    fn frame_rate(&self, state: &State) -> Option<f32> {
        state
            .running
            .as_ref()
            .and_then(|r| r.plan.consumers.first())
            .and_then(|c| c.delivered().fps)
            .or(state.fps_override.map(|f| f as f32))
    }

    /// Every control, its value now and whether `caller` may change it.
    pub(in crate::ipc::service) fn list_controls(
        &self,
        caller: &ControlCaller,
        policy: &ControlPolicy,
    ) -> Vec<ControlDescriptor> {
        let state = self.state.lock();
        let (metas, backend) = self.metas(&state);
        let mut out: Vec<ControlDescriptor> = metas
            .iter()
            .map(|meta| {
                let standard = standard_of(&metas, backend, meta.id);
                let current = match state.capture() {
                    Some(capture) => capture.get_control(meta.id).ok(),
                    None => state.remembered(meta.id),
                };
                ControlDescriptor {
                    meta: meta.clone(),
                    current,
                    standard,
                    writable: meta.access == Access::ReadWrite
                        && policy.check(caller, meta.id, standard).is_ok(),
                }
            })
            .collect();
        let has_rate = out
            .iter()
            .any(|d| d.standard == Some(StandardControl::FrameRate));
        if !has_rate && policy.restarts() {
            out.push(self.frame_rate_descriptor(&state, caller, policy));
        }
        out
    }

    /// The frame rate the service sets by restarting the capture.
    fn frame_rate_descriptor(
        &self,
        state: &State,
        caller: &ControlCaller,
        policy: &ControlPolicy,
    ) -> ControlDescriptor {
        let fastest = self
            .device
            .backends
            .iter()
            .flat_map(|b| &b.descriptor.modes)
            .flat_map(|m| {
                m.intervals
                    .iter()
                    .copied()
                    .chain(m.interval_stepwise.map(|s| s.min))
            })
            .map(|i| i.fps())
            .fold(1.0f32, f32::max);
        let standard = Some(StandardControl::FrameRate);
        ControlDescriptor {
            meta: ControlMeta {
                id: SERVICE_FRAME_RATE,
                name: StandardControl::FrameRate.name().into(),
                kind: ControlKind::Float,
                access: Access::ReadWrite,
                min: ControlValue::Float(1.0),
                max: ControlValue::Float(fastest.round()),
                default: ControlValue::Float(30f32.min(fastest)),
                step: None,
                menu: None,
                metadata: ControlMetadata::default(),
            },
            current: self.frame_rate(state).map(ControlValue::Float),
            standard,
            writable: policy.check(caller, SERVICE_FRAME_RATE, standard).is_ok(),
        }
    }

    /// Send control changes on `socket` (a duplicate of a control connection's) until
    /// [`Camera::unsubscribe`]; returns its key.
    pub(in crate::ipc::service) fn subscribe(&self, socket: OwnedFd) -> u64 {
        let mut state = self.state.lock();
        let key = state.next_subscriber;
        state.next_subscriber += 1;
        state.subscribers.push((key, socket));
        key
    }

    pub(in crate::ipc::service) fn unsubscribe(&self, key: u64) {
        self.state.lock().subscribers.retain(|(k, _)| *k != key);
    }

    /// Tell every subscriber (a full socket misses the event; a closed one is forgotten).
    fn broadcast(&self, state: &mut State, event: &ControlEvent) {
        if state.subscribers.is_empty() {
            return;
        }
        let message = crate::ipc::wire::encode_control_event(event);
        state
            .subscribers
            .retain(|(_, socket)| socket::send(socket, &message, &[]).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: ControlKind, min: ControlValue, max: ControlValue) -> ControlMeta {
        ControlMeta {
            id: ControlId(1),
            name: "c".into(),
            kind,
            access: Access::ReadWrite,
            default: min.clone(),
            min,
            max,
            step: None,
            menu: None,
            metadata: ControlMetadata::default(),
        }
    }

    #[test]
    fn values_are_fitted_to_their_control() {
        let m = meta(
            ControlKind::Uint,
            ControlValue::Uint(10),
            ControlValue::Uint(100),
        );
        assert_eq!(
            fit(&m, ControlValue::Float(55.4)).unwrap(),
            (ControlValue::Uint(55), true)
        );
        assert_eq!(
            fit(&m, ControlValue::Uint(50)).unwrap(),
            (ControlValue::Uint(50), false)
        );
        assert_eq!(
            fit(&m, ControlValue::Int(-5)).unwrap(),
            (ControlValue::Uint(10), true)
        );
        assert_eq!(
            fit(&m, ControlValue::Uint(500)).unwrap(),
            (ControlValue::Uint(100), true)
        );
        let mut stepped = meta(
            ControlKind::Int,
            ControlValue::Int(0),
            ControlValue::Int(100),
        );
        stepped.step = Some(ControlValue::Int(10));
        assert_eq!(
            fit(&stepped, ControlValue::Int(44)).unwrap(),
            (ControlValue::Int(40), true)
        );
        let f = meta(
            ControlKind::Float,
            ControlValue::Float(1.0),
            ControlValue::Float(16.0),
        );
        assert_eq!(
            fit(&f, ControlValue::Float(2.5)).unwrap(),
            (ControlValue::Float(2.5), false)
        );
        assert!(
            fit(
                &f,
                ControlValue::Rect(ControlRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                })
            )
            .is_err()
        );
        let mut menu = meta(
            ControlKind::Menu,
            ControlValue::Int(0),
            ControlValue::Int(1),
        );
        menu.menu = Some(vec!["a".into(), "b".into()]);
        assert!(fit(&menu, ControlValue::Int(1)).is_ok());
        assert!(matches!(
            fit(&menu, ControlValue::Int(5)),
            Err(ControlRefusal::Invalid(_))
        ));
    }

    #[test]
    fn standard_controls_convert_to_v4l2_units() {
        assert_eq!(
            to_backend(Scale::Per100Us, &ControlValue::Uint(10_000)),
            ControlValue::Int(100)
        );
        assert_eq!(
            from_backend(Scale::Per100Us, ControlValue::Int(100)),
            ControlValue::Uint(10_000)
        );
        assert_eq!(
            to_backend(Scale::AeMenu, &ControlValue::Bool(true)),
            ControlValue::Int(3)
        );
        assert_eq!(
            from_backend(Scale::AeMenu, ControlValue::Int(1)),
            ControlValue::Bool(false)
        );
        assert_eq!(
            to_backend(Scale::Milli, &ControlValue::Float(-0.5)),
            ControlValue::Int(-500)
        );
        let metas = vec![ControlMeta {
            id: ControlId(0x009a_0902),
            name: "Exposure Time, Absolute".into(),
            ..meta(
                ControlKind::Int,
                ControlValue::Int(1),
                ControlValue::Int(5000),
            )
        }];
        let r = resolve(
            &metas,
            BackendKind::V4l2,
            StandardControl::ExposureUs.into(),
        )
        .unwrap();
        assert_eq!((r.id.0, r.scale), (0x009a_0902, Scale::Per100Us));
        assert!(
            resolve(
                &metas,
                BackendKind::Native,
                StandardControl::ExposureUs.into()
            )
            .is_err()
        );
        let r = resolve(&metas, BackendKind::V4l2, StandardControl::FrameRate.into()).unwrap();
        assert_eq!(r.id, SERVICE_FRAME_RATE);
    }
}
