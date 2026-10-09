//! The recording loop: frames from the source, their Y planes to the disk writer, the sidecar
//! rows and the settings file; stops on the duration, the frame count, the stills, Ctrl-C, or
//! the source closing, and always finalises the files.
//!
//! The loop only takes frames: each frame's Y plane is copied into a writer buffer and the
//! frame (the camera's buffer) is released at once, so the camera never waits for the disk.
//! The camera's controls are read on their own thread ([`ControlPoller`]): a read can take a
//! frame period per control (libcamera), and doing it here held frames back long enough for
//! the capture queue to overflow.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx::prelude::*;

use crate::config::{Config, Mode, SourceKind, StillTrigger};
use crate::json::unix_ns_now;
use crate::luma::luma_into;
use crate::settings::Settings;
use crate::sidecar::{GapSource, GapTracker, Row, clock_name};
use crate::source::{Control, ControlReader, DirectSource, ServiceSource, Source};
use crate::writer::{Job, Layout, Writer};

/// How long the loop waits for a frame before checking for Ctrl-C and triggers.
const POLL: Duration = Duration::from_millis(100);
const STATUS_EVERY: Duration = Duration::from_secs(5);

/// What a recording produced.
#[derive(Debug)]
pub struct Summary {
    pub settings_path: PathBuf,
    pub raw_path: Option<PathBuf>,
    pub csv_path: PathBuf,
    pub frames: u64,
    pub stills: Vec<PathBuf>,
    pub dropped: u64,
    pub writer_dropped: u64,
    pub stop_reason: String,
    pub errors: Vec<String>,
    pub restarts_caused: Option<u64>,
}

/// Open the configured source and record.
pub fn run(cfg: &Config, stop: &AtomicBool, keys: Option<Receiver<()>>) -> Result<Summary, String> {
    let source: Box<dyn Source> = match cfg.source {
        SourceKind::Service => Box::new(ServiceSource::open(cfg)?),
        SourceKind::Direct => Box::new(DirectSource::open(cfg)?),
    };
    record(cfg, source, stop, keys)
}

/// A control read: when it finished, and what it read.
type ControlRead = (Instant, Result<Vec<Control>, String>);

