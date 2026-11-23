//! A camera's 3A loop: auto exposure, auto white balance and flicker avoidance for a native
//! camera's processed stream (`aaa = true` beside `isp`), with every part on, off or manual at run
//! time.
//!
//! - **Where it runs.** On the camera's ISP thread, after the frame it has statistics for is
//!   published: the picture goes out first, then [`styx_controller::Controller::process`] runs on
//!   the frame's statistics. Its ISP settings are for the next frame, and its exposure and gain for
//!   the frame its request names. Nothing waits for it on the publish path.
//! - **Statistics.** Taken inside a pass that already reads every pixel, never a copy of the frame:
//!   on the back end, the raw rows the input pass unpacks ([`styx_pisp::LiveIsp::count_statistics`],
//!   every other quad of every other quad row); on the soft ISP, its own statistics. A frame the
//!   ISP did not make has none, and the loop skips it.
//! - **Sensor requests.** The loop's exposure and gain, each for the frame its request names, are
//!   left on the [`Link`] (the newest replaces an older one not yet taken). The capture thread
//!   takes them between frames and posts them with `Capture::set_controls`, which lands each on its
//!   frame by the sensor's delays. The frame rate is never posted: AE runs within the rate in force.
//! - **Metering against what was applied.** Each frame's exposure and gain are the ones its
//!   metadata reports in force (the sensor's report), so a request still on its way is not taken
//!   for one that has landed. The first [`UNSETTLED`] frames of a stream are not metered.
//! - **Manual wins.** A manual exposure or gain set for frame `F` is posted for `F` as before, and
//!   from then on every request of the loop that lands on `F` or later carries the manual value
//!   (the capture thread replaces it before posting), so no automatic value lands after a manual
//!   one. The loop is told too, and moves only what is left automatic. `ae off` holds both where
//!   they are; `auto` gives a control back to the loop.
//! - **Start.** Frames are never withheld: the first frames carry the configured exposure and gain
//!   (or the tuning's start values) and the ISP's neutral settings until the loop's first step;
//!   the status says `ae_state=converging` until AE reports a lock.
//! - **Settings on the soft ISP** are applied between frames, never inside one, and only when they
//!   moved by more than [`SETTLE`] (a settled loop applies none, so its frames allocate nothing).
//!   The back end takes them with each job's own configuration.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use styx_aaa::{CameraConfig, ControlDelays, Controls, Flicker, FrameMetadata, Statistics, Tuning};
use styx_controller::Controller;
use styx_controls::{Control, ControlValue, Update};
use styx_select::SensorTiming;
use styx_settings::IspSettings;

/// Frames at a stream's start that are not metered (the sensor settling after its start).
pub(crate) const UNSETTLED: u32 = 2;
/// Frames from the frame statistics came from to the frame start where the capture thread posts:
/// the hand-off to the ISP thread and the wait for the next frame.
const ISSUE_LATENCY: u32 = 2;
/// Frames ahead the loop's requests are for, before it has made one: the issue latency and a
/// sensor's usual exposure delay.
const LEAD: u64 = ISSUE_LATENCY as u64 + 2;
/// How far a soft-ISP setting must move to be applied again (relative for gains, absolute for the
/// matrix, curve and tables).
pub(crate) const SETTLE: f64 = 1.0 / 512.0;

/// The names of the controls only the 3A loop has, as `set` and `controls` take them.
pub const NAMES: [&str; 9] = [
    "ae",
    "ev",
    "ae_exposure_max_us",
    "ae_gain_max",
    "awb",
    "red_gain",
    "blue_gain",
    "colour_temperature",
    "flicker",
];

