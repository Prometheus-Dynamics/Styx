//! Thin adapters: the three backends' existing stream types behind [`Capture`].
//!
//! No backend crate is changed. Where a backend lacks something the trait asks for, the adapter
//! says so in its docs.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use styx_controls::{Control, ControlValue, DeviceKind, FrameMetadata, Update};
use styx_frame::FrameLease;
use styx_libcamera::{
    Capture as LibcameraStream, CaptureConfig, DeviceEntry, Error as LibcameraError,
    StandardControl as LibcameraStandard, StandardRange as LibcameraRange,
};
use styx_native_run::{Clock, Event, Ranges, Session, State};
use styx_v4l2::{
    NodeControls, RawControl, StandardControl, StandardRange, State as V4l2State, Stream,
    StreamConfig,
};

use crate::capture::{Capture, CaptureError, Edge, RateReport, SensorTiming, Status};
use crate::controls::{
    Access, ControlsInfo, DeviceInfo, MenuEntry, NATIVE_TEST_PATTERN, Real, RealValues, RectValues,
    Span, StandardInfo,
};

impl<C: Clock> std::fmt::Debug for NativeCapture<'_, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCapture")
            .field("session", &self.session)
            .finish()
    }
}

/// A native camera: a started [`Session`] (Lemnos sensor, receiver and ISP), with its
/// controls and retries as the session runs them.
///
/// The session must be configured with [`Session::with_controls`] first, since it does not start
/// without it. [`Update::frame`] is honoured: the session applies each value from its frame.
pub struct NativeCapture<'a, C: Clock> {
    session: Session<'a, C>,
    /// The achieved rate of the last frame delivered, from its metadata (0 until one is).
    achieved_milli: u32,
    /// The stall or return the last wait found, for the caller to take ([`Capture::take_edge`]).
    edge: Option<Edge>,
    /// What the controls answer was built from last, and the count that moves when it changes.
    controls_seen: (Option<Ranges>, Option<u16>, u16),
    controls_generation: u64,
}

impl<'a, C: Clock> NativeCapture<'a, C> {
    /// Starts the session. A start refused for a device contract another build's is
    /// [`CaptureError::Contract`], with the contract the session names.
    pub fn start(mut session: Session<'a, C>) -> Result<Self, CaptureError> {
        if let Err(e) = session.start() {
            return Err(match session.status().contract.clone() {
                Some(contract) => CaptureError::Contract {
                    contract,
                    text: e.to_string(),
                },
                None => CaptureError::Failed(e.to_string()),
            });
        }
        let mut capture = Self {
            session,
            achieved_milli: 0,
            edge: None,
            controls_seen: (None, None, 0),
            controls_generation: 1,
        };
        capture.track_controls();
        Ok(capture)
    }

    /// Moves the controls count when the mode's ranges, the frame length in force or the test
    /// pattern changed. Plain values compared: nothing allocated.
    fn track_controls(&mut self) {
        let seen = (
            self.session.ranges(),
            self.session.test_patterns(),
            self.session.test_pattern(),
        );
        if seen != self.controls_seen {
            self.controls_seen = seen;
            self.controls_generation += 1;
        }
    }
}

/// A native camera's controls: exposure, gain and frame rate with the ranges the selected mode gives
/// (the sensor contract's `Mode`), and the sensor's test patterns as a numbered menu (the contract
/// names none). Nothing else is settable on this path, so nothing else is listed.
fn native_controls(ranges: Option<Ranges>, patterns: Option<u16>, pattern: u16) -> ControlsInfo {
    let mut info = ControlsInfo::default();
    if let Some(r) = ranges {
        let manual = |(min, max): (u32, u32)| Span::Manual { min, max };
        info.standard.push(StandardInfo {
            control: Control::Exposure,
            range: Some(manual(r.exposure_us)),
            // The sensor moves in exposure codes (a fraction of its line time), not whole
            // microseconds: no even step in this unit.
            step: None,
            default: Some(ControlValue::Manual(r.initial.exposure_us)),
            effective: Some((r.exposure_us.0, r.exposure_effective_max_us)),
            auto: Some(false),
            access: Access::ReadWrite,
        });
        info.standard.push(StandardInfo {
            control: Control::Gain,
            range: r.gain_milli.map(manual),
            step: r.gain_step_milli,
            default: Some(ControlValue::Manual(r.initial.gain_milli)),
            effective: r.gain_milli,
            auto: Some(false),
            access: Access::ReadWrite,
        });
        info.standard.push(StandardInfo {
            control: Control::FrameRate,
            range: r.frame_rate_milli.map(manual),
            step: None,
            default: Some(ControlValue::Manual(r.initial.frame_rate_milli)),
            effective: r.frame_rate_milli,
            auto: Some(false),
            access: Access::ReadWrite,
        });
    }
    if let Some(count) = patterns.filter(|n| *n > 0) {
        info.device.push(DeviceInfo {
            id: NATIVE_TEST_PATTERN,
            name: "test_pattern".to_string(),
            kind: DeviceKind::Menu,
            min: Some(0),
            max: Some(i64::from(count)),
            step: Some(1),
            default: Some(0),
            effective: Some((0, i64::from(count))),
            value: Some(i64::from(pattern)),
            standard: None,
            access: Access::ReadWrite,
            real: None,
            rect: None,
            menu: (0..=i64::from(count))
                .map(|value| MenuEntry { value, label: None })
                .collect(),
        });
    }
    info
}

