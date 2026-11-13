//! The command line: `styx-measure read ...` and `styx-measure status ...`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::clock::{cpu_of, memory_of, monotonic_ns, own_cpu_ns};
use crate::control::{CameraLine, ask, camera_for_socket};
use crate::isp::{IspRunner, SoftIspArg};
use crate::report::{CopySite, Process, Report, ServiceCamera};
use crate::session::{Session, SessionError, Stop};

pub const USAGE: &str = "\
usage:
  styx-measure read --socket <camera frame socket> (--frames N | --seconds S)
                    [--control <service control socket>] [--service-pid PID]
                    [--asked-fps FPS] [--soft-isp <FOURCC>[:WxH]] [--report <file.json>]
  styx-measure status --control <service control socket>

read attaches to a camera's frame socket as a reader in this process and measures what it takes
(see the crate's docs/board-run.md). status prints the service's `list` answer as it is.

--asked-fps: an override for the rate the camera was asked for (for example 120.00). Without it
the requested rate is read from the service's status (`requested=`, the config's fps_milli or the
last frame-rate control accepted).
Report: requested, achieved (backend), achieved (metadata), measured (service and reader) and the
service's reason when they differ, read from the status and the frame metadata.
--soft-isp: run the software ISP on each raw frame into an output picture of FOURCC (NV12, I420,
RGB3, GREY) at full size, or at WxH (full or half of the raw size).
";

/// What `read` was asked to do.
#[derive(Debug, Clone)]
pub struct ReadArgs {
    pub socket: PathBuf,
    pub control: Option<PathBuf>,
    pub frames: Option<u64>,
    pub seconds: Option<f64>,
    pub soft_isp: Option<SoftIspArg>,
    pub report: Option<PathBuf>,
    pub service_pid: Option<u32>,
    pub asked_fps_milli: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Read(ReadArgs),
    Status(PathBuf),
    Help,
}

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    match verb.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "read" => parse_read(rest).map(Command::Read),
        "status" => {
            let mut control = None;
            let mut it = rest.iter();
            while let Some(flag) = it.next() {
                match flag.as_str() {
                    "--control" => control = Some(value(&mut it, flag)?),
                    other => return Err(format!("status: unknown argument {other}")),
                }
            }
            let control = control.ok_or("status: --control <socket> is required")?;
            Ok(Command::Status(control))
        }
        other => Err(format!("unknown command {other}")),
    }
}

fn value(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<PathBuf, String> {
    it.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn text(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_read(rest: &[String]) -> Result<ReadArgs, String> {
    let mut socket = None;
    let mut control = None;
    let mut frames = None;
    let mut seconds = None;
    let mut soft_isp = None;
    let mut report = None;
    let mut service_pid = None;
    let mut asked_fps_milli = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--socket" => socket = Some(value(&mut it, flag)?),
            "--control" => control = Some(value(&mut it, flag)?),
            "--frames" => {
                let n: u64 = text(&mut it, flag)?
                    .parse()
                    .map_err(|_| "--frames: a count".to_string())?;
                if n == 0 {
                    return Err("--frames must be above zero".into());
                }
                frames = Some(n);
            }
            "--seconds" => {
                let s: f64 = text(&mut it, flag)?
                    .parse()
                    .map_err(|_| "--seconds: a number".to_string())?;
                if !(s.is_finite() && s > 0.0) {
                    return Err("--seconds must be above zero".into());
                }
                seconds = Some(s);
            }
            "--soft-isp" => soft_isp = Some(text(&mut it, flag)?.parse::<SoftIspArg>()?),
            "--report" => report = Some(value(&mut it, flag)?),
            "--service-pid" => {
                service_pid = Some(
                    text(&mut it, flag)?
                        .parse::<u32>()
                        .map_err(|_| "--service-pid: a process id".to_string())?,
                )
            }
            "--asked-fps" => asked_fps_milli = Some(parse_fps_milli(&text(&mut it, flag)?)?),
            other => return Err(format!("read: unknown argument {other}")),
        }
    }
    let socket = socket.ok_or("read: --socket <camera frame socket> is required")?;
    if frames.is_none() && seconds.is_none() {
        return Err("read: give --frames N or --seconds S".into());
    }
    Ok(ReadArgs {
        socket,
        control,
        frames,
        seconds,
        soft_isp,
        report,
        service_pid,
        asked_fps_milli,
    })
}

/// A decimal rate (`120.00`) as thousandths, exactly. More than three decimals is an error.
pub fn parse_fps_milli(text: &str) -> Result<u32, String> {
    let bad = || format!("--asked-fps {text}: expected a rate such as 120.00");
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() && frac.is_empty() {
        return Err(bad());
    }
    if frac.len() > 3
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || !whole.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let whole: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| bad())?
    };
    let frac: u32 = format!("{frac:0<3}").parse().map_err(|_| bad())?;
    whole
        .checked_mul(1000)
        .and_then(|m| m.checked_add(frac))
        .filter(|&m| m > 0)
        .ok_or_else(bad)
}

