//! `styx-camerad [CONFIG.json]`: serve every attached camera (see the `styx_camerad` crate). The
//! configuration is optional (`/etc/styx/camerad.json` when it exists); SIGINT or SIGTERM stops
//! the cameras, removes their sockets and empties the endpoint list.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

fn main() {
    let path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let default = std::path::Path::new("/etc/styx/camerad.json");
    let config = match path.as_deref().or(default.exists().then_some(default)) {
        Some(p) => match std::fs::read(p)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<styx_camerad::Config>(&bytes).map_err(|e| e.to_string())
            }) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("styx-camerad: {}: {e}", p.display());
                std::process::exit(2);
            }
        },
        None => styx_camerad::Config::default(),
    };
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(
                sig,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }
    if let Err(e) = styx_camerad::run(config, styx_camerad::endpoints::Nowhere, &STOP) {
        eprintln!("styx-camerad: {e}");
        std::process::exit(1);
    }
}
