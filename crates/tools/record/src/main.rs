//! `styx-record`: a recording from the command line.
//!
//! - `styx-record info <file.styxrec>`: the header, the channels with their clocks and sample
//!   counts, the frame count, and how the file ended.
//! - `styx-record export <file.styxrec> <out.mcap> [--uncompressed] [--chunk-size BYTES]`:
//!   MCAP (lz4 chunks by default), then what MCAP could not carry.
//! - `styx-record import <in.mcap> <out.styxrec>`: `styxrec`, then what was not carried.
//!
//! Exit status 0 on success, 1 on a refused or unreadable file, 2 on bad arguments.

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::process::ExitCode;

use styx_record::{
    ExportOptions, Item, McapCompression, RecordError, Replay, SourceClock, export_mcap,
    import_mcap,
};

/// `println!` that returns the write error (a closed pipe ends the command quietly).
macro_rules! out {
    ($($arg:tt)*) => {
        writeln!(std::io::stdout().lock(), $($arg)*)?
    };
}

const USAGE: &str = "usage:
  styx-record info <file.styxrec>
  styx-record export <file.styxrec> <out.mcap> [--uncompressed] [--chunk-size BYTES]
  styx-record import <in.mcap> <out.styxrec>";

fn clock(c: SourceClock) -> &'static str {
    match c {
        SourceClock::Boottime => "boottime",
        _ => "monotonic",
    }
}

/// One line of a losses table.
fn row(label: &str, n: u64) -> std::io::Result<()> {
    out!("  {label:<32} {n}");
    Ok(())
}

fn info(path: &str) -> Result<(), RecordError> {
    let mut replay = Replay::open(path)?;
    let h = replay.header().clone();
    out!("file      {path}");
    out!("version   {}", replay.version());
    out!("device    {}", h.device);
    if !h.keys.is_empty() {
        out!("keys      {}", h.keys.join(", "));
    }
    out!("backend   {}", h.backend);
    out!(
        "format    {} {}x{}",
        h.format.code,
        h.format.resolution.width,
        h.format.resolution.height
    );
    match h.interval {
        Some(i) => out!("interval  {}/{} s", i.numerator, i.denominator),
        None => out!("interval  unknown"),
    }
    out!("payload   {} bytes", h.payload);
    let mut per_channel: Vec<u64> = Vec::new();
    let (mut frames, mut first, mut last) = (0u64, u64::MAX, 0u64);
    while let Some(item) = replay.next_item()? {
        let ts = match item {
            Item::Frame(f) => {
                frames += 1;
                f.timestamp_ns()
            }
            Item::Sample(s) => {
                let n = usize::from(s.channel.get());
                if per_channel.len() <= n {
                    per_channel.resize(n + 1, 0);
                }
                per_channel[n] += 1;
                s.timestamp_ns
            }
        };
        first = first.min(ts);
        last = last.max(ts);
    }
    out!("frames    {frames}");
    if frames + per_channel.iter().sum::<u64>() > 0 {
        out!(
            "span      {:.3} s ({first} .. {last} ns, CLOCK_MONOTONIC)",
            (last - first) as f64 / 1e9
        );
    }
    out!("channels  {}", replay.channels().len());
    for c in replay.channels() {
        let n = per_channel
            .get(usize::from(c.id.get()))
            .copied()
            .unwrap_or(0);
        let s = &c.spec;
        out!(
            "  {:>3} {}  encoding={:?} schema={:?} clock={} samples={n} declared mono={} boot={}",
            c.id.get(),
            s.name,
            s.encoding,
            s.schema_name,
            clock(s.clock),
            c.declared.monotonic_ns,
            c.declared.boottime_ns,
        );
        for (k, v) in &s.metadata {
            out!("        {k}={v}");
        }
    }
    out!("late      {} samples", replay.late_samples());
    let end = if replay.truncated() {
        "truncated: cut inside a record (the replay ends at its last whole record)"
    } else if replay.end_marker() {
        "clean: finished with the end marker"
    } else {
        "not finished: no end marker, but no record was cut"
    };
    out!("end       {end}");
    Ok(())
}

fn export(input: &str, output: &str, options: ExportOptions) -> Result<(), RecordError> {
    let mut replay = Replay::open(input)?;
    let out = BufWriter::with_capacity(1 << 20, File::create(output)?);
    let (stats, mut out) = export_mcap(&mut replay, out, options)?;
    out.flush()?;
    out!(
        "wrote {output}: {} frames, {} channels, {} samples ({} late), {}",
        stats.frames,
        stats.channels,
        stats.samples,
        stats.late_samples,
        if stats.truncated {
            "source truncated"
        } else {
            "source complete"
        }
    );
    let l = stats.losses;
    out!("not carried into MCAP (frames):");
    row("control values", l.control_values)?;
    row("row/plane padding", l.padding)?;
    row("ids above u32", l.ids)?;
    row("sensor timestamps", l.sensor_timestamps)?;
    row("zero lux or colour temperature", l.zero_lux_or_temperature)?;
    Ok(())
}

fn import(input: &str, output: &str) -> Result<(), RecordError> {
    let mut source = BufReader::with_capacity(1 << 20, File::open(input)?);
    let out = BufWriter::with_capacity(1 << 20, File::create(output)?);
    let stats = import_mcap(&mut source, out)?;
    out!(
        "wrote {output}: {} frames, {} channels, {} samples, {} clock records, {}",
        stats.frames,
        stats.channels,
        stats.samples,
        stats.clock_records,
        if stats.truncated {
            "source truncated"
        } else {
            "source complete"
        }
    );
    let l = stats.losses;
    out!("not carried into styxrec:");
    row("attachments", l.attachments)?;
    row("other metadata records", l.metadata_records)?;
    row("data message sequences", l.data_sequences)?;
    row("data publish times (differed)", l.data_publish_times)?;
    row("pyramid messages", l.pyramid_messages)?;
    row("other /styx/ messages", l.other_styx_messages)?;
    row("samples over 1 MiB", l.samples_too_large)?;
    row("boottime as monotonic (no clock)", l.boottime_assumed_equal)?;
    Ok(())
}

fn export_options(rest: &[String]) -> Option<ExportOptions> {
    let mut options = ExportOptions::default();
    let mut args = rest.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--uncompressed" => options.compression = McapCompression::None,
            "--chunk-size" => options.chunk_size = args.next()?.parse().ok()?,
            _ => return None,
        }
    }
    Some(options)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["info", path] => info(path),
        ["import", input, output] => import(input, output),
        ["export", input, output, ..] => match export_options(&args[3..]) {
            Some(options) => export(input, output, options),
            None => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The reader went away (`| head`): not a failure.
        Err(RecordError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("styx-record: {e}");
            ExitCode::FAILURE
        }
    }
}
