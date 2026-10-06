//! Several cameras' frames on one thread: no thread per camera. Each `FrameClient` is a file
//! descriptor that is readable when it has a frame, and can be awaited on any executor.
//!
//! ```text
//! camera_service_async [poll|async] [camera ...]   # clients of the service at $STYX_SOCKET
//!                                                  #   (or /tmp/styx-camera.sock), one per camera
//! camera_service_async [poll|async] --demo         # a service with three virtual cameras, here
//! ```
//!
//! `poll`: one `poll(2)` over every client's descriptor, then `try_next` on the readable ones
//! until they have nothing more; the same loop fits an epoll set or a GUI/event loop that
//! watches descriptors. `async`: one future polling every client (`FrameClient::poll_next`),
//! run by `styx_graph::rt::block_on` here; any executor (tokio, smol) works the same, waking
//! through styx-graph's reactor thread. Where frames go next (a Daedalus host input with
//! `host.push_payload("frame", frame_payload(frame))`, an encoder, a UI) happens on this thread.

use std::future::poll_fn;
use std::os::fd::AsRawFd;
use std::task::Poll;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device;
use styx::ipc::{CameraService, CameraServiceHandle, FrameClient};
use styx::prelude::*;

fn socket_path() -> String {
    std::env::var("STYX_SOCKET").unwrap_or_else(|_| "/tmp/styx-camera.sock".into())
}

/// A service with three virtual cameras, for trying this without cameras.
fn demo(path: &str) -> Result<CameraServiceHandle, Box<dyn std::error::Error>> {
    let camera = |name: &str, fps| {
        let mode = Mode::with_interval(
            MediaFormat::new(
                FourCc::NV12,
                Resolution::new(640, 400).unwrap(),
                ColorSpace::Srgb,
            ),
            Interval::from_fps(fps).unwrap(),
        );
        make_virtual_device(name, [mode])
    };
    Ok(CameraService::with_cameras(vec![
        camera("front", 30),
        camera("left", 30),
        camera("right", 15),
    ])
    .keep_streaming()
    .serve(path)?)
}

/// What each frame feeds: here a count per camera.
fn consume(counts: &mut [u64], camera: usize, frame: FrameLease) {
    counts[camera] += 1;
    std::hint::black_box(frame.meta().timestamp);
}

/// One `poll(2)` over every client; `try_next` drains the readable ones.
fn poll_loop(clients: &[FrameClient], until: Instant) -> Vec<u64> {
    let mut counts = vec![0; clients.len()];
    let mut polls: Vec<libc::pollfd> = clients
        .iter()
        .map(|c| libc::pollfd {
            fd: c.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    while Instant::now() < until {
        // SAFETY: valid pollfds for the call's duration.
        unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 100) };
        for (i, p) in polls.iter().enumerate() {
            if p.revents == 0 {
                continue;
            }
            loop {
                match clients[i].try_next() {
                    RecvOutcome::Data(frame) => consume(&mut counts, i, frame),
                    RecvOutcome::Empty => break,
                    RecvOutcome::Closed => return counts,
                }
            }
        }
    }
    counts
}

/// One future polling every client, on any executor (here: block_on on this thread).
fn async_loop(clients: &[FrameClient], until: Instant) -> Vec<u64> {
    let mut counts = vec![0; clients.len()];
    styx_graph::rt::block_on(styx_graph::rt::timeout(
        until.saturating_duration_since(Instant::now()),
        poll_fn(|cx| {
            for (i, client) in clients.iter().enumerate() {
                loop {
                    match client.poll_next(cx) {
                        Poll::Ready(RecvOutcome::Data(frame)) => consume(&mut counts, i, frame),
                        Poll::Ready(_) => return Poll::Ready(()),
                        Poll::Pending => break,
                    }
                }
            }
            Poll::Pending
        }),
    ))
    .ok();
    counts
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let asynchronous = args.first().is_some_and(|a| a == "async");
    let mut rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !matches!(*a, "poll" | "async"))
        .collect();
    let path = socket_path();
    let _service = if rest.first() == Some(&"--demo") {
        rest = vec!["front", "left", "right"];
        Some(demo(&path)?)
    } else {
        None
    };
    let names: Vec<String> = if rest.is_empty() {
        FrameClient::cameras(&path)?
            .into_iter()
            .map(|c| c.name)
            .collect()
    } else {
        rest.iter().map(|s| s.to_string()).collect()
    };
    let clients = names
        .iter()
        .map(|name| {
            FrameClient::options(&path)
                .camera(name)
                .reconnecting()
                .request(&Frames::nv12())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let started = Instant::now();
    let until = started + Duration::from_secs(5);
    let counts = if asynchronous {
        async_loop(&clients, until)
    } else {
        poll_loop(&clients, until)
    };
    let secs = started.elapsed().as_secs_f64();
    for (name, n) in names.iter().zip(counts) {
        println!("{name}: {n} frames, {:.1} fps", n as f64 / secs);
    }
    println!(
        "one thread, {} cameras ({})",
        names.len(),
        if asynchronous { "async" } else { "poll" }
    );
    Ok(())
}
