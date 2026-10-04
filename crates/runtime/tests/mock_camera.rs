//! A camera on the mock platform: start and stop order, frames from the receiver with the
//! values that produced them, frame starts through the sensor service, buffers given back,
//! frames outliving their stream, a failed start leaving the sensor in standby.

mod common;

use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use common::{Mock, config, noop, sensor};
use styx_hal::mock::MockReceiver;
use styx_hal::{ErrorKind, Receiver, SyncEvent};
use styx_runtime::sync::lock;
use styx_runtime::{Camera, CameraOptions, Error, RunError, serve_sync};
use styx_sensor::{ControlRequest, DriverState};

fn camera() -> (Camera<Mock>, Arc<MockReceiver>) {
    let receiver = Arc::new(MockReceiver::new(4, 4096));
    let camera = Camera::<Mock>::new(Arc::clone(&receiver), sensor(), CameraOptions::default());
    (camera, receiver)
}

#[test]
fn frames_flow_with_their_values_and_buffers_go_back() {
    let (mut camera, receiver) = camera();
    let (mut frames, configured) = camera.start(&config(3)).unwrap();
    assert_eq!(configured.buffers, 3);
    assert!(receiver.streaming());
    assert_eq!(
        lock(camera.sensor()).driver().state(),
        DriverState::Streaming
    );
    assert_eq!(receiver.queued(), [0, 1, 2]);
    let health = Arc::clone(camera.health().unwrap());
    let mut cx = noop();
    assert!(matches!(frames.poll_frame(&mut cx), Poll::Pending));
    for seq in 0..10u64 {
        receiver.frame(seq * 33_333_333);
        serve_sync(&*receiver, &**camera.sensor(), &health).unwrap();
        if seq == 2 {
            // 10 ms exposure for frame 5 (the OV9782's delay is two frames).
            let landed = lock(camera.sensor())
                .request_at(
                    5,
                    &ControlRequest {
                        exposure: Some(Duration::from_millis(10)),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(landed[0].frame, 5);
        }
        let Poll::Ready(Some(Ok(frame))) = frames.poll_frame(&mut cx) else {
            panic!("frame {seq}");
        };
        assert_eq!(frame.sequence, seq);
        assert_eq!(frame.data().len(), 4096);
        // CPU access started once.
        let _ = frame.data();
        assert_eq!(frame.buffer().syncs(), (1 + seq as u32 / 3, 0));
        let c = frame.controls.unwrap();
        if seq >= 5 {
            assert!(
                (c.exposure.as_secs_f64() - 0.010).abs() < 1e-4,
                "{seq}: {c:?}"
            );
        } else {
            assert!(
                (c.exposure.as_secs_f64() - 0.010).abs() > 1e-3,
                "{seq}: {c:?}"
            );
        }
        // Dropping it gives the buffer back.
        drop(frame);
        assert_eq!(receiver.queued().len(), 3);
    }
    assert_eq!(health.frame_syncs.get(), 10);
    assert_eq!(frames.stats().frames, 10);
    assert!(!frames.frame_sync_fallback());
    camera.stop().unwrap();
    assert!(!receiver.streaming());
    assert_eq!(lock(camera.sensor()).driver().state(), DriverState::Powered);
    assert!(matches!(frames.poll_frame(&mut cx), Poll::Ready(None)));
}

#[test]
fn frames_outlive_their_stream_and_never_go_back_to_a_newer_one() {
    let (mut camera, receiver) = camera();
    let (mut frames, _) = camera.start(&config(2)).unwrap();
    receiver.frame(0);
    let Poll::Ready(Some(Ok(held))) = frames.poll_frame(&mut noop()) else {
        panic!()
    };
    camera.stop().unwrap();
    let (_frames, _) = camera.start(&config(2)).unwrap();
    assert_eq!(receiver.queued(), [0, 1]);
    // The old frame is still readable, and dropping it queues nothing.
    assert_eq!(held.data().len(), 4096);
    drop(held);
    assert_eq!(receiver.queued(), [0, 1]);
    assert!(camera.start(&config(2)).is_err(), "already streaming");
    camera.shut_down().unwrap();
    assert_eq!(lock(camera.sensor()).driver().state(), DriverState::Off);
}

#[test]
fn missing_frame_starts_are_inferred_from_frames() {
    let (mut camera, receiver) = camera();
    let (mut frames, _) = camera.start(&config(2)).unwrap();
    for seq in 0..6 {
        receiver.frame(seq * 1000);
        // Nobody serves the frame starts.
        while receiver.try_sync().unwrap().is_some() {}
        let Poll::Ready(Some(Ok(_))) = frames.poll_frame(&mut noop()) else {
            panic!()
        };
    }
    assert!(frames.frame_sync_fallback());
    // The schedule saw frame starts from the dequeues (frame 6 is starting).
    assert_eq!(lock(camera.sensor()).current_frame(), Some(6));
}

#[test]
fn faults_end_the_stream_after_one_error() {
    let (mut camera, receiver) = camera();
    let (mut frames, _) = camera.start(&config(2)).unwrap();
    receiver.push_done(Err(ErrorKind::Disconnected));
    assert!(matches!(
        frames.poll_frame(&mut noop()),
        Poll::Ready(Some(Err(RunError::Runtime(Error::Disconnected))))
    ));
    assert!(frames.is_disconnected());
    assert!(matches!(frames.poll_frame(&mut noop()), Poll::Ready(None)));
    camera.stop().unwrap();

    // Corrupt frames in a row.
    let mut camera = Camera::<Mock>::new(
        Arc::clone(&receiver),
        sensor(),
        CameraOptions {
            frame_sync: true,
            max_error_frames: 2,
        },
    );
    let (mut frames, _) = camera.start(&config(2)).unwrap();
    for _ in 0..2 {
        receiver.push_done(Ok(styx_hal::FrameDone {
            index: 0,
            sequence: 0,
            timestamp: styx_hal::Instant(0),
            bytes_used: 0,
            corrupt: true,
            stats_slot: None,
        }));
        let _ = frames.poll_frame(&mut noop());
    }
    let Poll::Ready(Some(Err(RunError::Runtime(Error::Fault(f))))) = frames.poll_frame(&mut noop())
    else {
        panic!()
    };
    assert_eq!(f.kind, ErrorKind::Corrupt);
    // Glitches are counted, the stream goes on.
    receiver.push_sync(Ok(SyncEvent::Glitch(ErrorKind::Overrun)));
    serve_sync(&*receiver, &**camera.sensor(), camera.health().unwrap()).unwrap();
    assert_eq!(camera.health().unwrap().glitches.get(), 1);
}

#[test]
fn a_failed_start_leaves_the_sensor_in_standby_and_no_buffers() {
    let (mut camera, receiver) = camera();
    receiver.fail_start(ErrorKind::Timeout);
    let Err(RunError::Receiver(ErrorKind::Timeout)) = camera.start(&config(2)) else {
        panic!()
    };
    assert!(!camera.is_streaming());
    assert_ne!(
        lock(camera.sensor()).driver().state(),
        DriverState::Streaming
    );
    assert!(receiver.queue(0).is_err(), "released");
    // A start that fails in the sensor (the mock receiver starts it after arming).
    lock(camera.sensor()).driver_mut().bus_mut().values[0x300a] = 0;
    let (_frames, _) = camera.start(&config(2)).unwrap();
}