/// Reads the camera's controls on its own thread: at once, then every `period`.
pub struct ControlPoller {
    reads: Receiver<ControlRead>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ControlPoller {
    pub fn start(mut reader: Box<dyn ControlReader>, period: Duration) -> Self {
        let (tx, reads) = channel();
        let (stop, stop_rx) = channel::<()>();
        let thread = std::thread::Builder::new()
            .name("styx-record-controls".into())
            .spawn(move || {
                loop {
                    let read = reader.read();
                    if tx.send((Instant::now(), read)).is_err() {
                        break;
                    }
                    match stop_rx.recv_timeout(period) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })
            .ok();
        Self {
            reads,
            stop: Some(stop),
            thread,
        }
    }

    /// A read that has finished, if any (never waits).
    pub fn try_read(&self) -> Option<ControlRead> {
        self.reads.try_recv().ok()
    }

    /// Stop polling (after a read in progress) and return the reads not taken yet.
    pub fn stop(mut self) -> Vec<ControlRead> {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.reads.try_iter().collect()
    }
}

struct Loop<'a> {
    cfg: &'a Config,
    started: Instant,
    settings: Settings,
    gaps: GapTracker,
    writer: Option<Writer>,
    layout: Option<Layout>,
    settings_path: PathBuf,
    /// Rows recorded (frames or stills).
    index: u64,
    warned_disk: bool,
    poller: Option<ControlPoller>,
    /// A control read has arrived.
    controls_read: bool,
}

/// Record from `source` (already open) until a stop condition; the files are finalised
/// whatever stopped it.
pub fn record(
    cfg: &Config,
    mut source: Box<dyn Source>,
    stop: &AtomicBool,
    keys: Option<Receiver<()>>,
) -> Result<Summary, String> {
    let mode = match (cfg.stills, cfg.mode) {
        (Some(_), _) => "stills",
        (None, Mode::Every) => "every",
        (None, Mode::Latest) => "latest",
    };
    let started = Instant::now();
    let settings = Settings::new(source.info().clone(), mode, unix_ns_now());
    let poller = ControlPoller::start(source.control_reader(), cfg.settings_poll);
    let interval_ns = source
        .info()
        .fps
        .filter(|f| *f > 0.0)
        .map(|f| (1e9 / f64::from(f)) as u64);
    let mut l = Loop {
        cfg,
        started,
        settings,
        gaps: GapTracker::new(interval_ns),
        writer: None,
        layout: None,
        settings_path: cfg.out.join(format!("{}.json", cfg.name)),
        index: 0,
        warned_disk: false,
        poller: Some(poller),
        controls_read: false,
    };
    if !cfg.quiet {
        let info = source.info();
        eprintln!(
            "styx-record: {} ({}), {mode}, into {}",
            info.camera,
            info.kind,
            cfg.out.display()
        );
        if let Some(plan) = &info.plan {
            eprint!("{plan}");
        }
    }
    let deadline = cfg.seconds.map(|s| started + Duration::from_secs_f64(s));
    let mut next_timer = match cfg.stills.map(|s| s.trigger) {
        Some(StillTrigger::Every(period)) => Some((started + period, period)),
        _ => None,
    };
    // A still is due: take the first frame captured after this (CLOCK_MONOTONIC ns).
    let mut pending: Option<u64> = None;
    let mut last_status = Instant::now();
    let mut result = Ok(());
    let reason = loop {
        if stop.load(Ordering::SeqCst) {
            break "interrupted (Ctrl-C / SIGTERM)".to_string();
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break "duration reached".into();
        }
        if cfg.frames.is_some_and(|n| l.index >= n) {
            break "frame count reached".into();
        }
        if cfg.stills.is_some_and(|s| l.index >= u64::from(s.count)) {
            break "stills taken".into();
        }
        if l.writer.as_ref().is_some_and(Writer::failed) {
            break "write error".into();
        }
        if let Some(keys) = &keys {
            loop {
                match keys.try_recv() {
                    Ok(()) => {
                        pending.get_or_insert_with(now_mono);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if pending.is_none() {
                            // stdin closed: no more stills will be asked for.
                            pending = Some(u64::MAX);
                        }
                        break;
                    }
                }
            }
            if pending == Some(u64::MAX) {
                break "stdin closed".into();
            }
        }
        if let Some((due, period)) = &mut next_timer
            && Instant::now() >= *due
        {
            pending.get_or_insert_with(now_mono);
            *due += *period;
        }
        while let Some(read) = l.poller.as_ref().and_then(ControlPoller::try_read) {
            l.controls(read);
        }
        if !cfg.quiet && last_status.elapsed() >= STATUS_EVERY {
            last_status = Instant::now();
            l.status();
        }
        let frame = match source.next(POLL) {
            crate::source::Next::Frame(frame) => frame,
            crate::source::Next::Empty => continue,
            crate::source::Next::Closed(why) => {
                l.settings.outcome.errors.push(why.clone());
                break why;
            }
        };
        if cfg.stills.is_some() {
            let Some(due) = pending else { continue };
            // Only a frame exposed after the trigger (when the clocks can be compared).
            if frame
                .meta()
                .sensor_monotonic_ns()
                .is_some_and(|ns| ns < due)
            {
                continue;
            }
            pending = None;
        }
        if let Err(err) = l.take(frame) {
            l.settings.outcome.errors.push(err.clone());
            result = Err(err);
            break "error".into();
        }
    };
    let summary = l.finish(reason, source.restarts_caused());
    result.map(|()| summary)
}

fn now_mono() -> u64 {
    TimestampClock::Monotonic.now_ns().unwrap_or(0)
}

impl Loop<'_> {
    /// A control read back: the first is the initial state (its error is reported), later ones
    /// record changes.
    fn controls(&mut self, (at, read): ControlRead) {
        let first = !self.controls_read;
        self.controls_read = true;
        match read {
            Ok(controls) => {
                let at = at.saturating_duration_since(self.started).as_nanos() as u64;
                self.settings.controls(controls, self.index, at);
            }
            Err(err) if first => self
                .settings
                .outcome
                .errors
                .push(format!("controls: {err}")),
            Err(_) => {}
        }
    }

