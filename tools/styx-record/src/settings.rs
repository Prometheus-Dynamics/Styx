//! The settings file `<name>.json`: the camera settings in effect (exposure, gain, frame rate,
//! size, format, camera, AE/AWB), every control, changes while recording, and what was
//! recorded. Also an Eidos fixture descriptor: `width`, `height`, `frames`,
//! `raw_gray_bytes`, `raw_gray_sha256`.

use styx::ipc::StandardControl;
use styx::prelude::*;

use crate::json::{self, Object, array, number, opt, string};
use crate::sidecar::{Row, clock_name};
use crate::source::{Control, SourceInfo};

/// Bumped when the file's fields change meaning.
pub const SETTINGS_VERSION: u32 = 1;

/// A value that changed while recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The recording's frame (or still) index it was seen at.
    pub frame: u64,
    /// ns since the recording started.
    pub at_ns: u64,
    pub name: String,
    /// The new value, as JSON.
    pub value: String,
    /// `control` (read back from the camera) or `frame` (a frame's own metadata).
    pub from: &'static str,
}

/// Per-frame exposure, gain and frame duration, when frames carry them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSettings {
    pub exposure_us: Option<f64>,
    pub analogue_gain: Option<f32>,
    pub frame_duration_us: Option<f64>,
}

impl FrameSettings {
    pub fn of(row: &Row) -> Self {
        Self {
            exposure_us: row.exposure_us,
            analogue_gain: row.analogue_gain,
            frame_duration_us: row.frame_duration_us,
        }
    }
}

/// The recording's outcome.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub complete: bool,
    pub stop_reason: String,
    pub frames: u64,
    pub raw_file: Option<String>,
    pub raw_gray_bytes: u64,
    pub raw_gray_sha256: Option<String>,
    pub stills: Vec<String>,
    pub csv_file: String,
    /// Camera frames missing between recorded frames (sequence gaps; timestamps without them).
    pub dropped: u64,
    pub gaps: u64,
    /// Frames the disk writer had no room for (the disk did not keep up).
    pub writer_dropped: u64,
    pub writer_max_queued: usize,
    pub restarts_caused: Option<u64>,
    pub first_timestamp_ns: Option<u64>,
    pub last_timestamp_ns: Option<u64>,
    pub clock: Option<TimestampClock>,
    pub errors: Vec<String>,
}

pub struct Settings {
    pub commit: String,
    pub start_unix_ns: u64,
    pub end_unix_ns: Option<u64>,
    pub mode: &'static str,
    pub source: SourceInfo,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_format: Option<String>,
    pub initial: Vec<Control>,
    current: Vec<Control>,
    pub first_frame: Option<FrameSettings>,
    last_frame: Option<FrameSettings>,
    pub changes: Vec<Change>,
    pub outcome: Outcome,
}

/// Changes kept in the file at most (per-frame AE changes of a long recording are in the CSV).
const MAX_CHANGES: usize = 10_000;

pub fn commit() -> &'static str {
    env!("STYX_RECORD_COMMIT")
}

impl Settings {
    pub fn new(source: SourceInfo, mode: &'static str, start_unix_ns: u64) -> Self {
        Self {
            commit: commit().into(),
            start_unix_ns,
            end_unix_ns: None,
            mode,
            source,
            width: None,
            height: None,
            frame_format: None,
            initial: Vec::new(),
            current: Vec::new(),
            first_frame: None,
            last_frame: None,
            changes: Vec::new(),
            outcome: Outcome::default(),
        }
    }

    fn push(&mut self, change: Change) {
        if self.changes.len() < MAX_CHANGES {
            self.changes.push(change);
        }
    }

