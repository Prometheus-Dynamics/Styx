//! `--dry-run`: read-only checks that can run before the bridge is set up. Opens device nodes
//! read-only and only queries; never claims the I²C address, powers anything or sets a link.

use std::path::Path;

use styx_kernel::FourCc;
use styx_kernel::bus::find_bridges;
use styx_kernel::media::{self, MediaDevice};
use styx_kernel::subdev::{Subdev, Which};
use styx_kernel::v4l2::{BufType, Format, PixFormat, VideoDevice};
use styx_sensor::SensorDescription;

use crate::args::Args;
use crate::checks::{rate_plans, standby_problems};
use crate::frames::choose_format;
use crate::log;
use crate::pipeline;

/// Outcome: problems that would stop the real run.
#[derive(Debug, Default)]
pub struct Findings {
    /// Blocking problems.
    pub problems: Vec<String>,
}

impl Findings {
    fn problem(&mut self, p: impl Into<String>) {
        let p = p.into();
        log!("PROBLEM: {p}");
        self.problems.push(p);
    }
}

/// Runs the read-only checks.
pub fn run(args: &Args, desc: &SensorDescription) -> Findings {
    let mut f = Findings::default();
    let code = check_description(args, desc, &mut f);
    let bridges = find_bridges().unwrap_or_default();
    match bridges.first() {
        Some(b) => {
            log!(
                "bridge: {} sensor \"{}\" on I2C bus {:?} address {:?}, clock {} Hz",
                b.subdev.display(),
                b.sensor_name,
                b.i2c_bus,
                b.i2c_address,
                b.clock_frequency
            );
            if let Ok(sd) = Subdev::open_read_only(&b.subdev)
                && let Ok(fmt) = sd.format(0, Which::Active)
            {
                log!("bridge pad 0: {}x{} {:?}", fmt.width, fmt.height, fmt.code);
            }
        }
        None => log!("bridge: none bound (expected before up.sh; the real run needs it)"),
    }
    check_i2c(args, desc, bridges.first().and_then(|b| b.i2c_bus));
    check_overlay();
    check_graph(code, bridges.first().map(|b| b.subdev.as_path()), &mut f);
    f
}

fn check_description(
    args: &Args,
    desc: &SensorDescription,
    f: &mut Findings,
) -> Option<(u32, u32, u32)> {
    let chip = desc.sensor.chip_id.as_ref().map_or("none".to_owned(), |c| {
        let values: Vec<String> = c.values.iter().map(|v| format!("{v:#06x}")).collect();
        format!(
            "{:#06x} ({} bytes) in [{}]",
            c.address,
            c.bytes,
            values.join(", ")
        )
    });
    log!(
        "description: {} ({}), chip id {chip}, I2C address {}, {}-bit registers",
        desc.sensor.name,
        args.description.display(),
        desc.sensor
            .i2c_address
            .map_or("none".to_owned(), |a| format!("{a:#04x}")),
        desc.sensor.address_bits
    );
    let problems = standby_problems(desc, &args.mode, &args.format);
    if problems.is_empty() {
        log!(
            "standby: power_up, init, format and mode registers never start the stream (lanes stay in LP-11 until stream_on)"
        );
    }
    for p in problems {
        f.problem(format!("standby: {p}"));
    }
    let (Ok(mode), Ok(timing)) = (desc.mode(&args.mode), desc.timing(&args.mode, &args.format))
    else {
        f.problem(format!("no mode {} / format {}", args.mode, args.format));
        return None;
    };
    let format = desc.format_for(mode, &args.format).ok()?;
    let (lo, hi) = timing.fps_range();
    log!(
        "mode {} {}: {:?} code {:#06x}, pixel rate {}, link {:?} Hz, line length {}, {lo:.2}..{hi:.2} fps, default {:.2} fps",
        mode.name,
        args.format,
        (mode.size.width, mode.size.height),
        format.code.0,
        timing.pixel_rate,
        format.link_frequency,
        timing.line_length(),
        timing.fps(timing.frame_length_default())
    );
    for p in rate_plans(&timing, &args.fps) {
        log!(
            "  {} fps -> frame length {} (vblank {}), {:.3} fps{}",
            p.requested,
            p.frame_length,
            p.vblank,
            p.fps,
            if p.clamped { " CLAMPED" } else { "" }
        );
        if p.clamped {
            f.problem(format!("{} fps is outside the mode's range", p.requested));
        }
    }
    let fl = timing.frame_length_for_fps(args.exposure_fps).lines;
    let lim = timing.exposure_limits(fl);
    log!(
        "  exposure at {} fps: {:.3}..{:.3} ms",
        args.exposure_fps,
        lim.min.as_secs_f64() * 1e3,
        lim.max.as_secs_f64() * 1e3
    );
    Some((format.code.0, mode.size.width, mode.size.height))
}