/// What the status says the loop is doing (`isp_3a`, `ae_state`, `awb_gains`, ...).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AaaStatus {
    /// The loop runs on this camera's processed stream.
    pub running: bool,
    /// AE moves the exposure or the gain (on, and not both manual).
    pub ae: bool,
    /// AWB picks the white balance.
    pub awb: bool,
    /// Flicker avoidance (`off`, `50`, `60`, `auto`).
    pub flicker: &'static str,
    /// AE has locked on its target (`None` while AE is off).
    pub ae_settled: Option<bool>,
    /// The red and blue gains in force, in thousandths of green's.
    pub wb_milli: Option<(u32, u32)>,
    /// The colour temperature AWB estimates or the one set, in kelvin.
    pub colour_temperature: Option<u32>,
    /// The zones' mean R/G and B/G in thousandths, while AWB finds the scene outside the
    /// tuning's colour range and takes the curve's nearest point (`styx_aaa::AwbStatus::outside`).
    pub awb_outside_milli: Option<(u32, u32)>,
    /// The statistics are taken on one frame in this many and counted on one quad row in this
    /// many (`aaa_stats_every`, `aaa_stats_rows` as they run: fewer rows on a short frame).
    pub stats_every: u32,
    pub stats_rows: u32,
    /// The tuning in use, or why the loop does not run.
    pub tuning: Option<String>,
    pub reason: Option<String>,
}

/// The loop's controls and its requests, shared by the control socket, the capture thread and the
/// ISP thread. Every method holds the lock briefly and allocates nothing in steady state.
#[derive(Debug, Default)]
pub(crate) struct Link {
    state: Mutex<LinkState>,
}

#[derive(Debug, Default)]
struct LinkState {
    /// The application's controls, and a count that moves on each change.
    controls: Controls,
    seq: u64,
    /// Manual exposure (us) and gain (thousandths), each with the frame it applies from.
    manual_exposure: Option<(u32, u64)>,
    manual_gain: Option<(u32, u64)>,
    /// The loop's newest request not yet posted: exposure and gain.
    pending: Option<[Update; 2]>,
    /// The sensor's ranges and delays, from the capture thread.
    timing: Option<SensorTiming>,
    /// Whether the capture thread has posted the manual exposure and gain for their own frames
    /// (see [`Link::take_manual`]).
    posted_exposure: bool,
    posted_gain: bool,
    /// The newest frame a posted request of the loop is for (its values may be queued up to it).
    newest_target: u64,
}

/// The capture thread posts a manual value this many frames before its post frame (the frame
/// its exposure delay before it): the sensor's schedule holds it until then, and the margin
/// covers a capture thread that sees a frame late.
const MANUAL_MARGIN: u64 = 2;

fn lock(m: &Mutex<LinkState>) -> std::sync::MutexGuard<'_, LinkState> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a 3A control's value was refused.
fn bad(name: &str, what: &str) -> String {
    format!("{name} takes {what}")
}

fn milli(text: &str, name: &str) -> Result<u32, String> {
    text.parse::<u32>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| bad(name, "a positive whole number or auto"))
}