    /// The controls read back now: the first read is the initial state, later reads record
    /// what changed.
    pub fn controls(&mut self, now: Vec<Control>, frame: u64, at_ns: u64) {
        if self.initial.is_empty() && self.current.is_empty() {
            self.initial = now.clone();
            self.current = now;
            return;
        }
        for control in &now {
            let before = self.current.iter().find(|c| c.id == control.id);
            if before.is_none_or(|b| b.value != control.value) {
                self.push(Change {
                    frame,
                    at_ns,
                    name: control.name.clone(),
                    value: opt(control.value.as_ref(), value_json),
                    from: "control",
                });
            }
        }
        self.current = now;
    }

    /// A frame's own settings (frames that carry them): changes recorded.
    pub fn frame(&mut self, row: &Row, at_ns: u64) {
        let now = FrameSettings::of(row);
        if now == FrameSettings::default() {
            return;
        }
        if self.first_frame.is_none() {
            self.first_frame = Some(now);
        }
        if let Some(last) = self.last_frame
            && last != now
        {
            let fields = [
                ("exposure_us", last.exposure_us, now.exposure_us),
                (
                    "analogue_gain",
                    last.analogue_gain.map(f64::from),
                    now.analogue_gain.map(f64::from),
                ),
                (
                    "frame_duration_us",
                    last.frame_duration_us,
                    now.frame_duration_us,
                ),
            ];
            for (name, a, b) in fields {
                if a != b {
                    self.push(Change {
                        frame: row.index,
                        at_ns,
                        name: name.into(),
                        value: opt(b, number),
                        from: "frame",
                    });
                }
            }
        }
        self.last_frame = Some(now);
    }

    /// A standard control's initial value: by its standard id, its native name, or libcamera's.
    fn standard(&self, control: StandardControl) -> Option<&ControlValue> {
        let libcamera = match control {
            StandardControl::ExposureUs => "ExposureTime",
            StandardControl::Gain => "AnalogueGain",
            StandardControl::AeEnable => "AeEnable",
            StandardControl::ExposureValue => "ExposureValue",
            StandardControl::AwbEnable => "AwbEnable",
            StandardControl::ColourTemperature => "ColourTemperature",
            StandardControl::LensPosition => "LensPosition",
            _ => "",
        };
        self.initial
            .iter()
            .find(|c| {
                c.standard == Some(control) || c.name == control.name() || c.name == libcamera
            })
            .and_then(|c| c.value.as_ref())
    }

    fn standard_num(&self, control: StandardControl) -> Option<f64> {
        self.standard(control).and_then(value_f64)
    }

    fn standard_bool(&self, control: StandardControl) -> Option<bool> {
        self.standard(control).and_then(|v| match v {
            ControlValue::Bool(b) => Some(*b),
            ControlValue::Int(i) => Some(*i != 0),
            ControlValue::Uint(u) => Some(*u != 0),
            _ => None,
        })
    }

