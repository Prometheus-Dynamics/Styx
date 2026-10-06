//! Connection changes in order with what a client receives (`try_client_event`,
//! `next_client_event`, `client_events`): the camera service is stopped and started again, and
//! frame and control clients report `Disconnected` then `Connected`, once each, through `poll(2)`
//! on their descriptors and awaited; a setting re-applied on `Connected` holds. Also: a blocking
//! connection's first `Connected`, a non-blocking client's first connection, a client that does
//! not reconnect (`Disconnected` once, then `Closed`), and changes queued between reads.

#![cfg(target_os = "linux")]

use std::future::poll_fn;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device_with_controls;
use styx::ipc::{
    CameraService, CameraServiceHandle, ClientEvent, ControlClient, ControlEvent, FrameClient,
    StandardControl,
};
use styx::prelude::*;
use styx_graph::rt::block_on;

const EXPOSURE: ControlId = ControlId(0xF400_0001);

fn camera(name: &str) -> ProbedDevice {
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(160, 100).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(60).unwrap(),
    );
    let exposure = ControlMeta {
        id: EXPOSURE,
        name: "exposure_time_us".into(),
        kind: ControlKind::Uint,
        access: Access::ReadWrite,
        default: ControlValue::Uint(10),
        min: ControlValue::Uint(10),
        max: ControlValue::Uint(33_000),
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    };
    make_virtual_device_with_controls(name, [mode], vec![exposure])
}

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-conn-{name}-{}.sock", std::process::id()))
}

fn serve(path: &PathBuf) -> CameraServiceHandle {
    CameraService::new(camera("cam"))
        .keep_streaming()
        .serve(path)
        .unwrap()
}

/// Wait up to `ms` for `fd` to be readable.
fn readable(fd: i32, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a valid pollfd for the call's duration.
    unsafe { libc::poll(&mut p, 1, ms) };
    p.revents != 0
}

/// What a test looks at in an event.
#[derive(Debug, PartialEq)]
enum Seen {
    Connected(u64),
    Disconnected,
    Data,
    Closed,
}

fn seen<T>(event: ClientEvent<T>) -> Seen {
    match event {
        ClientEvent::Connected { reconnects } => Seen::Connected(reconnects),
        ClientEvent::Disconnected { .. } => Seen::Disconnected,
        ClientEvent::Data(_) => Seen::Data,
    }
}

/// A poll loop: wait for the descriptor (it must become readable), then drain with `next`,
/// until `until` has been seen; everything seen, in order (consecutive `Data` as one).
fn poll_until<T>(
    fd: i32,
    mut next: impl FnMut() -> RecvOutcome<ClientEvent<T>>,
    until: &Seen,
) -> Vec<Seen> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "never saw {until:?}: {all:?}");
        assert!(readable(fd, 3000), "the descriptor never woke: {all:?}");
        loop {
            let started = Instant::now();
            let got = next();
            assert!(started.elapsed() < Duration::from_millis(100), "blocked");
            let got = match got {
                RecvOutcome::Data(event) => seen(event),
                RecvOutcome::Empty => break,
                RecvOutcome::Closed => Seen::Closed,
            };
            let done = &got == until;
            if !(got == Seen::Data && all.last() == Some(&Seen::Data)) {
                all.push(got);
            }
            if done {
                return all;
            }
        }
    }
}

/// The same, awaited (`next_client_event` on `block_on`).
fn await_until<T>(
    mut next: impl FnMut() -> RecvOutcome<ClientEvent<T>>,
    until: &Seen,
) -> Vec<Seen> {
    let mut all = Vec::new();
    loop {
        let got = match next() {
            RecvOutcome::Data(event) => seen(event),
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => Seen::Closed,
        };
        let done = &got == until;
        if !(got == Seen::Data && all.last() == Some(&Seen::Data)) {
            all.push(got);
        }
        if done {
            return all;
        }
        assert!(all.len() < 10_000, "never saw {until:?}");
    }
}

fn exposure(event: &RecvOutcome<ClientEvent<ControlEvent>>) -> Option<ControlValue> {
    match event {
        RecvOutcome::Data(ClientEvent::Data(e)) if e.id == EXPOSURE => Some(e.value.clone()),
        _ => None,
    }
}

