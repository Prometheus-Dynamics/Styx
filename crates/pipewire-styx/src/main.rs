//! `styx-pipewire`: publish Styx cameras as PipeWire video sources.
//!
//! ```text
//! styx-pipewire                         every camera Styx finds, opened in this process
//! styx-pipewire --camera ov9782         only cameras matching (name, part of it, key)
//! styx-pipewire --service /run/styx/cam.sock
//!                                       the cameras of a Styx camera service, shared with its
//!                                       other clients
//! ```
//!
//! Each camera becomes a `Video/Source` node with role `Camera` (what the camera portal, browsers
//! and OBS look for). It offers the formats and sizes the Styx planner can deliver; the camera
//! runs only while a consumer streams from it.

// Without PipeWire only the format and capture logic is built (for tests).
#![cfg_attr(not(feature = "pipewire"), allow(dead_code))]

mod capture;
mod formats;
#[cfg(feature = "pipewire")]
mod node;
#[cfg(feature = "pipewire")]
mod params;
mod pool;

use styx::prelude::*;

/// Command line options.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    cameras: Vec<String>,
    service: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--camera" | "-c" => options
                .cameras
                .push(args.next().ok_or("--camera needs a value")?),
            "--service" | "-s" => {
                options.service = Some(args.next().ok_or("--service needs a socket path")?)
            }
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

const USAGE: &str = "usage: styx-pipewire [--camera SELECTOR]... [--service SOCKET]";

/// A camera to publish: its name, description and where its frames come from, with offers.
struct Published {
    name: String,
    description: String,
    source: capture::Source,
    offers: Vec<formats::Offer>,
}

fn matches(selectors: &[String], name: &str, keys: &[String]) -> bool {
    selectors.is_empty()
        || selectors.iter().any(|s| {
            let s = s.to_ascii_lowercase();
            name.to_ascii_lowercase().contains(&s)
                || keys.iter().any(|k| k.to_ascii_lowercase().contains(&s))
        })
}

fn cameras(options: &Options) -> Result<Vec<Published>, String> {
    if let Some(path) = &options.service {
        let cameras = styx::ipc::FrameClient::cameras(path)
            .map_err(|e| format!("camera service {path}: {e}"))?;
        return Ok(cameras
            .into_iter()
            .filter(|c| matches(&options.cameras, &c.name, &c.keys))
            .map(|c| Published {
                description: format!("{} (Styx service)", c.name),
                source: capture::Source::Service {
                    path: path.clone(),
                    camera: c.name.clone(),
                },
                // The service plans for its clients; offer common sizes in its formats.
                offers: service_offers(),
                name: c.name,
            })
            .collect());
    }
    let devices = match virtual_selectors(&options.cameras)? {
        Some(devices) => devices,
        None => probe_all()
            .into_iter()
            .filter(|d| matches(&options.cameras, &d.identity.display, &d.identity.keys))
            .collect(),
    };
    Ok(devices
        .into_iter()
        .map(|device| Published {
            name: device.identity.display.clone(),
            description: format!("{} (Styx)", device.identity.display),
            offers: formats::offers(&device),
            source: capture::Source::Local(Box::new(device)),
        })
        .filter(|p| !p.offers.is_empty())
        .collect())
}

/// Styx's synthetic test cameras (`virtual` or `virtual:WIDTHxHEIGHT[:FOURCC]`) when every
/// selector names one; `None` when none does (real cameras are probed).
fn virtual_selectors(selectors: &[String]) -> Result<Option<Vec<ProbedDevice>>, String> {
    let specs: Vec<&str> = selectors
        .iter()
        .filter_map(|s| s.strip_prefix("virtual"))
        .collect();
    if specs.is_empty() {
        return Ok(None);
    }
    if specs.len() != selectors.len() {
        return Err("virtual cameras cannot be mixed with real ones".into());
    }
    specs
        .into_iter()
        .map(virtual_camera)
        .collect::<Result<_, _>>()
        .map(Some)
}

fn virtual_camera(spec: &str) -> Result<ProbedDevice, String> {
    let mut config = VirtualSourceConfig::new().name("virtual").fps(30);
    if let Some(rest) = spec.strip_prefix(':') {
        let (size, format) = rest.split_once(':').unwrap_or((rest, ""));
        let (w, h) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .ok_or_else(|| format!("bad virtual camera size \"{size}\" (want WIDTHxHEIGHT)"))?;
        config = config.resolution(w, h);
        if !format.is_empty() {
            let bytes: [u8; 4] = format
                .as_bytes()
                .try_into()
                .map_err(|_| format!("bad virtual camera format \"{format}\" (want a fourcc)"))?;
            config = config.format(FourCc::new(bytes));
        }
    } else if !spec.is_empty() {
        return Err(format!("unknown camera \"virtual{spec}\""));
    }
    Ok(CaptureRequest::virtual_source(config).into_device())
}

/// Sizes offered for a camera behind a camera service (which decides the mode itself; frames of
/// another size than negotiated are dropped).
fn service_offers() -> Vec<formats::Offer> {
    let mut offers = Vec::new();
    for fourcc in [FourCc::YUYV, FourCc::NV12, FourCc::RG24] {
        for (width, height) in [(1280, 720), (640, 480), (640, 360), (320, 240)] {
            offers.push(formats::Offer {
                fourcc,
                width,
                height,
                rates: vec![(30, 1), (15, 1)],
            });
        }
    }
    offers
}

#[cfg(feature = "pipewire")]
fn run(published: Vec<Published>) -> Result<(), String> {
    use pipewire as pw;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("cannot connect to PipeWire: {e}"))?;
    let mut nodes = Vec::new();
    for camera in published {
        match node::publish(
            &core,
            mainloop.loop_(),
            &camera.name,
            &camera.description,
            camera.source,
            &camera.offers,
        ) {
            Ok(node) => nodes.push(node),
            Err(err) => eprintln!("{}: cannot publish: {err}", camera.name),
        }
    }
    if nodes.is_empty() {
        return Err("no camera could be published".into());
    }
    let quit = mainloop.clone();
    let _sigint = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::INT, move || quit.quit());
    let quit = mainloop.clone();
    let _sigterm = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::TERM, move || quit.quit());
    mainloop.run();
    drop(nodes);
    Ok(())
}

#[cfg(not(feature = "pipewire"))]
fn run(published: Vec<Published>) -> Result<(), String> {
    for camera in &published {
        eprintln!("{}: {} formats", camera.name, camera.offers.len());
    }
    Err("built without the `pipewire` feature".into())
}

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(err) => {
            if !err.is_empty() {
                eprintln!("{err}");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let result = cameras(&options).and_then(|published| {
        if published.is_empty() {
            return Err("no cameras found".into());
        }
        run(published)
    });
    if let Err(err) = result {
        eprintln!("styx-pipewire: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_options() {
        let options = parse_args(args(&["--camera", "c270", "-s", "/run/styx.sock"])).unwrap();
        assert_eq!(options.cameras, vec!["c270"]);
        assert_eq!(options.service.as_deref(), Some("/run/styx.sock"));
        assert!(parse_args(args(&["--camera"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }

    #[test]
    fn selectors_match_names_and_keys() {
        let keys = args(&["usb:046d:0825"]);
        assert!(matches(&[], "anything", &[]));
        assert!(matches(&args(&["C270"]), "Logitech C270", &keys));
        assert!(matches(&args(&["046d"]), "cam", &keys));
        assert!(!matches(&args(&["ov9782"]), "Logitech C270", &keys));
    }
}
