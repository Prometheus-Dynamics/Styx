//! Prints every media device's topology, every video node's capabilities, formats, sizes,
//! intervals and controls, and every subdevice's formats and controls.
//!
//! Read-only: every node is opened `O_RDONLY` and only queried.
//!
//! ```text
//! cargo run -p styx-kernel --example probe [-- --no-controls]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use styx_kernel::media::{EntityFunction, MediaDevice, PadFlags, Topology};
use styx_kernel::subdev::{Subdev, Which};
use styx_kernel::v4l2::{
    BufType, ControlInfo, ControlType, ControlValue, ControlWhich, Controls, Format,
    FrameIntervals, FrameSizes, MenuValue, SelectionTarget, VideoDevice,
};
use styx_kernel::{FourCc, Fraction, media, subdev, v4l2};

/// What the media graphs say about a device node.
struct NodeInfo {
    media: PathBuf,
    entity: String,
    function: EntityFunction,
    pads: Vec<(u32, PadFlags)>,
}

fn main() {
    let show_controls = !std::env::args().any(|a| a == "--no-controls");
    let mut nodes: HashMap<PathBuf, NodeInfo> = HashMap::new();

    for path in media::list_media_devices() {
        match MediaDevice::open_read_only(&path) {
            Ok(dev) => probe_media(&dev, &mut nodes),
            Err(e) => println!("== media {}: {e}", path.display()),
        }
    }
    for path in v4l2::list_video_nodes() {
        probe_video(&path, nodes.get(&path), show_controls);
    }
    for path in subdev::list_subdev_nodes() {
        probe_subdev(&path, nodes.get(&path), show_controls);
    }
}

fn probe_media(dev: &MediaDevice, nodes: &mut HashMap<PathBuf, NodeInfo>) {
    let path = dev.path().display();
    match dev.device_info() {
        Ok(i) => println!(
            "== media {path}: driver {:?} model {:?} bus {:?} serial {:?} hw {:#x}",
            i.driver, i.model, i.bus_info, i.serial, i.hw_revision
        ),
        Err(e) => {
            println!("== media {path}: {e}");
            return;
        }
    }
    let topo = match dev.topology() {
        Ok(t) => t,
        Err(e) => {
            println!("  topology: {e}");
            return;
        }
    };
    print_topology(&topo);
    for e in &topo.entities {
        if let Some(node) = topo.devnode_path(e.id) {
            nodes.insert(
                node,
                NodeInfo {
                    media: dev.path().to_owned(),
                    entity: e.name.clone(),
                    function: e.function,
                    pads: topo
                        .pads_of(e.id)
                        .iter()
                        .map(|p| (p.index, p.flags))
                        .collect(),
                },
            );
        }
    }
}

fn print_topology(topo: &Topology) {
    println!("  topology version {}", topo.version);
    for e in &topo.entities {
        let node = topo
            .devnode_path(e.id)
            .map(|p| format!(" -> {}", p.display()))
            .unwrap_or_default();
        println!("  entity {} {:?} [{}]{node}", e.id, e.name, e.function);
        for p in topo.pads_of(e.id) {
            println!("    pad {} {:?}", p.index, p.flags);
        }
    }
    let name = |id: u32| topo.entity(id).map_or("?", |e| e.name.as_str());
    for l in topo.data_links() {
        println!(
            "  link {:?}:{} -> {:?}:{} [{:?}]",
            name(l.source.entity),
            l.source.index,
            name(l.sink.entity),
            l.sink.index,
            l.flags
        );
    }
    for i in &topo.interfaces {
        let target = topo
            .entity_of_interface(i.id)
            .map_or("?", |e| e.name.as_str());
        let dev = i
            .devnode
            .map(|d| format!("{}:{}", d.major, d.minor))
            .unwrap_or_default();
        println!(
            "  interface {:#x} {} {dev} -> {target:?}",
            i.id, i.intf_type
        );
    }
}