#[test]
fn a_service_restart_is_reported_through_poll_and_settings_are_reapplied() {
    let path = socket("sync");
    let mut service = Some(serve(&path));
    let frames = FrameClient::options(&path)
        .reconnecting()
        .request(&Frames::nv12())
        .unwrap();
    let controls = ControlClient::options(&path)
        .reconnecting()
        .controls()
        .unwrap();
    let (ffd, cfd) = (frames.as_raw_fd(), controls.as_raw_fd());

    // Connected before `request`/`controls` returned: still the first event.
    assert!(readable(cfd, 0), "the first Connected did not wake");
    assert_eq!(
        poll_until(cfd, || controls.try_client_event(), &Seen::Connected(0)),
        [Seen::Connected(0)]
    );
    assert_eq!(
        poll_until(ffd, || frames.try_client_event(), &Seen::Data),
        [Seen::Connected(0), Seen::Data]
    );
    controls.set_exposure_us(1000).unwrap();
    assert!(readable(cfd, 2000));
    assert_eq!(
        exposure(&controls.try_client_event()),
        Some(ControlValue::Uint(1000))
    );
    // Drained and idle: not readable (no change is reported twice).
    assert!(matches!(controls.try_client_event(), RecvOutcome::Empty));
    assert!(!readable(cfd, 100), "readable with nothing to read");

    for round in 1..=2u64 {
        drop(service.take());
        // Disconnected, once, after whatever came on the old connection.
        let lost = poll_until(cfd, || controls.try_client_event(), &Seen::Disconnected);
        assert_eq!(lost, [Seen::Disconnected], "round {round}");
        let lost = poll_until(ffd, || frames.try_client_event(), &Seen::Disconnected);
        assert_eq!(lost.last(), Some(&Seen::Disconnected));
        assert!(!lost.contains(&Seen::Connected(round)), "{lost:?}");
        assert!(!controls.is_connected() && !frames.is_connected());

        // Back: Connected (and nothing from the new connection before it).
        service = Some(serve(&path));
        let back = poll_until(cfd, || controls.try_client_event(), &Seen::Connected(round));
        assert_eq!(back, [Seen::Connected(round)], "round {round}");
        let back = poll_until(ffd, || frames.try_client_event(), &Seen::Data);
        assert_eq!(back, [Seen::Connected(round), Seen::Data], "round {round}");
        assert_eq!((controls.reconnects(), frames.reconnects()), (round, round));

        // Re-apply the setting on Connected: it holds, and its event comes after.
        let value = 1000 + round as u32;
        controls.set_exposure_us(value).unwrap();
        assert_eq!(
            frames.get_control(EXPOSURE).unwrap(),
            ControlValue::Uint(value)
        );
        assert!(readable(cfd, 2000));
        assert_eq!(
            exposure(&controls.try_client_event()),
            Some(ControlValue::Uint(value))
        );
    }
}

#[test]
fn a_service_restart_is_reported_when_awaited() {
    let path = socket("async");
    let service = serve(&path);
    let frames = FrameClient::options(&path)
        .reconnecting()
        .request(&Frames::nv12())
        .unwrap();
    let controls = ControlClient::options(&path)
        .reconnecting()
        .controls()
        .unwrap();
    let next_control = || block_on(controls.next_client_event());
    assert_eq!(
        await_until(next_control, &Seen::Connected(0)),
        [Seen::Connected(0)]
    );
    let mut stream = frames.client_events();
    let mut next_frame = || match block_on(poll_fn(|cx| {
        futures_core::Stream::poll_next(Pin::new(&mut stream), cx)
    })) {
        Some(event) => RecvOutcome::Data(event),
        None => RecvOutcome::Closed,
    };
    assert_eq!(
        await_until(&mut next_frame, &Seen::Data),
        [Seen::Connected(0), Seen::Data]
    );

    drop(service);
    assert_eq!(
        await_until(next_control, &Seen::Disconnected),
        [Seen::Disconnected]
    );
    assert_eq!(
        await_until(&mut next_frame, &Seen::Disconnected).last(),
        Some(&Seen::Disconnected)
    );
    let _service = serve(&path);
    assert_eq!(
        await_until(next_control, &Seen::Connected(1)),
        [Seen::Connected(1)]
    );
    assert_eq!(
        await_until(&mut next_frame, &Seen::Data),
        [Seen::Connected(1), Seen::Data]
    );
    let applied =
        block_on(controls.set_control_async(StandardControl::ExposureUs, ControlValue::Uint(2500)))
            .unwrap();
    assert_eq!(applied.value, ControlValue::Uint(2500));
    assert_eq!(
        exposure(&block_on(controls.next_client_event())),
        Some(ControlValue::Uint(2500))
    );
}

