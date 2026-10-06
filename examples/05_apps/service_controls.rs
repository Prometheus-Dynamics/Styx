//! A camera's controls without its frames: a `ControlClient` sets exposure and follows every
//! control change on the camera, never joining the camera's frame plan or holding a buffer.
//! It connects without blocking, so it can start before the service (or the camera) is there.
//!
//! ```text
//! service_controls [--camera NAME] [EXPOSURE_US]   # the service at $STYX_SOCKET
//!                                                  #   (or /tmp/styx-camera.sock)
//! service_controls --demo                          # a service with a virtual camera, here
//! ```

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device_with_controls;
use styx::ipc::{CameraService, ControlClient, FrameClient};
use styx::prelude::*;

fn socket_path() -> String {
    std::env::var("STYX_SOCKET").unwrap_or_else(|_| "/tmp/styx-camera.sock".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let demo = args.first().is_some_and(|a| a == "--demo");
    let path = if demo {
        std::env::temp_dir().join(format!("styx-controls-demo-{}.sock", std::process::id()))
    } else {
        socket_path().into()
    };
    let mut options = ControlClient::options(&path).reconnecting();
    if args.first().is_some_and(|a| a == "--camera") {
        options = options.camera(args.get(1).ok_or("--camera NAME")?);
        args.drain(..2);
    }
    let exposure: u32 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(8000);

    // Returns at once, whether or not the service is up; it connects in the background.
    let controls = options.controls_nonblocking()?;
    // The demo's service (and a frame client of its own) appear only now.
    let demo = demo.then(|| demo_service(&path)).transpose()?;

    // One poll loop: the client's descriptor is readable when an event or news is waiting.
    let mut set = false;
    let until = Instant::now() + Duration::from_secs(if demo.is_some() { 2 } else { 10 });
    while Instant::now() < until {
        let mut p = libc::pollfd {
            fd: controls.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a valid pollfd for the call's duration.
        unsafe { libc::poll(&mut p, 1, 100) };
        loop {
            match controls.try_event() {
                RecvOutcome::Data(e) => println!(
                    "{:?} {:#010x} = {:?} (by {})",
                    e.standard,
                    e.id.0,
                    e.value,
                    e.by.map_or("a control client".into(), |id| format!("client {id}"))
                ),
                RecvOutcome::Empty => break,
                RecvOutcome::Closed => return Err("the camera service went away".into()),
            }
        }
        if !set && controls.is_connected() {
            set = true;
            let applied = controls.set_exposure_us(exposure)?;
            println!(
                "exposure {:?}{}{}",
                applied.value,
                if applied.clamped { " (clamped)" } else { "" },
                if applied.deferred {
                    " (applied when the camera starts)"
                } else {
                    ""
                }
            );
            // Another client's change reaches this one too.
            if let Some((_, frames)) = &demo {
                frames.set_gain(2.0)?;
            }
        }
    }
    if !set {
        let why = controls.last_error().map(|e| e.to_string());
        return Err(format!("no camera service: {}", why.unwrap_or_default()).into());
    }
    Ok(())
}

/// A service with a virtual camera with exposure and gain, and a frame client of it.
fn demo_service(
    path: &std::path::Path,
) -> Result<(styx::ipc::CameraServiceHandle, FrameClient), Box<dyn std::error::Error>> {
    let meta = |id, name: &str, kind, min: ControlValue, max| ControlMeta {
        id: ControlId(id),
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        default: min.clone(),
        min,
        max,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    };
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(320, 200).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(30).unwrap(),
    );
    let camera = make_virtual_device_with_controls(
        "demo",
        [mode],
        vec![
            meta(
                0xF400_0001,
                "exposure_time_us",
                ControlKind::Uint,
                ControlValue::Uint(10),
                ControlValue::Uint(33_000),
            ),
            meta(
                0xF400_0002,
                "gain",
                ControlKind::Float,
                ControlValue::Float(1.0),
                ControlValue::Float(16.0),
            ),
        ],
    );
    let service = CameraService::new(camera).keep_streaming().serve(path)?;
    let frames = FrameClient::request(path, &Frames::nv12())?;
    Ok((service, frames))
}
