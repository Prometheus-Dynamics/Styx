//! `styx-record`: grey test recordings from a Styx camera service or a camera opened
//! directly. `styx-record --help`; docs/recording.md.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use std::io::BufRead;

    use styx_record::{Config, StillTrigger, record, signal};

    if std::env::args().nth(1).as_deref() == Some("convert") {
        return convert(std::env::args().skip(2));
    }
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

/// `styx-record convert`: a video file to an MCAP recording (feature `convert`).
#[cfg(target_os = "linux")]
fn convert(args: impl Iterator<Item = String>) -> ExitCode {
    #[cfg(feature = "convert")]
    {
        let a = match styx_record::convert::parse(args) {
            Ok(a) => a,
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        };
        match styx_record::convert::run(&a) {
            Ok(done) => {
                eprintln!(
                    "{} frames, {}x{}: {}",
                    done.frames,
                    done.width,
                    done.height,
                    a.output.display()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("styx-record convert: {err}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "convert"))]
    {
        let _ = args;
        eprintln!("styx-record was built without `convert` (feature `convert`)");
        ExitCode::from(2)
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("styx-record runs on Linux only");
    ExitCode::FAILURE
}