/// Runs the command. Output goes to `out` (the table, the status lines); errors to `err`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match parse(args) {
        Ok(Command::Help) => {
            let _ = write!(out, "{USAGE}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            let _ = writeln!(err, "styx-measure: {why}\n{USAGE}");
            ExitCode::from(2)
        }
        Ok(Command::Status(control)) => match ask(&control, "list") {
            Ok(lines) => {
                for line in &lines {
                    let _ = writeln!(out, "{line}");
                }
                if lines.last().is_some_and(|l| l == "ok") {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                let _ = writeln!(err, "styx-measure: control {}: {e}", control.display());
                ExitCode::FAILURE
            }
        },
        Ok(Command::Read(read)) => match read_run(&read) {
            Ok(report) => {
                let _ = write!(out, "{}", report.table());
                if let Some(path) = &read.report {
                    let json = report.to_json().render();
                    if let Err(e) = std::fs::write(path, format!("{json}\n")) {
                        let _ = writeln!(err, "styx-measure: writing {}: {e}", path.display());
                        return ExitCode::FAILURE;
                    }
                    let _ = writeln!(out, "report: {}", path.display());
                }
                ExitCode::SUCCESS
            }
            Err(why) => {
                let _ = writeln!(err, "styx-measure: {why}");
                ExitCode::FAILURE
            }
        },
    }
}

/// The camera's status line now, if a control socket was given and the service lists the camera.
/// The control socket's answer for one camera: the control socket's name, the camera's line, and
/// the service's copy counters.
type StatusOf = (Option<String>, Option<CameraLine>, Vec<CopySite>);

fn status_of(control: Option<&Path>, socket: &Path) -> Result<StatusOf, String> {
    let Some(control) = control else {
        return Ok((None, None, Vec::new()));
    };
    let lines = ask(control, "list").map_err(|e| format!("control {}: {e}", control.display()))?;
    if lines.last().map(String::as_str) != Some("ok") {
        return Err(format!(
            "control {}: {}",
            control.display(),
            lines.last().cloned().unwrap_or_default()
        ));
    }
    let cam = camera_for_socket(&lines, socket);
    let copies = copy_sites(&lines);
    Ok((Some(control.display().to_string()), cam, copies))
}

/// The service's copy-site lines (`copy site=.. copies=.. bytes=..`) of a `list` answer.
fn copy_sites(lines: &[String]) -> Vec<CopySite> {
    lines
        .iter()
        .filter_map(|line| {
            // `copy copies=N bytes=N site=<free text>`: the site is everything after ` site=`.
            let rest = line.strip_prefix("copy ")?;
            let (head, site) = rest.split_once(" site=")?;
            let (mut copies, mut bytes) = (None, None);
            for word in head.split_ascii_whitespace() {
                match word.split_once('=')? {
                    ("copies", v) => copies = v.parse().ok(),
                    ("bytes", v) => bytes = v.parse().ok(),
                    _ => {}
                }
            }
            Some(CopySite {
                site: site.to_string(),
                copies: copies?,
                bytes: bytes?,
            })
        })
        .collect()
}

fn service_camera(cam: &CameraLine) -> ServiceCamera {
    // `none` and `unknown` are words, not numbers: they read as absent.
    let reason = cam
        .get("rate_reason")
        .filter(|r| *r != "-")
        .map(|r| r.replace('_', " "));
    ServiceCamera {
        id: cam.id.clone(),
        state: cam.get("state").unwrap_or("-").to_string(),
        frames: cam.number("frames"),
        skipped: cam.number("skipped"),
        last_id: cam.number("last_id"),
        publish: cam.get("publish").unwrap_or("-").to_string(),
        readers: cam.number("readers"),
        published: cam.number("published"),
        publish_dropped: cam.number("publish_dropped"),
        health: cam.get("health").unwrap_or("-").to_string(),
        requested_milli: cam.number("requested"),
        achieved_milli: cam.number("achieved"),
        achieved_meta_milli: cam.number("achieved_meta"),
        measured_milli: cam.number("measured"),
        rate_clamped: cam.get("rate_clamped").map(|v| v == "true"),
        rate_reason: reason,
        keep: cam.number("keep"),
        buffers: cam.number("buffers"),
        hardware_reserve: cam.number("reserve"),
        reclaimed: cam.number("reclaimed"),
        raw: cam.raw.clone(),
    }
}

