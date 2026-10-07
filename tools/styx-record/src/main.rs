//! `styx-record`: grey test recordings from a Styx camera service or a camera opened
//! directly. `styx-record --help`; docs/recording.md.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use std::io::BufRead;

    use styx_record::{Config, StillTrigger, record, signal};

    let cfg = match Config::parse(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let stop = signal::install();
    let keys = cfg
        .stills
        .is_some_and(|s| s.trigger == StillTrigger::Key)
        .then(|| {
            eprintln!("press Enter for each still (Ctrl-C to stop)");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    if line.is_err() || tx.send(()).is_err() {
                        break;
                    }
                }
            });
            rx
        });
    match record::run(&cfg, stop, keys) {
        Ok(summary) => {
            println!("{}", summary.settings_path.display());
            if let Some(raw) = &summary.raw_path {
                println!("{}", raw.display());
            }
            for still in &summary.stills {
                println!("{}", still.display());
            }
            println!("{}", summary.csv_path.display());
            if summary.errors.iter().any(|e| e.starts_with("writing")) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("styx-record: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("styx-record runs on Linux only");
    ExitCode::FAILURE
}
