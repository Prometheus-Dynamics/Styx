//! Frame clients without a thread each: several clients of two cameras on one thread, through
//! `poll(2)` on their descriptors and `try_next`, and awaited (`next`, `poll_next`, `stream`)
//! on styx-graph's `block_on`; a client sees its service go away, and a reconnecting one comes
//! back without blocking.

use std::future::poll_fn;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device;
use styx::ipc::{CameraService, CameraServiceHandle, FrameClient};
use styx::prelude::*;
use styx_graph::rt::block_on;

fn camera(name: &str, fps: u32) -> ProbedDevice {
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(160, 100).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(fps).unwrap(),
    );
    make_virtual_device(name, [mode])
}

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-async-{name}-{}.sock", std::process::id()))
}

fn serve(path: &PathBuf) -> CameraServiceHandle {
    CameraService::with_cameras(vec![camera("left", 60), camera("right", 60)])
        .keep_streaming()
        .serve(path)
        .unwrap()
}

/// Three clients of two cameras (two share one).
fn clients(path: &PathBuf) -> Vec<FrameClient> {
    let req = Frames::nv12();
    vec![
        FrameClient::request_camera(path, "left", &req).unwrap(),
        FrameClient::request_camera(path, "right", &req).unwrap(),
        FrameClient::request_camera(path, "left", &req).unwrap(),
    ]
}

/// Wait up to `ms` for any of `fds` to be readable; which are.
fn poll_fds(fds: &[i32], ms: i32) -> Vec<bool> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: valid pollfds for the call's duration.
    unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
    polls.iter().map(|p| p.revents != 0).collect()
}

#[test]
fn one_thread_polls_many_clients() {
    let path = socket("poll");
    let _service = serve(&path);
    let clients = clients(&path);
    let fds: Vec<i32> = clients.iter().map(AsRawFd::as_raw_fd).collect();
    let mut counts = [0usize; 3];
    let deadline = Instant::now() + Duration::from_secs(10);
    while counts.iter().any(|&n| n < 20) {
        assert!(Instant::now() < deadline, "frames: {counts:?}");
        for (i, ready) in poll_fds(&fds, 1000).into_iter().enumerate() {
            if !ready {
                continue;
            }
            // Drain: the descriptor is readable until try_next has nothing more.
            while let RecvOutcome::Data(frame) = clients[i].try_next() {
                assert_eq!(frame.meta().format.code, FourCc::NV12);
                counts[i] += 1;
            }
        }
    }
    // Nothing waiting: not readable, and try_next does not block.
    let started = Instant::now();
    for client in &clients {
        while let RecvOutcome::Data(_) = client.try_next() {}
    }
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[test]
fn frames_are_awaited_on_any_executor() {
    let path = socket("await");
    let _service = serve(&path);
    let clients = clients(&path);

    // One future polling every client, on block_on (no runtime).
    let mut counts = [0usize; 3];
    let counts = block_on(poll_fn(|cx| {
        for (i, client) in clients.iter().enumerate() {
            while counts[i] < 15 {
                match client.poll_next(cx) {
                    Poll::Ready(RecvOutcome::Data(_)) => counts[i] += 1,
                    Poll::Ready(other) => {
                        panic!("closed: {}", matches!(other, RecvOutcome::Closed))
                    }
                    Poll::Pending => break,
                }
            }
        }
        if counts.iter().all(|&n| n >= 15) {
            Poll::Ready(counts)
        } else {
            Poll::Pending
        }
    }));
    assert!(counts.iter().all(|&n| n >= 15));

    // `next` and `stream`, one client at a time.
    for _ in 0..5 {
        assert!(matches!(block_on(clients[0].next()), RecvOutcome::Data(_)));
    }
    let mut stream = clients[1].stream();
    for _ in 0..5 {
        let frame = block_on(poll_fn(|cx| {
            futures_core::Stream::poll_next(Pin::new(&mut stream), cx)
        }));
        assert!(frame.is_some());
    }
}

#[test]
fn a_client_learns_its_service_went_away_and_a_reconnecting_one_comes_back() {
    let path = socket("gone");
    let service = serve(&path);
    let plain = FrameClient::request_camera(&path, "left", &Frames::nv12()).unwrap();
    let back = FrameClient::options(&path)
        .camera("right")
        .timeout(Duration::from_secs(2))
        .reconnecting()
        .request(&Frames::nv12())
        .unwrap();
    assert!(matches!(block_on(back.next()), RecvOutcome::Data(_)));
    drop(service);

    // The plain client: its descriptor becomes readable and try_next says closed (and stays
    // readable, so a poll loop does not miss it).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "never closed");
        poll_fds(&[plain.as_raw_fd()], 500);
        if matches!(plain.try_next(), RecvOutcome::Closed) {
            break;
        }
    }
    assert!(poll_fds(&[plain.as_raw_fd()], 100)[0]);
    assert!(matches!(block_on(plain.next()), RecvOutcome::Closed));

    // The reconnecting client: Empty meanwhile, frames again once the service is back, without
    // a blocking call (the poll loop wakes for each attempt).
    let _service = serve(&path);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "never reconnected");
        poll_fds(&[back.as_raw_fd()], 500);
        let started = Instant::now();
        let got = back.try_next();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "try_next blocked"
        );
        match got {
            RecvOutcome::Data(_) => break,
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => panic!("a reconnecting client closed"),
        }
    }
    assert_eq!(back.reconnects(), 1);
}