impl Link {
    /// Sets one of the loop's own controls ([`NAMES`]) from its text. From the next frame the loop
    /// processes.
    pub(crate) fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let mut s = lock(&self.state);
        let c = &mut s.controls;
        match name {
            "ae" => {
                c.ae_enable = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad(name, "on or off")),
                };
            }
            "ev" => {
                let ev = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && v.abs() <= 8.0)
                    .ok_or_else(|| bad(name, "stops from -8 to 8"))?;
                c.ev = ev;
            }
            "ae_exposure_max_us" => {
                c.exposure_max = match value {
                    "auto" => None,
                    v => Some(Duration::from_micros(u64::from(milli(v, name)?))),
                };
            }
            "ae_gain_max" => {
                c.gain_max = match value {
                    "auto" => None,
                    v => Some(f64::from(milli(v, name)?) / 1000.0),
                };
            }
            "awb" => {
                c.awb_enable = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad(name, "on or off")),
                };
                if c.awb_enable {
                    (c.colour_gains, c.colour_temperature) = (None, None);
                }
            }
            "red_gain" | "blue_gain" => match value {
                "auto" => {
                    c.colour_gains = None;
                    c.awb_enable = c.colour_temperature.is_none();
                }
                v => {
                    let g = f64::from(milli(v, name)?) / 1000.0;
                    let (r, b) = c.colour_gains.unwrap_or((1.0, 1.0));
                    c.colour_gains = Some(if name == "red_gain" { (g, b) } else { (r, g) });
                    c.awb_enable = false;
                }
            },
            "colour_temperature" => match value {
                "auto" => {
                    c.colour_temperature = None;
                    c.awb_enable = c.colour_gains.is_none();
                }
                v => {
                    let k = v
                        .parse::<u32>()
                        .ok()
                        .filter(|k| (1000..=20000).contains(k))
                        .ok_or_else(|| bad(name, "kelvin from 1000 to 20000, or auto"))?;
                    c.colour_temperature = Some(f64::from(k));
                    c.colour_gains = None;
                    c.awb_enable = false;
                }
            },
            "flicker" => {
                c.flicker = match value {
                    "off" => Flicker::Off,
                    "50" => Flicker::Mains50,
                    "60" => Flicker::Mains60,
                    "auto" => Flicker::Auto,
                    _ => return Err(bad(name, "off, 50, 60 or auto")),
                };
            }
            _ => return Err(format!("no control named {name}")),
        }
        s.seq += 1;
        Ok(())
    }

    /// A `set` of exposure or gain on a camera with the loop. A number is manual from `frame`
    /// (the caller posts it); `auto` gives it back to the loop. Returns whether there is a value
    /// to post.
    pub(crate) fn manual(
        &self,
        control: Control,
        value: ControlValue,
        frame: u64,
    ) -> Result<bool, String> {
        let mut s = lock(&self.state);
        let manual = match value {
            ControlValue::Manual(v) => Some(v),
            ControlValue::Auto => None,
            _ => return Err(format!("{} takes a number or auto", control.name())),
        };
        match control {
            // The loop takes it as fixed only for the frames it asks for from `frame` (see
            // `Aaa::step`): its requests for earlier frames stay its own.
            Control::Exposure => s.manual_exposure = manual.map(|v| (v, frame)),
            Control::Gain => s.manual_gain = manual.map(|v| (v, frame)),
            other => return Err(format!("{} is not the loop's", other.name())),
        }
        s.seq += 1;
        Ok(manual.is_some())
    }

    /// The value shown for one of the loop's controls ([`NAMES`]), and for exposure and gain
    /// whether they are left to the loop.
    pub(crate) fn shown(&self, name: &str) -> String {
        let s = lock(&self.state);
        let c = &s.controls;
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        let opt = |v: Option<u64>| v.map_or_else(|| "auto".to_string(), |v| v.to_string());
        match name {
            "ae" => on(c.ae_enable),
            "ev" => format!("{:.2}", c.ev),
            "ae_exposure_max_us" => opt(c.exposure_max.map(|d| d.as_micros() as u64)),
            "ae_gain_max" => opt(c.gain_max.map(|g| (g * 1000.0).round() as u64)),
            "awb" => on(c.awb_enable),
            "red_gain" => opt(c.colour_gains.map(|g| (g.0 * 1000.0).round() as u64)),
            "blue_gain" => opt(c.colour_gains.map(|g| (g.1 * 1000.0).round() as u64)),
            "colour_temperature" => opt(c.colour_temperature.map(|k| k.round() as u64)),
            "flicker" => flicker_name(c.flicker).to_string(),
            _ => "-".to_string(),
        }
    }

    /// Whether exposure (or gain) is left to the loop now.
    pub(crate) fn is_auto(&self, control: Control) -> bool {
        let s = lock(&self.state);
        match control {
            Control::Exposure => s.manual_exposure.is_none(),
            Control::Gain => s.manual_gain.is_none(),
            _ => false,
        }
    }

    /// The capture thread: the sensor's ranges and delays, when they change.
    pub(crate) fn set_timing(&self, timing: Option<SensorTiming>) {
        let mut s = lock(&self.state);
        if s.timing != timing {
            s.timing = timing;
        }
    }

    /// The capture thread: the loop's newest request, with a manual value in place of the loop's
    /// for every frame from the one it was set for. `None` when there is none.
    pub(crate) fn take_request(&self) -> Option<[Update; 2]> {
        let mut s = lock(&self.state);
        let mut updates = s.pending.take()?;
        for u in &mut updates {
            let manual = match u.control {
                Control::Exposure => s.manual_exposure,
                Control::Gain => s.manual_gain,
                _ => None,
            };
            if let Some((v, from)) = manual
                && u.frame >= from
            {
                u.value = ControlValue::Manual(v);
            }
            s.newest_target = s.newest_target.max(u.frame);
        }
        Some(updates)
    }

    /// The capture thread, at frame `at`: the manual exposure and gain to post for their own
    /// frames, once each, into `out` (one update per frame, cleared first). A manual value is
    /// posted for its frame F and for every frame after it a loop request already went out for,
    /// so no loop value queued before the set lands on or after F. It does not wait for a loop
    /// request that names F: the loop asks only on frames its ISP made, and when frames go
    /// without (a slow soft ISP, statistics every Nth frame) no request may name F, which put
    /// the manual value on F+1 on the board. Requests after this carry the manual value
    /// ([`Self::take_request`]).
    pub(crate) fn take_manual(&self, at: u64, out: &mut Vec<Update>) {
        out.clear();
        let mut s = lock(&self.state);
        let (exposure_delay, gain_delay) = s
            .timing
            .map_or((0, 0), |t| (u64::from(t.delays.0), u64::from(t.delays.1)));
        let newest = s.newest_target;
        let mut due = |manual: Option<(u32, u64)>, posted: &mut bool, delay: u64, control| {
            let Some((value, from)) = manual else { return };
            if *posted || at + delay + MANUAL_MARGIN < from {
                return;
            }
            *posted = true;
            for frame in from..=newest.max(from) {
                out.push(Update {
                    control,
                    value: ControlValue::Manual(value),
                    frame,
                });
            }
        };
        let LinkState {
            manual_exposure,
            manual_gain,
            posted_exposure,
            posted_gain,
            ..
        } = &mut *s;
        due(
            *manual_exposure,
            posted_exposure,
            exposure_delay,
            Control::Exposure,
        );
        due(*manual_gain, posted_gain, gain_delay, Control::Gain);
    }

    /// The ISP thread: the controls, copied into `into` when they changed since `seen`; the
    /// sensor's timing; and the manual exposure and gain with the frame each is manual from.
    fn read(&self, seen: &mut u64, into: &mut Controls) -> Read {
        let s = lock(&self.state);
        if s.seq != *seen {
            *seen = s.seq;
            into.clone_from(&s.controls);
        }
        Read {
            timing: s.timing,
            exposure: s.manual_exposure,
            gain: s.manual_gain,
        }
    }

    /// The ISP thread: leaves a request for the capture thread (replacing one not yet taken).
    fn request(&self, updates: [Update; 2]) {
        lock(&self.state).pending = Some(updates);
    }
}