    /// The whole file.
    pub fn to_json(&self) -> String {
        let s = &self.source;
        let o = &self.outcome;
        let frame = self.first_frame.unwrap_or_default();
        let fps = s.fps.map(f64::from).or_else(|| {
            self.standard_num(StandardControl::FrameRate)
                .filter(|v| *v > 0.0)
        });
        let frame_duration_us = frame
            .frame_duration_us
            .or_else(|| fps.filter(|f| *f > 0.0).map(|f| 1e6 / f));

        let mut camera = Object::new(2);
        camera
            .str("name", &s.camera)
            .raw(
                "keys",
                format!(
                    "[{}]",
                    s.camera_keys
                        .iter()
                        .map(|k| string(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .raw("backend", opt(s.backend.as_deref(), string))
            .raw("source", string(s.kind))
            .raw(
                "socket",
                opt(s.socket.as_ref(), |p| string(&p.display().to_string())),
            )
            .raw("plan", opt(s.plan.as_deref(), string));

        let mut set = Object::new(2);
        set.raw(
            "exposure_us",
            opt(
                frame
                    .exposure_us
                    .or_else(|| self.standard_num(StandardControl::ExposureUs)),
                number,
            ),
        )
        .raw(
            "analogue_gain",
            opt(
                frame
                    .analogue_gain
                    .map(f64::from)
                    .or_else(|| self.standard_num(StandardControl::Gain)),
                number,
            ),
        )
        .raw("fps", opt(fps, number))
        .raw("frame_duration_us", opt(frame_duration_us, number))
        .raw(
            "ae_enable",
            opt(self.standard_bool(StandardControl::AeEnable), |b| {
                b.to_string()
            }),
        )
        .raw(
            "awb_enable",
            opt(self.standard_bool(StandardControl::AwbEnable), |b| {
                b.to_string()
            }),
        )
        .raw(
            "exposure_value",
            opt(self.standard_num(StandardControl::ExposureValue), number),
        )
        .raw(
            "colour_temperature_k",
            opt(
                self.standard_num(StandardControl::ColourTemperature),
                number,
            ),
        )
        .raw(
            "red_gain",
            opt(self.standard_num(StandardControl::RedGain), number),
        )
        .raw(
            "blue_gain",
            opt(self.standard_num(StandardControl::BlueGain), number),
        )
        .raw(
            "values_from",
            string(if self.first_frame.is_some() {
                "frame metadata (exposure, gain, frame duration), camera controls (the rest)"
            } else {
                "camera controls"
            }),
        );

        let controls: Vec<String> = self
            .initial
            .iter()
            .map(|c| {
                format!(
                    "{{\"name\": {}, \"id\": {}, \"value\": {}, \"standard\": {}}}",
                    string(&c.name),
                    c.id,
                    opt(c.value.as_ref(), value_json),
                    opt(c.standard, |s| string(s.name()))
                )
            })
            .collect();
        let changes: Vec<String> = self
            .changes
            .iter()
            .map(|c| {
                format!(
                    "{{\"frame\": {}, \"at_ms\": {}, \"name\": {}, \"value\": {}, \"from\": {}}}",
                    c.frame,
                    number(c.at_ns as f64 / 1e6),
                    string(&c.name),
                    c.value,
                    string(c.from)
                )
            })
            .collect();
        let stills: Vec<String> = o.stills.iter().map(|f| string(f)).collect();
        let errors: Vec<String> = o.errors.iter().map(|e| string(e)).collect();

        let mut root = Object::new(0);
        root.str("recorder", "styx-record")
            .raw("settings_version", SETTINGS_VERSION.to_string())
            .str("styx_version", env!("CARGO_PKG_VERSION"))
            .str("styx_commit", &self.commit)
            .raw("start_unix_ns", self.start_unix_ns.to_string())
            .str("start_utc", &json::utc(self.start_unix_ns))
            .raw("end_unix_ns", opt(self.end_unix_ns, |v| v.to_string()))
            .raw("end_utc", opt(self.end_unix_ns, |v| string(&json::utc(v))))
            .raw("complete", o.complete.to_string())
            .str("stop_reason", &o.stop_reason)
            .str("mode", self.mode)
            .str(
                "layout",
                "raw grey: the Y plane, width*height bytes per frame, rows top to bottom with \
                 no padding, frames back to back, no header",
            )
            .str("format", "R8")
            .raw("width", opt(self.width, |v| v.to_string()))
            .raw("height", opt(self.height, |v| v.to_string()))
            .raw("frame_format", opt(self.frame_format.as_deref(), string))
            .raw(
                "source_stream",
                opt(self.source.source_stream.as_deref(), string),
            )
            .raw("frames", o.frames.to_string())
            .raw("raw_file", opt(o.raw_file.as_deref(), string))
            .raw("raw_gray_bytes", o.raw_gray_bytes.to_string())
            .raw("raw_gray_sha256", opt(o.raw_gray_sha256.as_deref(), string))
            .raw("stills", array(&stills, 2))
            .str("frames_csv", &o.csv_file)
            .raw("camera", camera.finish())
            .raw("settings", set.finish())
            .raw("controls", array(&controls, 2))
            .raw("changes", array(&changes, 2))
            .str("timestamp_clock", clock_name(o.clock))
            .raw(
                "first_timestamp_ns",
                opt(o.first_timestamp_ns, |v| v.to_string()),
            )
            .raw(
                "last_timestamp_ns",
                opt(o.last_timestamp_ns, |v| v.to_string()),
            )
            .raw("dropped_frames", o.dropped.to_string())
            .raw("drop_gaps", o.gaps.to_string())
            .raw("writer_dropped_frames", o.writer_dropped.to_string())
            .raw("writer_max_queued", o.writer_max_queued.to_string())
            .raw(
                "service_restarts_caused",
                opt(o.restarts_caused, |v| v.to_string()),
            )
            .raw("errors", array(&errors, 2));
        let mut out = root.finish();
        out.push('\n');
        out
    }
}

fn value_f64(v: &ControlValue) -> Option<f64> {
    match v {
        ControlValue::Bool(b) => Some(f64::from(u8::from(*b))),
        ControlValue::Int(i) => Some(f64::from(*i)),
        ControlValue::Uint(u) => Some(f64::from(*u)),
        ControlValue::Float(f) => Some(f64::from(*f)),
        _ => None,
    }
}

/// A control value as JSON: a number, a bool, `null`, or rectangles as `[x, y, w, h]`.
pub fn value_json(v: &ControlValue) -> String {
    let rect = |r: &ControlRect| format!("[{}, {}, {}, {}]", r.x, r.y, r.width, r.height);
    match v {
        ControlValue::None => "null".into(),
        ControlValue::Bool(b) => b.to_string(),
        ControlValue::Int(i) => i.to_string(),
        ControlValue::Uint(u) => u.to_string(),
        ControlValue::Float(f) => number(f64::from(*f)),
        ControlValue::Rect(r) => rect(r),
        ControlValue::Rects(rs) => {
            format!("[{}]", rs.iter().map(rect).collect::<Vec<_>>().join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(id: u32, name: &str, value: ControlValue) -> Control {
        Control {
            id,
            name: name.into(),
            value: Some(value),
            standard: None,
        }
    }

    #[test]
    fn records_control_changes_after_the_first_read() {
        let mut s = Settings::new(SourceInfo::default(), "every", 0);
        s.controls(
            vec![
                control(1, "ExposureTime", ControlValue::Int(5000)),
                control(2, "AeEnable", ControlValue::Bool(true)),
            ],
            0,
            0,
        );
        assert!(s.changes.is_empty());
        s.controls(
            vec![
                control(1, "ExposureTime", ControlValue::Int(8000)),
                control(2, "AeEnable", ControlValue::Bool(true)),
            ],
            30,
            1_000_000_000,
        );
        assert_eq!(s.changes.len(), 1);
        assert_eq!(s.changes[0].name, "ExposureTime");
        assert_eq!(s.changes[0].value, "8000");
        assert_eq!(s.changes[0].frame, 30);
        // libcamera names answer the standard settings.
        assert_eq!(s.standard_num(StandardControl::ExposureUs), Some(5000.0));
        assert_eq!(s.standard_bool(StandardControl::AeEnable), Some(true));
    }

    #[test]
    fn frame_settings_win_and_their_changes_are_kept() {
        let mut s = Settings::new(SourceInfo::default(), "every", 0);
        let row = |index, exposure| Row {
            index,
            exposure_us: Some(exposure),
            analogue_gain: Some(2.0),
            frame_duration_us: Some(33_333.0),
            ..Row::default()
        };
        s.frame(&row(0, 1000.0), 0);
        s.frame(&row(1, 1000.0), 1);
        s.frame(&row(2, 2000.0), 2);
        assert_eq!(s.changes.len(), 1);
        assert_eq!(s.changes[0].value, "2000");
        let json = s.to_json();
        assert!(json.contains("\"exposure_us\": 1000"), "{json}");
        assert!(json.contains("\"frame_duration_us\": 33333"), "{json}");
    }
}