impl<C: Clock> Capture for NativeCapture<'_, C> {
    fn lemnos_skipped(&self) -> u64 {
        self.session.skipped()
    }

    fn lemnos_at_limit(&self) -> u64 {
        self.session.at_limit()
    }

    fn hold(&self) -> Option<u32> {
        self.session.hold()
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        let event = self.session.next_event(timeout).map_err(|e| match e {
            styx_native_run::Error::Stopped => CaptureError::Stopped,
            other => CaptureError::Failed(other.to_string()),
        })?;
        self.track_controls();
        match event {
            Event::Frame(lease) => {
                if lease.meta().achieved_fps_milli != 0 {
                    self.achieved_milli = lease.meta().achieved_fps_milli;
                }
                Ok(Some(lease))
            }
            Event::Idle => Ok(None),
            Event::Stalled { since_ns, text } => {
                self.edge = Some(Edge::Stalled { since_ns, text });
                Ok(None)
            }
            Event::Resumed(carried) => {
                self.edge = Some(Edge::Resumed { carried });
                Ok(None)
            }
        }
    }

    fn take_edge(&mut self) -> Option<Edge> {
        self.edge.take()
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        self.session
            .request_controls(updates)
            .map(|_| ())
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn sensor_timing(&self) -> Option<SensorTiming> {
        self.session.ranges().map(|r| SensorTiming {
            exposure_us: r.exposure_us,
            exposure_effective_max_us: r.exposure_effective_max_us,
            gain_milli: r.gain_milli,
            frame_rate_milli: r.frame_rate_milli,
            delays: (r.delays.exposure, r.delays.gain, r.delays.frame_length),
        })
    }

    fn controls_info(&self) -> ControlsInfo {
        native_controls(
            self.session.ranges(),
            self.session.test_patterns(),
            self.session.test_pattern(),
        )
    }

    fn controls_generation(&self) -> u64 {
        self.controls_generation
    }

    fn set_device(&mut self, id: u32, value: i64) -> Result<(), CaptureError> {
        if id != NATIVE_TEST_PATTERN {
            return Err(CaptureError::Failed(format!("no device control {id:#x}")));
        }
        let code = u16::try_from(value)
            .map_err(|_| CaptureError::Failed(format!("no test pattern {value}")))?;
        self.session
            .set_test_pattern(code)
            .map_err(|e| CaptureError::Failed(e.to_string()))?;
        self.track_controls();
        Ok(())
    }

    fn status(&self) -> Status {
        let status = self.session.status();
        match &status.state {
            // A stall: the camera is retried and the stream stays attached (health, not a fault).
            State::Streaming if status.stall.is_some() => Status::Retrying {
                reason: status.stall.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
            State::Streaming => Status::Streaming,
            State::Stopped => Status::Stopped,
            State::Backoff => Status::Retrying {
                reason: status.last_error.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
            State::Off => Status::Failed {
                reason: status.last_error.clone().unwrap_or_default(),
                layout: status.layout,
                contract: status.contract.clone(),
            },
        }
    }

    fn rate(&self) -> RateReport {
        // The rate in force on the last frame; before a frame, the rate the camera started at.
        let start = self.session.frame_rate();
        let achieved = match self.achieved_milli {
            0 => start.map(|r| r.achieved_milli),
            milli => Some(milli),
        };
        RateReport {
            achieved_milli: achieved.filter(|milli| *milli != 0),
            clamped: start.is_some_and(|r| r.clamped),
        }
    }

    fn stop(&mut self) {
        let _ = self.session.stop();
    }
}

impl std::fmt::Debug for V4l2Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Capture")
            .field("running", &self.stream.is_some())
            .finish()
    }
}