fn fps(f: Fraction) -> String {
    format!("{:.3}", f.fps())
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn probe_video(path: &Path, info: Option<&NodeInfo>, show_controls: bool) {
    let dev = match VideoDevice::open_read_only(path) {
        Ok(d) => d,
        Err(e) => {
            println!("== video {}: {e}", path.display());
            return;
        }
    };
    let caps = dev.capabilities();
    println!(
        "== video {}: card {:?} driver {:?} bus {:?} version {}",
        path.display(),
        caps.card,
        caps.driver,
        caps.bus_info,
        caps.version
    );
    if let Some(i) = info {
        println!(
            "  entity {:?} [{}] in {}",
            i.entity,
            i.function,
            i.media.display()
        );
    }
    println!("  caps {:?}", caps.capabilities);
    println!("  device caps {:?}", caps.device_caps);
    for ty in caps.buffer_types() {
        probe_queue(&dev, ty);
    }
    if show_controls {
        print_controls(&dev);
    }
}

fn probe_queue(dev: &VideoDevice, ty: BufType) {
    println!("  queue {ty:?}");
    match dev.format(ty) {
        Ok(f) => println!("    current {}", describe_format(&f)),
        Err(e) => println!("    current: {e}"),
    }
    match dev.stream_params(ty) {
        Ok(p) if p.supports_time_per_frame() => {
            println!(
                "    frame interval {} ({} fps)",
                p.time_per_frame,
                fps(p.time_per_frame)
            );
        }
        Ok(_) => {}
        Err(e) if e.is_not_supported() => {}
        Err(e) => println!("    stream params: {e}"),
    }
    if matches!(ty, BufType::VideoCapture | BufType::VideoCaptureMplane)
        && let Ok(r) = dev.selection(ty, SelectionTarget::CropBounds)
    {
        println!("    crop bounds {r}");
    }
    let formats = match dev.formats(ty) {
        Ok(f) => f,
        Err(e) => {
            println!("    formats: {e}");
            return;
        }
    };
    for f in formats {
        let flags = if f.flags.is_empty() {
            String::new()
        } else {
            format!(" {:?}", f.flags)
        };
        println!("    format {} {:?}{flags}", f.fourcc, f.description);
        if ty.is_meta() {
            continue;
        }
        match dev.frame_sizes(f.fourcc) {
            Ok(FrameSizes::Discrete(sizes)) => {
                for s in sizes {
                    let ivals = describe_intervals(dev, f.fourcc, s.width, s.height);
                    println!("      {}x{}{ivals}", s.width, s.height);
                }
            }
            Ok(FrameSizes::Stepwise(r) | FrameSizes::Continuous(r)) => {
                println!(
                    "      {}x{} .. {}x{} step {}x{}",
                    r.min_width,
                    r.min_height,
                    r.max_width,
                    r.max_height,
                    r.step_width,
                    r.step_height
                );
                let ivals = describe_intervals(dev, f.fourcc, r.max_width, r.max_height);
                if !ivals.is_empty() {
                    println!("      at {}x{}{ivals}", r.max_width, r.max_height);
                }
            }
            Err(e) if e.is_invalid_argument() || e.is_not_supported() => {}
            Err(e) => println!("      sizes: {e}"),
        }
    }
}

fn describe_format(f: &Format) -> String {
    match f {
        Format::Single(p) => format!(
            "{}x{} {} stride {} size {} colorspace {} field {}",
            p.width, p.height, p.fourcc, p.bytes_per_line, p.size_image, p.colorspace, p.field
        ),
        Format::Multi(p) => {
            let planes: Vec<String> = p
                .planes
                .iter()
                .map(|pl| format!("{}/{}", pl.bytes_per_line, pl.size_image))
                .collect();
            format!(
                "{}x{} {} planes [{}] colorspace {}",
                p.width,
                p.height,
                p.fourcc,
                planes.join(", "),
                p.colorspace
            )
        }
        Format::Meta(m) => format!(
            "meta {} buffer {} ({}x{} stride {})",
            m.fourcc, m.buffer_size, m.width, m.height, m.bytes_per_line
        ),
    }
}

fn describe_intervals(dev: &VideoDevice, fourcc: FourCc, width: u32, height: u32) -> String {
    match dev.frame_intervals(fourcc, width, height) {
        Ok(FrameIntervals::Discrete(list)) if !list.is_empty() => {
            let fps: Vec<String> = list.iter().map(|&i| fps(i)).collect();
            format!(" @ {} fps", fps.join(", "))
        }
        Ok(FrameIntervals::Stepwise { min, max, step }) => {
            format!(" @ {}..{} fps (interval step {step})", fps(max), fps(min))
        }
        Ok(FrameIntervals::Continuous { min, max }) => format!(" @ {}..{} fps", fps(max), fps(min)),
        _ => String::new(),
    }
}

fn print_controls(dev: &impl Controls) {
    let controls = match dev.query_controls() {
        Ok(c) => c,
        Err(e) => {
            println!("  controls: {e}");
            return;
        }
    };
    if controls.is_empty() {
        return;
    }
    println!("  controls");
    for c in &controls {
        if c.control_type == ControlType::CtrlClass {
            println!("    -- {}", c.name);
            continue;
        }
        let value = if c.is_readable() {
            match dev.get_controls_for(ControlWhich::Current, std::slice::from_ref(c)) {
                Ok(mut v) => describe_value(c, v.remove(0)),
                Err(e) => format!("<{e}>"),
            }
        } else {
            "-".to_string()
        };
        let flags = if c.flags.is_empty() {
            String::new()
        } else {
            format!(" [{:?}]", c.flags)
        };
        let dims = if c.dims.is_empty() {
            String::new()
        } else {
            format!(" dims {:?}", c.dims)
        };
        println!(
            "    {:#010x} {:?} {:?} min {} max {} step {} default {} = {value}{dims}{flags}",
            c.id, c.name, c.control_type, c.minimum, c.maximum, c.step, c.default
        );
        if c.is_menu() {
            match dev.query_menu(c) {
                Ok(items) => {
                    for item in items {
                        match item.value {
                            MenuValue::Name(n) => println!("      {}: {n}", item.index),
                            MenuValue::Integer(v) => println!("      {}: {v}", item.index),
                        }
                    }
                }
                Err(e) => println!("      menu: {e}"),
            }
        }
    }
}

fn describe_value(c: &ControlInfo, v: ControlValue) -> String {
    match v {
        ControlValue::Integer(i) => i.to_string(),
        ControlValue::Integer64(i) => i.to_string(),
        ControlValue::String(s) => format!("{s:?}"),
        ControlValue::Payload(p) if p.len() <= 16 => format!("{p:02x?}"),
        ControlValue::Payload(p) => format!("<{} bytes, elem {}>", p.len(), c.elem_size),
    }
}

fn probe_subdev(path: &Path, info: Option<&NodeInfo>, show_controls: bool) {
    let sd = match Subdev::open_read_only(path) {
        Ok(s) => s,
        Err(e) => {
            println!("== subdev {}: {e}", path.display());
            return;
        }
    };
    let name = sysfs_name(path).unwrap_or_default();
    println!("== subdev {}: {name:?}", path.display());
    if let Some(i) = info {
        println!(
            "  entity {:?} [{}] in {}",
            i.entity,
            i.function,
            i.media.display()
        );
    }
    match sd.capabilities() {
        Ok(c) => println!("  caps {:?}", c.capabilities),
        Err(e) => println!("  caps: {e}"),
    }
    // Pads from the media graph; without one, try pads until the driver says no.
    let pads: Vec<(u32, Option<PadFlags>)> = match info {
        Some(i) => i.pads.iter().map(|&(idx, f)| (idx, Some(f))).collect(),
        None => (0..16)
            .take_while(|&p| sd.format(p, Which::Active).is_ok())
            .map(|p| (p, None))
            .collect(),
    };
    for (pad, flags) in pads {
        let flags = flags.map(|f| format!(" {f:?}")).unwrap_or_default();
        println!("  pad {pad}{flags}");
        match sd.format(pad, Which::Active) {
            Ok(f) => println!(
                "    active format {}x{} {} field {} colorspace {}",
                f.width, f.height, f.code, f.field, f.colorspace
            ),
            Err(e) => println!("    active format: {e}"),
        }
        for target in [
            SelectionTarget::Crop,
            SelectionTarget::NativeSize,
            SelectionTarget::CropBounds,
        ] {
            if let Ok(r) = sd.selection(pad, Which::Active, target) {
                println!("    {target:?} {r}");
            }
        }
        if let Ok(i) = sd.frame_interval(pad, Which::Active)
            && i.denominator != 0
        {
            println!("    frame interval {i} ({} fps)", fps(i));
        }
        let codes = match sd.mbus_codes(pad, Which::Active) {
            Ok(c) => c,
            Err(e) if e.is_not_supported() => {
                println!("    codes: not enumerable (ENOTTY)");
                continue;
            }
            Err(e) => {
                println!("    codes: {e}");
                continue;
            }
        };
        for code in codes {
            let sizes = sd.frame_sizes(pad, code, Which::Active).unwrap_or_default();
            let sizes: Vec<String> = sizes
                .iter()
                .map(|s| {
                    let base = if s.is_discrete() {
                        format!("{}x{}", s.max_width, s.max_height)
                    } else {
                        format!(
                            "{}x{}..{}x{}",
                            s.min_width, s.min_height, s.max_width, s.max_height
                        )
                    };
                    let ivals = sd
                        .frame_intervals(pad, code, s.max_width, s.max_height, Which::Active)
                        .unwrap_or_default();
                    if ivals.is_empty() {
                        base
                    } else {
                        let f: Vec<String> = ivals.iter().map(|&i| fps(i)).collect();
                        format!("{base} @ {} fps", f.join("/"))
                    }
                })
                .collect();
            println!("    code {code} ({:#06x}): {}", code.0, sizes.join(", "));
        }
    }
    if show_controls {
        print_controls(&sd);
    }
}

fn sysfs_name(dev: &Path) -> Option<String> {
    let node = dev.file_name()?.to_str()?;
    let name = std::fs::read_to_string(format!("/sys/class/video4linux/{node}/name")).ok()?;
    Some(name.trim().to_string())
}