/// What the ISP thread reads from the link each frame.
struct Read {
    timing: Option<SensorTiming>,
    exposure: Option<(u32, u64)>,
    gain: Option<(u32, u64)>,
}

fn flicker_name(f: Flicker) -> &'static str {
    match f {
        Flicker::Off => "off",
        Flicker::Mains50 => "50",
        Flicker::Mains60 => "60",
        Flicker::Period(_) => "period",
        Flicker::Auto => "auto",
    }
}

/// The camera configuration the loop runs against: the sensor's limits at the rate in force (the
/// rate is fixed: AE never lengthens a frame), its delays, and the frames it leaves unmetered.
fn camera_config(timing: &SensorTiming, fps_milli: u32) -> Option<CameraConfig> {
    if fps_milli == 0 {
        return None;
    }
    let frame = Duration::from_nanos(1_000_000_000_000 / u64::from(fps_milli));
    let (min_us, _) = timing.exposure_us;
    let max_us = timing.exposure_effective_max_us.max(min_us);
    let (gain_lo, gain_hi) = timing.gain_milli.unwrap_or((1000, 16_000));
    let (exposure, gain, frame_length) = timing.delays;
    let config = CameraConfig {
        exposure_limits: (
            Duration::from_micros(u64::from(min_us.max(1))),
            Duration::from_micros(u64::from(max_us.max(1))),
        ),
        exposure_margin: Duration::ZERO,
        frame_duration_limits: (frame, frame),
        analogue_gain_limits: (
            f64::from(gain_lo.max(1)) / 1000.0,
            f64::from(gain_hi.max(gain_lo.max(1))) / 1000.0,
        ),
        delays: ControlDelays {
            exposure,
            analogue_gain: gain,
            frame_duration: frame_length,
            issue_latency: ISSUE_LATENCY,
        },
        unsettled_frames: UNSETTLED,
        ..CameraConfig::default()
    };
    config.validate().ok().map(|()| config)
}

