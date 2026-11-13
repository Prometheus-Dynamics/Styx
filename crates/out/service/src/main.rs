//! `styx-service CONFIG`: runs the camera service with the config file at CONFIG until it gets
//! SIGTERM or SIGINT (the systemd unit stops it with SIGTERM). Then every camera is stopped through
//! the Lemnos contract (its sensor and receiver), its sockets are removed, and the process exits.
//! A SIGKILL cannot be caught; the next start stops a sensor a previous owner left started.

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use lemnos_linux_sys::poll::{POLLIN, poll_one};
use lemnos_linux_sys::signal::{SIGINT, SIGTERM, SignalFd};
use styx_native_run::SystemClock;
use styx_select::real::{Libcamera, Probe};
use styx_service::{Daemon, Deps, Discard, Listers, Nothing, Service};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: styx-service CONFIG");
        return ExitCode::from(2);
    };
    let config = match styx_service::Config::load(&PathBuf::from(path)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("styx-service: {e}");
            return ExitCode::from(2);
        }
    };
    // Before any thread starts, so every thread inherits the blocked signals. They then reach
    // this descriptor and do not end the process, which would leave the cameras streaming.
    let signals = match SignalFd::new(&[SIGTERM, SIGINT]) {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("styx-service: signals: {e}");
            return ExitCode::FAILURE;
        }
    };
    let native: Arc<dyn styx_select::listing::NativeLister + Send + Sync> =
        match &config.lemnos_socket {
            Some(socket) => Arc::new(Daemon {
                socket: socket.clone(),
            }),
            None => Arc::new(Nothing),
        };
    let deps = Deps {
        clock: Arc::new(SystemClock::default()),
        listers: Listers {
            native,
            libcamera: Arc::new(Libcamera),
            v4l2: Arc::new(Probe),
        },
        outlets: Arc::new(Discard),
    };
    let service = match Service::start(config, deps) {
        Ok(service) => service,
        Err(e) => {
            eprintln!("styx-service: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "styx-service: control socket {}",
        service.control_socket().display()
    );
    let waited = wait_for_signal(&signals);
    match &waited {
        Ok(signal) => eprintln!("styx-service: {}: stopping the cameras", name(*signal)),
        Err(e) => eprintln!("styx-service: waiting for a signal: {e}: stopping the cameras"),
    }
    // Each camera's thread ends, and its session stops the sensor and receiver before it does.
    service.shutdown();
    if waited.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Blocks until one of the signals is pending, and returns its number.
fn wait_for_signal(signals: &SignalFd) -> std::io::Result<i32> {
    loop {
        if let Some(signal) = signals.read()? {
            return Ok(signal);
        }
        poll_one(signals.as_fd(), POLLIN, None)?;
    }
}

fn name(signal: i32) -> &'static str {
    match signal {
        SIGTERM => "SIGTERM",
        SIGINT => "SIGINT",
        _ => "signal",
    }
}