#[test]
fn a_client_that_does_not_reconnect_reports_one_disconnected_then_closed() {
    // Connected, then the service goes: Disconnected once, then Closed (and stays readable).
    let path = socket("plain");
    let service = serve(&path);
    let frames = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let controls = ControlClient::connect(&path).unwrap();
    assert_eq!(
        poll_until(
            frames.as_raw_fd(),
            || frames.try_client_event(),
            &Seen::Data
        ),
        [Seen::Connected(0), Seen::Data]
    );
    drop(service);
    let end = poll_until(
        controls.as_raw_fd(),
        || controls.try_client_event(),
        &Seen::Closed,
    );
    assert_eq!(end, [Seen::Connected(0), Seen::Disconnected, Seen::Closed]);
    let end = poll_until(
        frames.as_raw_fd(),
        || frames.try_client_event(),
        &Seen::Closed,
    );
    assert_eq!(end[end.len() - 2..], [Seen::Disconnected, Seen::Closed]);
    for client in [frames.as_raw_fd(), controls.as_raw_fd()] {
        assert!(readable(client, 0), "closed but not readable");
    }
    assert!(matches!(frames.try_client_event(), RecvOutcome::Closed));
    assert!(matches!(
        block_on(controls.next_client_event()),
        RecvOutcome::Closed
    ));

    // Never connected, gives up: Disconnected (why) once, then Closed.
    let gone = socket("gone");
    let _ = std::fs::remove_file(&gone);
    let quick = ControlClient::options(&gone)
        .timeout(Duration::from_millis(300))
        .controls_nonblocking()
        .unwrap();
    let end = poll_until(
        quick.as_raw_fd(),
        || quick.try_client_event(),
        &Seen::Closed,
    );
    assert_eq!(end, [Seen::Disconnected, Seen::Closed]);
    let quick = FrameClient::options(&gone)
        .timeout(Duration::from_millis(300))
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    let end = poll_until(
        quick.as_raw_fd(),
        || quick.try_client_event(),
        &Seen::Closed,
    );
    assert_eq!(end, [Seen::Disconnected, Seen::Closed]);
}

#[test]
fn a_nonblocking_client_reports_its_first_connection_and_changes_queue_in_order() {
    let path = socket("later");
    let _ = std::fs::remove_file(&path);
    let frames = FrameClient::options(&path)
        .reconnecting()
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    let controls = ControlClient::options(&path)
        .reconnecting()
        .controls_nonblocking()
        .unwrap();
    // Not there yet: nothing to report (it was never connected).
    std::thread::sleep(Duration::from_millis(250));
    assert!(matches!(controls.try_client_event(), RecvOutcome::Empty));
    assert!(matches!(frames.try_client_event(), RecvOutcome::Empty));

    let service = serve(&path);
    assert_eq!(
        poll_until(
            frames.as_raw_fd(),
            || frames.try_client_event(),
            &Seen::Data
        ),
        [Seen::Connected(0), Seen::Data]
    );

    // Several changes before the client's events are read: queued, each once, in order.
    block_on(controls.ready()).unwrap();
    drop(service);
    let deadline = Instant::now() + Duration::from_secs(5);
    while controls.is_connected() {
        assert!(Instant::now() < deadline, "never noticed");
        // Noticed by a call that leaves connection changes unread.
        controls.recv_event(Duration::from_millis(100));
    }
    let _service = serve(&path);
    block_on(controls.ready()).unwrap();
    assert!(readable(controls.as_raw_fd(), 0));
    let all = poll_until(
        controls.as_raw_fd(),
        || controls.try_client_event(),
        &Seen::Connected(1),
    );
    assert_eq!(
        all,
        [Seen::Connected(0), Seen::Disconnected, Seen::Connected(1)]
    );
    assert!(matches!(controls.try_client_event(), RecvOutcome::Empty));
}