fn check_i2c(args: &Args, desc: &SensorDescription, bridge_bus: Option<u32>) {
    let bus = args.i2c_bus.or(bridge_bus).unwrap_or(10);
    let addr = desc.sensor.i2c_address.unwrap_or(0);
    let dev = format!("/dev/i2c-{bus}");
    let client = format!("/sys/bus/i2c/devices/{bus}-{addr:04x}");
    let owner = std::fs::read_link(format!("{client}/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    log!(
        "I2C: {dev} {}; client {bus}-{addr:04x} {}",
        if Path::new(&dev).exists() {
            "present"
        } else {
            "MISSING"
        },
        match (Path::new(&client).exists(), owner) {
            (false, _) => "absent: the address is free for i2c-dev".to_owned(),
            (true, Some(d)) => format!("bound to {d}: the address is busy (up.sh frees it)"),
            (true, None) =>
                "exists with no driver: the address is still busy for i2c-dev".to_owned(),
        }
    );
}

fn check_overlay() {
    let dir = Path::new("/sys/kernel/config/device-tree/overlays/styx-sensor-bridge");
    let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
    log!(
        "overlay: {}",
        if dir.exists() {
            format!("present, status {}", status.trim())
        } else {
            "not applied".to_owned()
        }
    );
}

fn check_graph(code: Option<(u32, u32, u32)>, bridge: Option<&Path>, f: &mut Findings) {
    let mut found = false;
    for p in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open_read_only(&p) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        let Ok(path) = pipeline::find_raw_path(&topo) else {
            continue;
        };
        found = true;
        log!(
            "media {}: \"{}\":{} -> \"{}\":{} ... :{} -> \"{}\" ({})",
            p.display(),
            path.sensor.1,
            path.sensor_pad,
            pipeline::RECEIVER,
            path.receiver_sink,
            path.receiver_source,
            pipeline::RAW_NODE,
            path.node_path
                .as_ref()
                .map_or("no node".into(), |n| n.display().to_string())
        );
        match (bridge, &path.sensor_path) {
            (Some(b), Some(s)) if b == s => log!("media: the bridge is the receiver's sensor"),
            (Some(_), _) => f.problem(
                "a bridge is bound but the receiver's sensor is something else (rebind rp1-cfe)",
            ),
            (None, _) => log!(
                "media: sensor is \"{}\" (the bridge replaces it after up.sh)",
                path.sensor.1
            ),
        }
        let plan = pipeline::link_plan(&topo, &path);
        if plan.is_empty() {
            log!("media: links already set for raw capture");
        }
        for c in &plan {
            log!("media: would {}", pipeline::describe(&topo, c));
        }
        if let (Some((code, w, h)), Some(node)) = (code, &path.node_path) {
            check_video(node, code, w, h, f);
        }
    }
    if !found {
        f.problem("no media graph with csi2 -> rp1-cfe-csi2_ch0 (is rp1-cfe bound?)");
    }
}

fn check_video(node: &Path, code: u32, width: u32, height: u32, f: &mut Findings) {
    let video = match VideoDevice::open_read_only(node) {
        Ok(v) => v,
        Err(e) => return f.problem(format!("open {}: {e}", node.display())),
    };
    let offered: Vec<FourCc> = match video.formats_for_mbus_code(BufType::VideoCapture, code) {
        Ok(v) => v.iter().map(|d| d.fourcc).collect(),
        Err(e) => return f.problem(format!("enumerate formats: {e}")),
    };
    let Some(fourcc) = choose_format(&offered) else {
        return f.problem(format!(
            "{} offers nothing for bus code {code:#06x}",
            node.display()
        ));
    };
    log!(
        "video {}: offers {offered:?} for {code:#06x}; would use {fourcc}",
        node.display()
    );
    let want = Format::Single(PixFormat {
        width,
        height,
        fourcc,
        field: 1,
        ..Default::default()
    });
    match video.try_format(BufType::VideoCapture, &want) {
        Ok(Format::Single(p)) => log!(
            "video: TRY_FMT {}x{} {} -> stride {}, size {}",
            p.width,
            p.height,
            p.fourcc,
            p.bytes_per_line,
            p.size_image
        ),
        Ok(other) => log!("video: TRY_FMT gave {other:?}"),
        Err(e) => log!("video: TRY_FMT not possible read-only ({e})"),
    }
}