    /// Record `frame` (a video frame or a still).
    fn take(&mut self, frame: FrameLease) -> Result<(), String> {
        let res = frame.meta().format.resolution;
        let (w, h) = (res.width.get(), res.height.get());
        if self.writer.is_none() {
            self.open(w, h, frame.meta().format.code)?;
        }
        let layout = self.layout.as_ref().expect("opened above");
        if (w, h) != (layout.width, layout.height) {
            return Err(format!(
                "the frame size changed from {}x{} to {w}x{h} (the capture was reconfigured)",
                layout.width, layout.height
            ));
        }
        let stem = layout.still_stem(self.index);
        let mut row = Row::of(&frame, self.index);
        let writer = self.writer.as_mut().expect("opened above");
        let Some(mut luma) = writer.buffer() else {
            writer.dropped += 1;
            self.disk_behind();
            return Ok(());
        };
        luma_into(&frame, &mut luma)?;
        let code = frame.meta().format.code;
        drop(frame);
        let at = self.started.elapsed().as_nanos() as u64;
        let stills = self.cfg.stills.is_some();
        let job = if stills {
            row.gap_source = Some(GapSource::Still);
            Job::Still {
                meta: still_meta(&self.settings, &row, &stem, w, h, code),
                luma,
                row: row.clone(),
                stem,
            }
        } else {
            self.gaps.fill(&mut row);
            Job::Frame {
                luma,
                row: row.clone(),
            }
        };
        if !writer.submit(job) {
            self.disk_behind();
            return Ok(());
        }
        if !stills {
            self.gaps.commit(&row);
        }
        self.settings.frame(&row, at);
        let o = &mut self.settings.outcome;
        o.first_timestamp_ns.get_or_insert(row.timestamp_ns);
        o.last_timestamp_ns = Some(row.timestamp_ns);
        o.clock = o.clock.or(row.clock);
        self.index += 1;
        if stills && !self.cfg.quiet {
            eprintln!(
                "still {} of {}",
                self.index,
                self.cfg.stills.map_or(0, |s| s.count)
            );
        }
        Ok(())
    }

    /// Create the files once the frame size is known, and a first settings file.
    fn open(&mut self, width: u32, height: u32, code: FourCc) -> Result<(), String> {
        let layout = Layout {
            dir: self.cfg.out.clone(),
            name: self.cfg.name.clone(),
            width,
            height,
            force: self.cfg.force,
        };
        if !self.cfg.force && self.settings_path.exists() {
            return Err(format!(
                "{} exists (use --force)",
                self.settings_path.display()
            ));
        }
        let writer = Writer::start(layout.clone(), self.cfg.stills.is_some(), self.cfg.queue)
            .map_err(|e| format!("creating the recording: {e}"))?;
        self.settings.width = Some(width);
        self.settings.height = Some(height);
        self.settings.frame_format = Some(code.to_string());
        self.settings.outcome.csv_file = layout.csv_name();
        if self.cfg.stills.is_none() {
            self.settings.outcome.raw_file = Some(layout.raw_name());
        }
        self.settings.outcome.stop_reason = "recording".into();
        self.layout = Some(layout);
        self.writer = Some(writer);
        self.write_settings()
    }

    fn disk_behind(&mut self) {
        if !self.warned_disk && !self.cfg.quiet {
            eprintln!(
                "styx-record: the disk is not keeping up: its queue of {} frames is full, \
                 dropping frames",
                self.cfg.queue
            );
        }
        self.warned_disk = true;
    }

    fn status(&self) {
        let (queued, disk_dropped) = self
            .writer
            .as_ref()
            .map_or((0, 0), |w| (w.queued(), w.dropped));
        eprintln!(
            "{:.0} s: {} {}, {} dropped in {} gaps, disk queue {queued}/{}, {disk_dropped} \
             dropped by the disk",
            self.started.elapsed().as_secs_f64(),
            self.index,
            if self.cfg.stills.is_some() {
                "stills"
            } else {
                "frames"
            },
            self.gaps.total,
            self.gaps.gaps,
            self.cfg.queue
        );
    }

    /// Atomically (re)write the settings file.
    fn write_settings(&self) -> Result<(), String> {
        let tmp = self.settings_path.with_extension("json.tmp");
        std::fs::write(&tmp, self.settings.to_json())
            .and_then(|()| std::fs::rename(&tmp, &self.settings_path))
            .map_err(|e| format!("{}: {e}", self.settings_path.display()))
    }