/// The loop on the ISP thread: the controller, the frame's statistics and what it was built for.
pub(crate) struct Aaa {
    tuning: Tuning,
    controller: Option<Controller>,
    /// What the controller was built for: the sensor's timing and the rate.
    built_for: Option<(SensorTiming, u32)>,
    /// The statistics of the frame being processed, filled by the ISP that made it.
    pub(crate) stats: Statistics,
    /// The soft ISP filled [`Self::stats`] for the frame just made.
    pub(crate) soft_counted: bool,
    /// Statistics on one frame in this many; frames made since the last ones.
    every: u32,
    since: u32,
    meta: FrameMetadata,
    seen: u64,
    /// The first frame id of this stream (the loop counts frames from it).
    first: Option<u64>,
    /// How many frames past the one processed the loop's requests are for (its last request's).
    lead: u64,
    status: AaaStatus,
}

impl Aaa {
    /// A loop with `tuning`, built once the capture thread has said the sensor's timing.
    /// `every` and `rows`: the statistics' sampling as it runs (see [`AaaStatus::stats_every`]).
    pub(crate) fn new(tuning: Tuning, described: String, every: u32, rows: u32) -> Self {
        Self {
            tuning,
            controller: None,
            built_for: None,
            stats: Statistics::new(
                styx_aaa::ZoneGrid::new(0, 0),
                None,
                styx_aaa::Histogram::default(),
                None,
            ),
            soft_counted: false,
            every: every.max(1),
            // The first frame made takes them.
            since: u32::MAX,
            meta: FrameMetadata::new(0, Duration::ZERO, 1.0, Duration::ZERO),
            seen: u64::MAX,
            first: None,
            lead: LEAD,
            status: AaaStatus {
                running: true,
                stats_every: every.max(1),
                stats_rows: rows,
                tuning: Some(described),
                ..AaaStatus::default()
            },
        }
    }

    /// The frame about to be made takes the statistics: one in [`Self::new`]'s `every`, counted
    /// from the last frame that took them (a frame that should have and was not made leaves the
    /// next one due).
    pub(crate) fn due(&self) -> bool {
        self.since.saturating_add(1) >= self.every
    }

    /// A frame was made: with the statistics (`counted`), or without.
    pub(crate) fn made(&mut self, counted: bool) {
        self.since = if counted {
            0
        } else {
            self.since.saturating_add(1)
        };
    }

    /// The status as the loop has it now.
    pub(crate) fn status(&self) -> &AaaStatus {
        &self.status
    }

