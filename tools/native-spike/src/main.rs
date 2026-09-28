//! First on-device spike of the Styx native stack: raw OV9782 frames from `rp1-cfe` on a
//! Raspberry Pi CM5, with the sensor driven from userspace over I²C and the Styx sensor bridge
//! standing in for the sensor driver. See `kernel-modules/styx-sensor-bridge/spike/README.md`.
//!
//! Flow: find the bridge → subscribe to its events → power up → chip id → init → mode (sensor
//! in standby) → bridge format and timing → media links, receiver pads, video format → start
//! the acknowledgement thread → buffers → STREAMON (returns once the sensor started) →
//! baseline, frame rate and exposure checks → save a frame → STREAMOFF → power down.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use styx_sensor::SensorDescription;

mod args;
mod checks;
mod dry_run;
mod experiments;
mod frames;
mod pipeline;
mod regbus;
mod rig;

/// Errors are messages: this is a diagnostic tool.
pub(crate) type Result<T> = std::result::Result<T, String>;

/// Adds context to any displayable error.
pub(crate) trait ResultExt<T> {
    fn ctx(self, what: &str) -> Result<T>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for std::result::Result<T, E> {
    fn ctx(self, what: &str) -> Result<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

fn start_time() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Prints a line with the time since start.
macro_rules! log {
    ($($t:tt)*) => {
        println!("[{:8.3}] {}", $crate::start_time().elapsed().as_secs_f64(), format_args!($($t)*))
    };
}
pub(crate) use log;

static INTERRUPT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// True once SIGINT or SIGTERM arrived (a second one terminates the process).
pub(crate) fn interrupted() -> bool {
    INTERRUPT.get().is_some_and(|f| f.load(Ordering::Relaxed))
}

fn install_signal_handlers() -> std::io::Result<()> {
    let flag = Arc::clone(INTERRUPT.get_or_init(|| Arc::new(AtomicBool::new(false))));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        // The first signal sets the flag (the run stops and cleans up); a second one exits.
        signal_hook::flag::register_conditional_shutdown(sig, 1, Arc::clone(&flag))?;
        signal_hook::flag::register(sig, Arc::clone(&flag))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    start_time();
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(args::Command::Run(a)) => a,
        Ok(args::Command::Help) => {
            print!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("native-spike: {e}");
            return ExitCode::from(2);
        }
    };
    let desc = match SensorDescription::from_file(&args.description) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("native-spike: {}: {e}", args.description.display());
            return ExitCode::from(2);
        }
    };
    if args.dry_run {
        let f = dry_run::run(&args, &desc);
        return if f.problems.is_empty() {
            log!("dry run: no problems found");
            ExitCode::SUCCESS
        } else {
            log!("dry run: {} problem(s)", f.problems.len());
            ExitCode::FAILURE
        };
    }
    if let Err(e) = install_signal_handlers() {
        eprintln!("native-spike: signal handlers: {e}");
        return ExitCode::FAILURE;
    }
    match run(&args, desc) {
        Ok(()) => {
            log!("done");
            ExitCode::SUCCESS
        }
        Err(e) => {
            log!("FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &args::Args, desc: Arc<SensorDescription>) -> Result<()> {
    let bridges = styx_kernel::bus::find_bridges().ctx("find bridges")?;
    let loc = bridges
        .iter()
        .find(|b| b.sensor_name == desc.sensor.name)
        .or(bridges.first())
        .ok_or("no styx-sensor-bridge bound (run up.sh first)")?;
    log!(
        "bridge {} for \"{}\"",
        loc.subdev.display(),
        loc.sensor_name
    );
    let link = desc
        .mode(&args.mode)
        .and_then(|m| desc.format_for(m, &args.format))
        .ctx("mode/format")?
        .link_frequency;
    let problems = checks::standby_problems(&desc, &args.mode, &args.format);
    if !problems.is_empty() {
        return Err(format!(
            "description would leave standby early: {problems:?}"
        ));
    }
    // From here on, dropping the rig (error, interrupt or success) cleans everything up.
    let mut rig = rig::Rig::open(
        Arc::clone(&desc),
        loc,
        args.i2c_bus,
        args.power_settle,
        args.row_step,
        &args.mode,
        &args.format,
    )?;
    rig.bring_up(&args.mode, &args.format)?;
    rig.configure_bridge(link, args.ack_timeout)?;
    rig.configure_graph()?;
    rig.start(args.buffers)?;

    let mut failures = Vec::new();
    experiments::baseline(&mut rig, args.frames)?;
    if args.fps_check {
        for (fps, predicted, measured) in experiments::fps_check(&mut rig, &args.fps)? {
            if (measured - predicted).abs() / predicted > 0.02 {
                failures.push(format!(
                    "{fps} fps: measured {measured:.3}, predicted {predicted:.3}"
                ));
            }
        }
    }
    if args.exposure_check {
        for (predicted, observed) in experiments::exposure_check(&mut rig, args.exposure_fps)? {
            if observed != Some(predicted) {
                failures.push(format!(
                    "exposure: predicted frame {predicted}, observed {observed:?}"
                ));
            }
        }
    }
    experiments::save_frame(&mut rig, &args.out_dir)?;
    drop(rig);
    if failures.is_empty() {
        Ok(())
    } else {
        for f in &failures {
            log!("check failed: {f}");
        }
        Err(format!(
            "{} check(s) did not match the model",
            failures.len()
        ))
    }
}