/// A V4L2 capture node: a running [`Stream`] from the `lemnos-v4l2` library.
///
/// [`Update::frame`] is not honoured: `Stream::set_control` applies a value on the next frame the
/// driver takes, and that is all the stream offers.
pub struct V4l2Capture {
    stream: Option<Stream>,
}

impl V4l2Capture {
    /// Opens and starts the node at `path` (the listing's key) with `config`.
    pub fn start(path: &str, config: StreamConfig) -> Result<Self, CaptureError> {
        let stream =
            Stream::start(path, config).map_err(|e| CaptureError::Failed(e.to_string()))?;
        Ok(Self {
            stream: Some(stream),
        })
    }
}

impl Capture for V4l2Capture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        let stream = self.stream.as_ref().ok_or(CaptureError::Stopped)?;
        match stream.frames().recv_timeout(timeout) {
            Ok(lease) => Ok(Some(lease)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
        }
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        let stream = self.stream.as_ref().ok_or(CaptureError::Stopped)?;
        for update in updates {
            stream
                .set_control(update.control, update.value)
                .map_err(|e| CaptureError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    fn status(&self) -> Status {
        match &self.stream {
            None => Status::Stopped,
            Some(stream) => match stream.status().state {
                V4l2State::Streaming => Status::Streaming,
                V4l2State::Retrying { error, .. } => Status::Retrying {
                    reason: error,
                    layout: None,
                    contract: None,
                },
                V4l2State::Stopped => Status::Stopped,
                V4l2State::Failed { error } => Status::Failed {
                    reason: error,
                    layout: None,
                    contract: None,
                },
            },
        }
    }

    fn controls_info(&self) -> ControlsInfo {
        match &self.stream {
            Some(stream) => v4l2_controls(&stream.controls().1),
            None => ControlsInfo::default(),
        }
    }

    fn controls_generation(&self) -> u64 {
        self.stream.as_ref().map_or(0, Stream::controls_generation)
    }

    fn set_device(&mut self, id: u32, value: i64) -> Result<(), CaptureError> {
        let stream = self.stream.as_ref().ok_or(CaptureError::Stopped)?;
        stream
            .set_device_control(id, value)
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn stop(&mut self) {
        // Dropping the stream stops it and waits for its thread.
        self.stream = None;
    }
}

fn v4l2_access(read_only: bool, inactive: bool, grabbed: bool, disabled: bool) -> Access {
    // The strongest reason first: a disabled control is not there to set at all.
    if disabled {
        Access::Disabled
    } else if read_only {
        Access::ReadOnly
    } else if grabbed {
        Access::Grabbed
    } else if inactive {
        Access::Inactive
    } else {
        Access::ReadWrite
    }
}

fn v4l2_standard(c: &StandardControl) -> StandardInfo {
    let range = c.range.map(|r| match r {
        StandardRange::Manual { min, max } => Span::Manual { min, max },
        StandardRange::Flag => Span::Flag,
    });
    StandardInfo {
        control: c.control,
        range,
        step: c.step,
        default: c.default,
        // V4L2 has no narrower range in effect than the one the driver gives now (it reports a
        // changed range as the control's range, read again on its event).
        effective: match range {
            Some(Span::Manual { min, max }) => Some((min, max)),
            _ => None,
        },
        auto: Some(c.auto),
        access: v4l2_access(c.read_only, c.inactive, c.grabbed, c.disabled),
    }
}

fn v4l2_device(raw: &RawControl) -> DeviceInfo {
    let control = raw.device_control();
    let menu = raw
        .menu
        .iter()
        .map(|item| MenuEntry {
            value: item.value,
            label: Some(item.name.clone()),
        })
        .collect();
    let bounded = !matches!(control.kind, DeviceKind::Button | DeviceKind::Other);
    DeviceInfo {
        id: raw.id,
        name: raw.name.clone(),
        kind: control.kind,
        min: bounded.then_some(raw.min),
        max: bounded.then_some(raw.max),
        // The driver's step; 0 is "does not say".
        step: (bounded && raw.step != 0).then_some(raw.step),
        default: bounded.then_some(raw.default),
        effective: bounded.then_some((raw.min, raw.max)),
        value: raw.value,
        standard: raw.mapped,
        access: v4l2_access(raw.read_only, raw.inactive, raw.grabbed, raw.disabled),
        menu,
        real: None,
        rect: None,
    }
}

/// A V4L2 node's controls: the Styx controls it maps in their units (exposure in microseconds from
/// the driver's 100 us units, normalised controls 0..1000 over the driver's range), and every
/// driver control as the driver gives it, by its `V4L2_CID_*`. Control classes (headings) are left
/// out: they are not controls.
fn v4l2_controls(node: &NodeControls) -> ControlsInfo {
    ControlsInfo {
        standard: node.standard.iter().map(v4l2_standard).collect(),
        device: node
            .devices
            .iter()
            .filter(|raw| raw.control_type != styx_v4l2::ControlType::CtrlClass)
            .map(v4l2_device)
            .collect(),
    }
}

impl std::fmt::Debug for LibcameraCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibcameraCapture")
            .field("running", &self.camera.is_some())
            .finish()
    }
}

/// A libcamera camera. Frames are pushed by libcamera; they are queued here for
/// [`Capture::next_frame`] up to the configured buffer count. When the queue is full the frame is
/// dropped (its buffer goes back to libcamera), never waited for.
///
/// [`Update::frame`] is not honoured: libcamera's `set_control` applies from the next request
/// queued.
pub struct LibcameraCapture {
    camera: Option<LibcameraStream>,
    frames: Receiver<FrameLease<FrameMetadata>>,
}

impl LibcameraCapture {
    /// Opens camera `id` (the listing's key) with `config`.
    pub fn open(id: &str, config: CaptureConfig) -> Result<Self, CaptureError> {
        let (tx, frames): (SyncSender<FrameLease<FrameMetadata>>, _) =
            mpsc::sync_channel(config.buffers.max(1) as usize);
        let sink = Box::new(move |lease: FrameLease<FrameMetadata>| {
            // A full queue (or a gone reader) drops the lease here: its buffer goes back to
            // libcamera. Nothing waits.
            let _ = tx.try_send(lease);
        });
        let camera = LibcameraStream::open(id, config, sink).map_err(libcamera_error)?;
        Ok(Self {
            camera: Some(camera),
            frames,
        })
    }
}

fn libcamera_error(e: LibcameraError) -> CaptureError {
    match e {
        LibcameraError::Unsupported(what) => CaptureError::Unsupported(what),
        other => CaptureError::Failed(other.to_string()),
    }
}

impl Capture for LibcameraCapture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError> {
        if self.camera.is_none() {
            return Err(CaptureError::Stopped);
        }
        match self.frames.recv_timeout(timeout) {
            Ok(lease) => Ok(Some(lease)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
        }
    }

    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError> {
        let camera = self.camera.as_ref().ok_or(CaptureError::Stopped)?;
        for update in updates {
            camera
                .set_control(*update)
                .map_err(|e| CaptureError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    fn status(&self) -> Status {
        match self.camera {
            Some(_) => Status::Streaming,
            None => Status::Stopped,
        }
    }

    fn controls_info(&self) -> ControlsInfo {
        match &self.camera {
            Some(camera) => {
                libcamera_controls(&camera.standard_controls(), &camera.device_controls())
            }
            None => ControlsInfo::unreported(),
        }
    }

    fn controls_generation(&self) -> u64 {
        self.camera
            .as_ref()
            .map_or(0, LibcameraStream::controls_generation)
    }

    fn set_device(&mut self, id: u32, value: i64) -> Result<(), CaptureError> {
        let camera = self.camera.as_ref().ok_or(CaptureError::Stopped)?;
        camera
            .set_device_control(id, value)
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn set_device_real(&mut self, id: u32, value: f32) -> Result<(), CaptureError> {
        let camera = self.camera.as_ref().ok_or(CaptureError::Stopped)?;
        camera
            .set_device_real(id, value)
            .map_err(|e| CaptureError::Failed(e.to_string()))
    }

    fn stop(&mut self) {
        self.camera = None;
    }
}

fn libcamera_standard(c: &LibcameraStandard) -> StandardInfo {
    let range = c.range.map(|r| match r {
        LibcameraRange::Manual { min, max } => Span::Manual { min, max },
        LibcameraRange::Flag => Span::Flag,
        LibcameraRange::Rect => Span::Rect,
    });
    StandardInfo {
        control: c.control,
        range,
        // libcamera has no step.
        step: None,
        default: c.default,
        // libcamera's ranges are read once the camera is configured: the range is the one in
        // effect.
        effective: match range {
            Some(Span::Manual { min, max }) => Some((min, max)),
            _ => None,
        },
        auto: Some(c.auto),
        access: if c.read_only {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        },
    }
}

fn libcamera_device(entry: &DeviceEntry) -> DeviceInfo {
    let control = &entry.control;
    // The integer fields are for the integer kinds; a float or rectangle control's numbers are in
    // `real` or `rect`, read at their own type.
    let bounded = matches!(
        control.kind,
        DeviceKind::Boolean | DeviceKind::Integer | DeviceKind::Integer64 | DeviceKind::Menu
    );
    DeviceInfo {
        id: control.id,
        name: entry.name.clone(),
        kind: control.kind,
        min: bounded.then_some(control.min),
        max: bounded.then_some(control.max),
        step: None,
        default: bounded.then_some(control.default),
        effective: bounded.then_some((control.min, control.max)),
        value: entry.value,
        standard: entry.mapped,
        access: if control.read_only {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        },
        menu: entry
            .menu
            .iter()
            .map(|item| MenuEntry {
                value: item.value,
                label: item.name.clone(),
            })
            .collect(),
        real: entry.real.map(|r| {
            let real = |v: Option<f32>| v.filter(|v| v.is_finite()).map(Real);
            RealValues {
                min: real(r.min),
                max: real(r.max),
                default: real(r.default),
                // libcamera's ranges are read once the camera is configured: in effect now.
                effective: real(r.min).zip(real(r.max)),
                value: real(r.value),
            }
        }),
        rect: entry.rect.map(|r| RectValues {
            min: r.min,
            max: r.max,
            default: r.default,
            value: r.value,
        }),
    }
}

/// A libcamera camera's controls: the Styx controls the backend sets, with the camera's configured
/// ranges in Styx units (exposure in microseconds as libcamera has it, gain and gamma floats in
/// thousandths, normalised controls 0..1000 over the camera's float range), and every control it
/// registers by libcamera's numeric id. A float or rectangle control's range is unknown on the
/// device line (the standard line converts the floats it maps). libcamera has no step.
fn libcamera_controls(standard: &[LibcameraStandard], device: &[DeviceEntry]) -> ControlsInfo {
    ControlsInfo {
        standard: standard.iter().map(libcamera_standard).collect(),
        device: device.iter().map(libcamera_device).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use styx_v4l2::{ControlType, DeviceMenuItem};

    fn raw(id: u32, control_type: ControlType) -> RawControl {
        RawControl {
            id,
            name: format!("control {id}"),
            control_type,
            min: 0,
            max: 10,
            step: 1,
            default: 5,
            read_only: false,
            inactive: false,
            grabbed: false,
            disabled: false,
            value: Some(5),
            menu: Vec::new(),
            mapped: None,
        }
    }

    #[test]
    fn a_v4l2_controls_access_is_its_strongest_reason() {
        assert_eq!(v4l2_access(false, false, false, false), Access::ReadWrite);
        assert_eq!(v4l2_access(false, true, false, false), Access::Inactive);
        assert_eq!(v4l2_access(false, true, true, false), Access::Grabbed);
        assert_eq!(v4l2_access(true, true, true, false), Access::ReadOnly);
        assert_eq!(v4l2_access(true, true, true, true), Access::Disabled);
    }

    /// Class headings are not controls; a button has no range; a step of 0 is "does not say";
    /// menu entries keep the driver's names.
    #[test]
    fn a_v4l2_nodes_device_controls_are_the_drivers_less_its_headings() {
        let mut menu = raw(3, ControlType::Menu);
        menu.menu = vec![
            DeviceMenuItem {
                value: 0,
                name: "Off".into(),
            },
            DeviceMenuItem {
                value: 2,
                name: "On".into(),
            },
        ];
        let mut stepless = raw(4, ControlType::Integer);
        stepless.step = 0;
        stepless.grabbed = true;
        let node = NodeControls {
            devices: vec![
                raw(1, ControlType::CtrlClass),
                raw(2, ControlType::Button),
                menu,
                stepless,
            ],
            standard: Vec::new(),
        };
        let info = v4l2_controls(&node);
        let ids: Vec<u32> = info.device.iter().map(|d| d.id).collect();
        assert_eq!(ids, [2, 3, 4]);

        let button = &info.device[0];
        assert_eq!(button.kind, DeviceKind::Button);
        assert_eq!(
            (button.min, button.max, button.step, button.default),
            (None, None, None, None)
        );
        assert_eq!(button.effective, None);

        let menu = &info.device[1];
        assert_eq!(menu.kind, DeviceKind::Menu);
        assert_eq!(
            menu.menu,
            [
                MenuEntry {
                    value: 0,
                    label: Some("Off".into())
                },
                MenuEntry {
                    value: 2,
                    label: Some("On".into())
                },
            ]
        );
        assert_eq!(menu.step, Some(1));
        assert_eq!(menu.effective, Some((0, 10)));

        let stepless = &info.device[2];
        assert_eq!(stepless.step, None);
        assert_eq!(stepless.access, Access::Grabbed);
        assert_eq!(stepless.value, Some(5));
    }

    fn entry(id: u32, name: &str, kind: DeviceKind, read_only: bool) -> DeviceEntry {
        DeviceEntry {
            control: styx_controls::DeviceControl {
                id,
                name: styx_controls::DeviceName::new(name),
                kind,
                min: 0,
                max: 3,
                step: 0,
                default: 1,
                read_only,
            },
            mapped: None,
            name: name.into(),
            menu: Vec::new(),
            value: None,
            real: None,
            rect: None,
        }
    }

    /// A libcamera device line keeps libcamera's full name, menu labels and the value last set; a
    /// float control's truncated bounds are unknown, and libcamera has no step. A standard line's
    /// range is the one in effect.
    #[test]
    fn a_libcamera_cameras_controls_keep_its_names_and_leave_floats_unknown() {
        let long = "VendorControlWithANameLongerThanThirtyTwoBytes";
        let mut menu = entry(7, long, DeviceKind::Menu, false);
        menu.menu = vec![
            styx_libcamera::MenuItem {
                value: 0,
                name: Some("Off".into()),
            },
            styx_libcamera::MenuItem {
                value: 5,
                name: None,
            },
        ];
        menu.value = Some(5);
        let mut gain = entry(8, "AnalogueGain", DeviceKind::Float, false);
        gain.real = Some(styx_libcamera::FloatValues {
            min: Some(1.0),
            max: Some(f32::INFINITY),
            default: Some(1.0),
            value: Some(2.5),
        });
        gain.mapped = Some(Control::Gain);
        let colour = entry(9, "ColourTemperature", DeviceKind::Integer, true);
        let standard = [LibcameraStandard {
            control: Control::Gain,
            range: Some(LibcameraRange::Manual {
                min: 1_000,
                max: 16_000,
            }),
            default: Some(ControlValue::Manual(1_000)),
            auto: false,
            read_only: false,
        }];
        let info = libcamera_controls(&standard, &[menu, gain, colour]);

        let menu = &info.device[0];
        assert_eq!(menu.name, long);
        assert_eq!((menu.min, menu.max, menu.step), (Some(0), Some(3), None));
        assert_eq!(menu.value, Some(5));
        assert_eq!(
            menu.menu,
            [
                MenuEntry {
                    value: 0,
                    label: Some("Off".into())
                },
                MenuEntry {
                    value: 5,
                    label: None
                },
            ]
        );
        let gain = &info.device[1];
        assert_eq!((gain.min, gain.max, gain.default), (None, None, None));
        let real = gain.real.expect("a float's numbers");
        assert_eq!(real.min, Some(Real(1.0)));
        assert_eq!(real.max, None, "an infinite bound is unknown");
        assert_eq!(
            real.effective, None,
            "no range in effect without both bounds"
        );
        assert_eq!(real.value, Some(Real(2.5)));
        assert_eq!(gain.standard, Some(Control::Gain));
        assert_eq!(info.device[2].access, Access::ReadOnly);

        let gain = info.standard[0];
        assert_eq!(
            gain.range,
            Some(Span::Manual {
                min: 1_000,
                max: 16_000
            })
        );
        assert_eq!(gain.effective, Some((1_000, 16_000)));
        assert_eq!((gain.step, gain.auto), (None, Some(false)));
    }
}
