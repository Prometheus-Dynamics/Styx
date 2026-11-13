//! The `vivid` test driver's capture node, on a machine that has one (a node whose card is
//! `vivid`). Without one, each test prints why it skipped and passes.
//!
//! The nodes are chosen from sysfs and probed one by one; a virtual node (OBS's loopback camera on
//! the dev PC) is never opened. The tests share one lock: they use the same device, and the copy
//! test reads a process-wide counter.

#![cfg(target_os = "linux")]

mod common;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use styx_controls::{Control, ControlValue};
use styx_format::FourCc;
use styx_lemnos::v4l2::v4l2::{BufType, VideoDevice};
use styx_v4l2::{
    Delivery, Device, Queue, Sizes, State, Stream, StreamConfig, copy_counters, probe_node,
};

/// vivid's "Disconnect" button, in its "Vivid Controls" class.
const DISCONNECT: u32 = 0x00f0_f041;
/// vivid's "OSD Text Mode" menu: 2 turns the counters off, so the picture stands still.
const OSD_TEXT_MODE: u32 = 0x00f0_f001;
const OSD_DISABLED: i64 = 2;
/// V4L2 `V4L2_CID_BRIGHTNESS`.
const BRIGHTNESS: u32 = 0x0098_0900;

/// The tests on the vivid node run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The first vivid capture node, probed. `None` when the machine has none.
fn vivid() -> Option<Device> {
    for path in common::physical_nodes() {
        let Ok(Some(device)) = probe_node(&path) else {
            continue;
        };
        if device.card.to_ascii_lowercase().contains("vivid") && device.queue == Queue::Single {
            return Some(device);
        }
    }
    None
}

/// A YUYV stream on the node: the first discrete size it lists for YUYV, or its first size.
fn yuyv_config(device: &Device) -> Option<StreamConfig> {
    let code = FourCc::YUYV;
    let format = device.formats.iter().find(|f| f.code == code)?;
    let Sizes::Discrete(sizes) = &format.sizes else {
        return None;
    };
    let size = sizes.first()?;
    Some(StreamConfig::new(code, size.width, size.height))
}

fn skip(test: &str, why: &str) {
    println!("SKIP {test}: {why}");
}

#[test]
fn vivid_controls_are_each_mapped_or_passthrough() {
    let _guard = serial();
    let Some(device) = vivid() else {
        return skip("controls", "no vivid capture node on this machine");
    };
    let mapped: Vec<_> = device
        .raw_controls
        .iter()
        .filter(|c| c.mapped.is_some())
        .collect();
    let passthrough: Vec<_> = device
        .raw_controls
        .iter()
        .filter(|c| c.mapped.is_none())
        .collect();
    println!(
        "vivid {} at {}: {} controls, {} mapped to Styx controls, {} passthrough",
        device.card,
        device.path.display(),
        device.raw_controls.len(),
        mapped.len(),
        passthrough.len()
    );
    for c in &mapped {
        println!(
            "  mapped      {:<28} id={:#010x} -> {}",
            c.name,
            c.id,
            c.mapped.unwrap().name()
        );
    }
    assert_eq!(
        mapped.len() + passthrough.len(),
        device.raw_controls.len(),
        "every control is one or the other"
    );
    assert_eq!(
        device.raw_controls.len(),
        77,
        "vivid reports 77 controls on this PC"
    );
    assert!(mapped.iter().any(|c| c.mapped == Some(Control::Brightness)));
    assert!(mapped.iter().any(|c| c.mapped == Some(Control::Hue)));
    assert!(mapped.iter().any(|c| c.mapped == Some(Control::HFlip)));
    assert!(passthrough.iter().any(|c| c.id == DISCONNECT));
}

#[test]
fn a_mapped_control_reaches_the_device_and_the_frame_metadata() {
    let _guard = serial();
    let Some(device) = vivid() else {
        return skip("brightness", "no vivid capture node on this machine");
    };
    let Some(mut config) = yuyv_config(&device) else {
        return skip("brightness", "vivid lists no discrete YUYV size");
    };
    config.controls = vec![(Control::Brightness, ControlValue::Manual(750))];
    let stream = Stream::start(&device.path, config).expect("start");

    let frame = stream
        .frames()
        .recv_timeout(Duration::from_secs(3))
        .expect("a frame");
    assert_eq!(
        frame.meta().control(Control::Brightness),
        ControlValue::Manual(750)
    );

    // vivid's brightness is 0..=255: 750 of 1000 is 191.25, which rounds to 191.
    let raw = stream
        .get_device_control(BRIGHTNESS)
        .expect("read brightness");
    println!("brightness: styx 750 -> device {raw} (0..=255)");
    assert_eq!(raw, 191);

    // A mapped control is refused on the passthrough: its Styx control keeps the metadata true.
    let refused = stream.set_device_control(BRIGHTNESS, 10).unwrap_err();
    println!("set_device_control(brightness): {refused}");
    assert!(matches!(refused, styx_v4l2::Error::DeviceValue(_)));

    // A passthrough control is set and read back by id.
    stream
        .set_device_control(OSD_TEXT_MODE, OSD_DISABLED)
        .expect("set osd");
    assert_eq!(
        stream.get_device_control(OSD_TEXT_MODE).expect("read osd"),
        OSD_DISABLED
    );
    let passthrough = stream.passthrough().count();
    println!("this stream lists {} passthrough controls", passthrough);
    stream.stop();
}