    /// One frame's step: `raw` is the frame [`Self::stats`] were taken from (id and metadata). The
    /// controller runs, its request is left on `link`, and `settings` takes the settings for the
    /// next frame. Returns whether it ran.
    pub(crate) fn step(
        &mut self,
        link: &Link,
        id: u64,
        raw: &styx_controls::FrameMetadata,
        fps_milli: u32,
        settings: &mut IspSettings,
    ) -> bool {
        let read = link.read(&mut self.seen, &mut self.meta.controls);
        // A manual value is fixed for the loop once its requests are for frames from the one it
        // was set for; until then the loop's own values go on (the capture thread puts the manual
        // value in its requests for those frames, `Link::take_request`).
        let from = |m: Option<(u32, u64)>| m.filter(|&(_, f)| id + self.lead >= f).map(|(v, _)| v);
        self.meta.controls.exposure =
            from(read.exposure).map(|us| Duration::from_micros(u64::from(us)));
        self.meta.controls.analogue_gain = from(read.gain).map(|m| f64::from(m) / 1000.0);
        let timing = read.timing;
        let fps = if raw.achieved_fps_milli != 0 {
            raw.achieved_fps_milli
        } else {
            fps_milli
        };
        let Some(timing) = timing else {
            self.note("waiting for the sensor's ranges");
            return false;
        };
        if self.built_for != Some((timing, fps)) || self.controller.is_none() {
            // A new rate or mode: the loop starts again from what it had settled on.
            let warm = self.controller.as_ref().and_then(Controller::warm_state);
            let Some(config) = camera_config(&timing, fps) else {
                self.note("the sensor's ranges give no exposure for the rate");
                return false;
            };
            match Controller::new(&self.tuning, config) {
                Ok(mut c) => {
                    c.set_warm_start(warm);
                    self.controller = Some(c);
                    self.built_for = Some((timing, fps));
                    self.first = None;
                }
                Err(e) => {
                    self.note(&format!("the tuning does not run: {e}"));
                    self.controller = None;
                    return false;
                }
            }
        }
        let Some(controller) = self.controller.as_mut() else {
            return false;
        };
        let first = *self.first.get_or_insert(id);
        if id < first {
            // The stream started again: count from here.
            self.first = Some(id);
        }
        let first = self.first.unwrap_or(first);
        self.meta.frame = id - first;
        self.meta.exposure = Duration::from_micros(u64::from(raw.exposure_us));
        self.meta.analogue_gain = f64::from(raw.analogue_gain_milli.max(1)) / 1000.0;
        self.meta.digital_gain = 1.0;
        self.meta.frame_duration = Duration::from_nanos(1_000_000_000_000 / u64::from(fps.max(1)));
        let step = match controller.process(&self.stats, &self.meta) {
            Ok(step) => step,
            Err(e) => {
                let why = format!("the loop stopped on a frame: {e}");
                self.note(&why);
                return false;
            }
        };
        if let Some(u) = step.updates {
            let mut exposure = u.exposure;
            let mut gain = u.gain;
            exposure.frame += first;
            gain.frame += first;
            self.lead = exposure.frame.saturating_sub(id);
            link.request([exposure, gain]);
        }
        settings.clone_from(step.isp);
        let c = &self.meta.controls;
        let p = step.params;
        let s = &mut self.status;
        s.reason = None;
        s.ae = c.ae_enable && !(c.exposure.is_some() && c.analogue_gain.is_some());
        s.awb = c.awb_enable;
        s.flicker = flicker_name(c.flicker);
        s.ae_settled = s.ae.then_some(p.ae.locked);
        let g = p.colour_gains[1].max(1e-6);
        s.wb_milli = Some((
            (p.colour_gains[0] / g * 1000.0).round() as u32,
            (p.colour_gains[2] / g * 1000.0).round() as u32,
        ));
        s.colour_temperature = Some(p.colour_temperature.round() as u32);
        s.awb_outside_milli = p
            .awb
            .outside
            .map(|(r, b)| ((r * 1000.0).round() as u32, (b * 1000.0).round() as u32));
        true
    }

    fn note(&mut self, why: &str) {
        if self.status.reason.as_deref() != Some(why) {
            self.status.reason = Some(why.to_string());
        }
    }
}