/// Does the run's stop condition hold after one more frame?
fn wanted(frames: Option<u64>) -> impl FnMut(&crate::stats::FrameStats) -> bool {
    move |s| frames.is_some_and(|n| s.delivered >= n)
}

/// The read command, end to end.
pub fn read_run(args: &ReadArgs) -> Result<Report, String> {
    let (control, start_line, _) = status_of(args.control.as_deref(), &args.socket)?;
    let service_start = start_line.as_ref().map(service_camera);

    let own_cpu0 = own_cpu_ns();
    let own_mem0 = memory_of("self");
    let svc_cpu0 = match args.service_pid {
        Some(pid) => {
            Some(cpu_of(pid).ok_or_else(|| format!("service pid {pid}: no /proc/{pid}/stat"))?)
        }
        None => None,
    };
    let svc_mem0 = args.service_pid.and_then(|pid| memory_of(&pid.to_string()));

    let isp = args.soft_isp.clone().map(IspRunner::new);
    let mut session =
        Session::connect(&args.socket, isp).map_err(|e: SessionError| e.to_string())?;
    let reader0 = session.reader_start();

    let started = monotonic_ns();
    let time = args.seconds.map(Duration::from_secs_f64);
    let stop = session
        .run(time, wanted(args.frames))
        .map_err(|e| e.to_string())?;
    let wall_s = monotonic_ns().saturating_sub(started) as f64 / 1e9;

    let own_cpu1 = own_cpu_ns();
    let own_mem1 = memory_of("self");
    let svc_cpu1 = args.service_pid.and_then(cpu_of);
    let svc_mem1 = args.service_pid.and_then(|pid| memory_of(&pid.to_string()));
    let (_, end_line, service_copies) = status_of(args.control.as_deref(), &args.socket)?;
    let service_end = end_line.as_ref().map(service_camera);
    let reader1 = session.reader_stats();

    let copies = styx_frame::copy_counters()
        .iter()
        .copied()
        .chain(styx_v4l2::copy_counters().iter().copied())
        .map(|c| CopySite {
            site: c.site().to_string(),
            copies: c.copies(),
            bytes: c.bytes(),
        })
        .collect();

    let stop_name = match stop {
        Stop::Frames => "frames",
        Stop::Time => "time",
        Stop::Idle => "idle: no frame for 30 s",
    };
    Ok(Report {
        socket: args.socket.display().to_string(),
        control,
        service_pid: args.service_pid,
        stop: stop_name.to_string(),
        wall_s,
        asked_frames: args.frames,
        asked_seconds: args.seconds,
        asked_fps_milli: args.asked_fps_milli,
        requested_source: match (
            args.asked_fps_milli,
            service_end.as_ref().or(service_start.as_ref()),
        ) {
            (Some(_), _) => "--asked-fps",
            (None, Some(c)) if c.requested_milli.is_some() => "status",
            _ => "none",
        },
        achieved_metadata_milli: session
            .metadata
            .map(|m| m.achieved_fps_milli)
            .filter(|m| *m != 0),
        service_copies,
        frames: session.stats.clone(),
        skipped: reader1.skipped.saturating_sub(reader0.skipped),
        superseded: reader1.superseded.saturating_sub(reader0.superseded),
        limited: reader1.limited.saturating_sub(reader0.limited),
        malformed: reader1.malformed.saturating_sub(reader0.malformed),
        dma_sync_errors: reader1
            .dma_sync_errors
            .saturating_sub(reader0.dma_sync_errors),
        held_at_end: reader1.held,
        service_start,
        service_end,
        copies,
        own: Process {
            cpu_ns: Some(own_cpu1.saturating_sub(own_cpu0)),
            memory_start: own_mem0,
            memory_end: own_mem1,
        },
        service: Process {
            cpu_ns: svc_cpu0.zip(svc_cpu1).map(|(a, b)| b.saturating_sub(a)),
            memory_start: svc_mem0,
            memory_end: svc_mem1,
        },
        isp: session.isp_stats(),
        last_metadata: session.metadata.map(|m| format!("{m:?}")),
    })
}