    fn finish(mut self, reason: String, restarts_caused: Option<u64>) -> Summary {
        if let Some(poller) = self.poller.take() {
            for read in poller.stop() {
                self.controls(read);
            }
        }
        let written = self.writer.take().map(|w| {
            let (dropped, max_queued) = (w.dropped, w.max_queued);
            (w.finish(), dropped, max_queued)
        });
        let o = &mut self.settings.outcome;
        o.stop_reason = reason.clone();
        o.dropped = self.gaps.total;
        o.gaps = self.gaps.gaps;
        o.restarts_caused = restarts_caused;
        if let Some((written, dropped, max_queued)) = &written {
            o.frames = if self.cfg.stills.is_some() {
                written.stills.len() as u64
            } else {
                written.frames
            };
            o.raw_gray_bytes = written.raw_bytes;
            o.raw_gray_sha256 = written.raw_sha256.clone();
            o.stills = written.stills.clone();
            o.writer_dropped = *dropped;
            o.writer_max_queued = *max_queued;
            if let Some(err) = &written.error {
                o.errors.push(format!("writing: {err}"));
            }
        }
        // The files are whole and consistent unless writing failed.
        o.complete = !o.errors.iter().any(|e| e.starts_with("writing"));
        self.settings.end_unix_ns = Some(unix_ns_now());
        if let Some(r) = restarts_caused.filter(|r| *r > 0)
            && !self.cfg.quiet
        {
            eprintln!(
                "styx-record: warning: the camera service restarted its capture {r} time(s) \
                 while the recorder joined (its other clients saw a gap)"
            );
        }
        if self.layout.is_some()
            && let Err(err) = self.write_settings()
        {
            self.settings.outcome.errors.push(err);
        }
        let o = &self.settings.outcome;
        let dir = &self.cfg.out;
        let summary = Summary {
            settings_path: self.settings_path.clone(),
            raw_path: o.raw_file.as_ref().map(|f| dir.join(f)),
            csv_path: dir.join(&o.csv_file),
            frames: o.frames,
            stills: o.stills.iter().map(|f| dir.join(f)).collect(),
            dropped: o.dropped,
            writer_dropped: o.writer_dropped,
            stop_reason: reason,
            errors: o.errors.clone(),
            restarts_caused,
        };
        if !self.cfg.quiet {
            eprintln!(
                "styx-record: {} ({}): {} {}, {} camera frames dropped in {} gaps, {} dropped by \
                 the disk{}",
                summary.stop_reason,
                clock_name(o.clock),
                summary.frames,
                if self.cfg.stills.is_some() {
                    "stills"
                } else {
                    "frames"
                },
                o.dropped,
                o.gaps,
                o.writer_dropped,
                if o.writer_dropped > 0 && self.cfg.mode == Mode::Every {
                    " (the disk did not keep up)"
                } else {
                    ""
                }
            );
        }
        summary
    }
}

/// A still's `.txt`: the Eidos live-capture metadata keys (`width`, `height`, `format`,
/// `source_fourcc`, `frame_index`, `capture_index`, `raw`, `pgm`) plus the recorder's.
fn still_meta(settings: &Settings, row: &Row, stem: &str, w: u32, h: u32, code: FourCc) -> String {
    let opt = |v: Option<String>| v.unwrap_or_default();
    format!(
        "width={w}\nheight={h}\nformat=R8\nsource_fourcc={code}\nframe_index={}\n\
         capture_index={}\nsequence={}\ntimestamp_ns={}\nclock={}\nraw={stem}.gray.raw\n\
         pgm={stem}.pgm\ncamera={}\nsource_stream={}\nexposure_us={}\nanalogue_gain={}\n\
         styx_commit={}\n",
        row.sequence.map_or(row.index, u64::from),
        row.index,
        opt(row.sequence.map(|s| s.to_string())),
        row.timestamp_ns,
        clock_name(row.clock),
        settings.source.camera,
        settings.source.source_stream.as_deref().unwrap_or_default(),
        opt(row.exposure_us.map(|v| v.to_string())),
        opt(row.analogue_gain.map(|v| v.to_string())),
        settings.commit,
    )
}