/// Whether `next` moved from `applied` by more than [`SETTLE`]: the soft ISP applies settings only
/// then.
pub(crate) fn moved(applied: &IspSettings, next: &IspSettings) -> bool {
    let rel = |a: f64, b: f64| (a - b).abs() > SETTLE * a.abs().max(b.abs()).max(1e-9);
    let abs = |a: f64, b: f64| (a - b).abs() > SETTLE;
    let slices =
        |a: &[f64], b: &[f64]| a.len() != b.len() || a.iter().zip(b).any(|(x, y)| abs(*x, *y));
    if applied.wb.iter().zip(&next.wb).any(|(a, b)| rel(*a, *b))
        || rel(applied.digital_gain, next.digital_gain)
        || abs(applied.black_level, next.black_level)
        || applied.black_channels != next.black_channels
        || applied.ccm.iter().zip(&next.ccm).any(|(a, b)| abs(*a, *b))
        || applied.denoise != next.denoise
        || applied.sharpen != next.sharpen
    {
        return true;
    }
    let gamma = match (&applied.gamma, &next.gamma) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            let (a, b) = (a.points(), b.points());
            a.len() != b.len() || a.iter().zip(b).any(|(p, q)| abs(p.0, q.0) || abs(p.1, q.1))
        }
        _ => true,
    };
    let shading = match (&applied.lens_shading, &next.lens_shading) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            (a.width, a.height) != (b.width, b.height)
                || slices(&a.r, &b.r)
                || slices(&a.g, &b.g)
                || slices(&a.b, &b.b)
        }
        _ => true,
    };
    gamma || shading
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board's case (3A proof 1, M.2): the loop asks only on the frames its ISP made, every
    /// other one, for `id + 4`; a manual exposure is set for frame F, which no request names. The
    /// camera's schedule is modelled as the native one: values queued by frame, the one in force
    /// on a frame the newest queued at or before it. Frame F has the manual value, every frame
    /// after it too, and none before it.
    #[test]
    fn the_statistics_are_due_on_one_frame_in_every_and_a_missed_one_leaves_the_next_due() {
        let mut aaa = Aaa::new(Tuning::default(), "defaults".into(), 4, 4);
        assert_eq!((aaa.status().stats_every, aaa.status().stats_rows), (4, 4));
        // The first frame made takes them; then one in 4.
        let mut taken = Vec::new();
        for frame in 0..12 {
            let due = aaa.due();
            if due {
                taken.push(frame);
            }
            aaa.made(due);
        }
        assert_eq!(taken, [0, 4, 8]);
        // Due on frame 12 but not counted (the frame was made without them): 13 is due.
        assert!(aaa.due());
        aaa.made(false);
        assert!(aaa.due());
        aaa.made(true);
        assert!(!aaa.due());
        // Every frame.
        let mut every = Aaa::new(Tuning::default(), "defaults".into(), 1, 2);
        for _ in 0..3 {
            assert!(every.due());
            every.made(true);
        }
    }

    #[test]
    fn a_manual_exposure_lands_on_its_frame_when_the_loop_skips_frames() {
        let link = Link::default();
        link.set_timing(Some(SensorTiming {
            exposure_us: (10, 10_000),
            exposure_effective_max_us: 10_000,
            gain_milli: Some((1000, 16_000)),
            frame_rate_milli: None,
            delays: (2, 2, 2),
        }));
        let from = 40;
        let mut queued = std::collections::BTreeMap::new();
        let mut manual: Vec<Update> = Vec::new();
        for at in 1..80u64 {
            if at == 10 {
                link.manual(Control::Exposure, ControlValue::Manual(4000), from)
                    .unwrap();
            }
            // The ISP made only odd frames: only they ask.
            if at % 2 == 1 {
                link.request([
                    update(Control::Exposure, 5000 + at as u32, at + 4),
                    update(Control::Gain, 1000, at + 4),
                ]);
            }
            if let Some(posted) = link.take_request() {
                for u in posted {
                    if u.control == Control::Exposure {
                        queued.insert(u.frame, u.value);
                    }
                }
            }
            link.take_manual(at, &mut manual);
            for u in &manual {
                queued.insert(u.frame, u.value);
            }
        }
        let in_force = |frame: u64| *queued.range(..=frame).next_back().unwrap().1;
        for frame in 20..from {
            assert_ne!(in_force(frame), ControlValue::Manual(4000), "frame {frame}");
        }
        for frame in from..80 {
            assert_eq!(in_force(frame), ControlValue::Manual(4000), "frame {frame}");
        }
    }

    fn update(control: Control, value: u32, frame: u64) -> Update {
        Update {
            control,
            value: ControlValue::Manual(value),
            frame,
        }
    }

    #[test]
    fn a_manual_value_replaces_the_loops_from_its_frame_on_and_not_before() {
        let link = Link::default();
        assert_eq!(
            link.manual(Control::Exposure, ControlValue::Manual(5000), 100),
            Ok(true)
        );
        for (frame, want) in [(99, 9000), (100, 5000), (140, 5000)] {
            link.request([
                update(Control::Exposure, 9000, frame),
                update(Control::Gain, 2000, frame),
            ]);
            let got = link.take_request().expect("a request");
            assert_eq!(got[0].value, ControlValue::Manual(want), "frame {frame}");
            assert_eq!(
                got[1].value,
                ControlValue::Manual(2000),
                "gain stays the loop's"
            );
        }
        assert_eq!(link.take_request(), None, "taken once");
        // Back to the loop: its value goes out again.
        assert_eq!(
            link.manual(Control::Exposure, ControlValue::Auto, 150),
            Ok(false)
        );
        link.request([
            update(Control::Exposure, 9000, 160),
            update(Control::Gain, 2000, 160),
        ]);
        assert_eq!(
            link.take_request().expect("a request")[0].value,
            ControlValue::Manual(9000)
        );
    }

    #[test]
    fn the_loops_controls_take_their_values_and_refuse_others_by_name() {
        let link = Link::default();
        for (name, value) in [
            ("ae", "off"),
            ("ev", "-1.5"),
            ("ae_exposure_max_us", "8000"),
            ("ae_gain_max", "4000"),
            ("red_gain", "1800"),
            ("blue_gain", "1500"),
            ("flicker", "50"),
        ] {
            link.set(name, value).expect(name);
            assert_eq!(
                link.shown(name),
                if name == "ev" { "-1.50" } else { value },
                "{name}"
            );
        }
        assert_eq!(link.shown("awb"), "off", "manual gains turn AWB off");
        link.set("colour_temperature", "auto").expect("auto");
        link.set("red_gain", "auto").expect("auto");
        assert_eq!(link.shown("awb"), "on", "both back to auto: AWB on");
        link.set("colour_temperature", "4000").expect("ct");
        assert_eq!(
            (link.shown("awb").as_str(), link.shown("red_gain").as_str()),
            ("off", "auto")
        );
        for (name, value) in [
            ("ae", "maybe"),
            ("ev", "9"),
            ("ev", "nan"),
            ("ae_gain_max", "0"),
            ("flicker", "55"),
            ("colour_temperature", "100"),
            ("nope", "1"),
        ] {
            assert!(link.set(name, value).is_err(), "{name} {value}");
        }
    }

    fn timing() -> SensorTiming {
        SensorTiming {
            exposure_us: (10, 66_000),
            exposure_effective_max_us: 8_000,
            gain_milli: Some((1000, 15_500)),
            frame_rate_milli: Some((15_000, 120_000)),
            delays: (2, 2, 2),
        }
    }

    #[test]
    fn the_loop_runs_within_the_rate_in_force_and_the_sensors_delays() {
        let c = camera_config(&timing(), 120_000).expect("a config");
        assert_eq!(c.exposure_limits.1, Duration::from_micros(8_000));
        assert_eq!(c.frame_duration_limits.0, c.frame_duration_limits.1);
        assert_eq!(c.frame_duration_limits.0, Duration::from_nanos(8_333_333));
        assert_eq!((c.delays.exposure, c.delays.analogue_gain), (2, 2));
        assert_eq!(c.analogue_gain_limits, (1.0, 15.5));
        assert!(camera_config(&timing(), 0).is_none());
    }

    #[test]
    fn a_dark_frame_raises_exposure_and_the_request_names_a_frame_after_the_delays() {
        let link = Link::default();
        link.set_timing(Some(timing()));
        let mut aaa = Aaa::new(Tuning::default(), "defaults".into(), 1, 2);
        let mut settings = IspSettings::neutral(0.0);
        // A dark, grey frame: every zone at 2% of full scale.
        let mut acc = styx_aaa::StatsAccumulator::new(4, 4, 64, 0.99);
        for zy in 0..4 {
            for zx in 0..4 {
                acc.add(zx, zy, 0.02, 0.02, 0.02);
            }
        }
        aaa.stats = acc.finish();
        let raw = styx_controls::FrameMetadata {
            exposure_us: 1000,
            analogue_gain_milli: 1000,
            ..styx_controls::FrameMetadata::default()
        };
        let mut asked = None;
        for id in 1000..1012 {
            assert!(aaa.step(&link, id, &raw, 120_000, &mut settings));
            if let Some(r) = link.take_request() {
                asked = Some(r);
                assert!(
                    r[0].frame >= id + 2,
                    "lands after the delays: {} at {id}",
                    r[0].frame
                );
            }
        }
        let r = asked.expect("AE asked for something");
        let ControlValue::Manual(us) = r[0].value else {
            panic!("a value")
        };
        let ControlValue::Manual(gain) = r[1].value else {
            panic!("a value")
        };
        assert!(us > 1000 || gain > 1000, "brighter: {us} us, {gain} gain");
        assert!(us <= 8000, "within the frame: {us}");
        // AE runs (a scene this dark pins it at its limits, which counts as settled).
        assert!(aaa.status().ae && aaa.status().ae_settled.is_some());
    }
}