/// Frames are copied out of the mapped buffers: one copy per frame, of the frame's bytes, and the
/// copied bytes equal what the mapped buffer holds. The picture is held still first (no OSD
/// counters, no movement), so the buffer's bytes are the same whenever they are read.
#[test]
fn the_mapped_copy_path_copies_each_frame_once_and_matches_the_buffer() {
    let _guard = serial();
    let Some(device) = vivid() else {
        return skip("mmap copy", "no vivid capture node on this machine");
    };
    let Some(mut config) = yuyv_config(&device) else {
        return skip("mmap copy", "vivid lists no discrete YUYV size");
    };
    config.copy_mapped = true;
    let (width, height) = (config.width, config.height);
    // The baseline is taken before the stream starts: the stream's whole life is measured.
    let before = copy_counters()[0];
    let (copies0, bytes0) = (before.copies(), before.bytes());
    let stream = Stream::start(&device.path, config).expect("start");
    let status = stream.status();
    println!("delivery={:?}: {}", status.delivery, status.delivery_note);
    assert_eq!(status.delivery, Delivery::Copied, "copy was forced");
    stream
        .set_device_control(OSD_TEXT_MODE, OSD_DISABLED)
        .expect("hold the picture still");
    // Frames already queued still carry the counters: drain them before the measured run.
    let settle = Instant::now() + Duration::from_millis(400);
    while Instant::now() < settle {
        let _ = stream.frames().recv_timeout(Duration::from_millis(50));
    }

    // A second, independent view of the same buffers: its mappings are read after the frames.
    let view = VideoDevice::open(&device.path).expect("second handle on the node");
    let mut mapped_buffers = Vec::new();
    for index in 0..16u32 {
        match view.map_buffer(BufType::VideoCapture, index) {
            Ok(planes) => {
                if let Some(first) = planes.into_iter().next() {
                    mapped_buffers.push(first);
                }
            }
            Err(_) => break,
        }
    }
    println!(
        "mapped {} buffers through a second handle",
        mapped_buffers.len()
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frames = 0usize;
    let mut span = 0usize;
    let mut matched = 0usize;
    while Instant::now() < deadline {
        let Ok(frame) = stream.frames().recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        frames += 1;
        span = frame.layouts().span().expect("a span");
        let read = frame.read().expect("a copied frame can be read");
        let bytes = &read.bytes()[..span];
        // Each mapped buffer holds the same still picture: compare against any of them.
        let same = mapped_buffers
            .iter()
            .any(|m| m.as_slice().len() >= span && &m.as_slice()[..span] == bytes);
        if same {
            matched += 1;
        }
    }
    // Finishing joins the worker: every copy made is settled in the counter from here on.
    let status = stream.finish();
    let after = copy_counters()[0];
    let copies = after.copies() - copies0;
    let bytes = after.bytes() - bytes0;
    println!(
        "{width}x{height}: received={frames} sent={} span={span} bytes/frame; copies={copies} \
         bytes={bytes}; received frames matching a mapped buffer={matched}; dropped={}",
        status.frames, status.dropped
    );

    assert!(frames > 10, "frames flowed: {frames}");
    assert_eq!(
        status.dropped, 0,
        "the consumer kept up, so every frame was counted"
    );
    assert!(
        frames as u64 <= status.frames,
        "the consumer cannot receive more than were sent"
    );
    assert_eq!(copies as u64, status.frames, "one copy per frame sent");
    assert_eq!(
        bytes as u64,
        status.frames * span as u64,
        "each frame's bytes counted once"
    );
    assert_eq!(
        matched, frames,
        "every copied frame matches a mapped buffer"
    );
}

#[test]
fn a_disconnected_device_retries_with_capped_backoff_and_comes_back() {
    let _guard = serial();
    let Some(device) = vivid() else {
        return skip("device loss", "no vivid capture node on this machine");
    };
    let Some(config) = yuyv_config(&device) else {
        return skip("device loss", "vivid lists no discrete YUYV size");
    };
    // vivid's Disconnect faults the stream (QBUF fails with ENODEV) and the node opens again at
    // once, so the first retry (250 ms) succeeds. The backoff cap is not reached on this device;
    // its sequence (250 ms doubling to 5 s) is checked by the unit test `backoff_doubles...`.
    let stream = Stream::start(&device.path, config).expect("start");
    let mut frames = 0;
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        if stream
            .frames()
            .recv_timeout(Duration::from_millis(100))
            .is_ok()
        {
            frames += 1;
        }
    }
    println!("before: {frames} frames, state {:?}", stream.status().state);

    let disconnect = stream
        .passthrough()
        .find(|c| c.id == DISCONNECT)
        .map(|c| (c.name.clone(), c.device_control().kind));
    println!("passthrough Disconnect: {disconnect:?}");
    stream
        .set_device_control(DISCONNECT, 1)
        .expect("press Disconnect");

    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(20);
    let mut back = false;
    while Instant::now() < end {
        let status = stream.status();
        let line = format!("{:?}", status.state);
        if seen.last() != Some(&line) {
            println!("state: {line}");
            seen.push(line);
        }
        if let State::Retrying {
            backoff, attempt, ..
        } = status.state
        {
            assert!(
                backoff <= Duration::from_secs(5),
                "backoff {backoff:?} at attempt {attempt}"
            );
        }
        if status.state == State::Streaming && status.reconnects > 0 {
            back = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut after = 0;
    let recover = Instant::now() + Duration::from_secs(3);
    while Instant::now() < recover && back {
        if stream
            .frames()
            .recv_timeout(Duration::from_millis(100))
            .is_ok()
        {
            after += 1;
        }
    }
    let status = stream.status();
    println!(
        "after: reconnects={} frames after recovery={after} state={:?} control_errors={}",
        status.reconnects, status.state, status.control_errors
    );
    stream.stop();
    assert!(
        back,
        "the stream came back to Streaming within 20 s: {seen:?}"
    );
    assert!(after > 0, "frames flowed again after the reconnect");
    assert!(
        seen.iter().any(|s| s.starts_with("Retrying")),
        "it went through Retrying: {seen:?}"
    );
}
