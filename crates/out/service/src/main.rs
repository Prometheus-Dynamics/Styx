//! `styx-service CONFIG`: runs the camera service with the config file at CONFIG until it is
//! killed (the systemd unit stops it with a signal, and the process exits).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    // Serve until the process is stopped. Nothing else runs on this thread.
    loop {
        std::thread::park();
    }
}
