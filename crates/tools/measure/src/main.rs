//! `styx-measure read` and `styx-measure status`. See `styx-measure --help`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    styx_measure::cli::run(&args, &mut stdout.lock(), &mut stderr.lock())
}
