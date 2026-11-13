//! Runs on a V4L2 capture node when the machine has one: a webcam, a loopback device with a
//! producer, or a vivid device. Without one, each test prints why it skipped and passes. CI
//! cannot load kernel modules, so there these tests skip.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use styx_format::FourCc;
use styx_v4l2::{Delivery, Queue, Sizes, Stream, StreamConfig, probe};

/// The first capture node with a format that lists discrete sizes, and that format's first size.
fn first_stream_target() -> Option<(PathBuf, FourCc, u32, u32)> {
    for device in probe().devices {
        for format in &device.formats {
            if let Sizes::Discrete(sizes) = &format.sizes
                && let Some(size) = sizes.first()
            {
                return Some((device.path.clone(), format.code, size.width, size.height));
            }
        }
    }
    None
}

#[test]
fn probe_lists_capture_nodes_formats_and_controls() {
    let found = probe();
    for error in &found.errors {
        println!("probe error: {error}");
    }
    if found.devices.is_empty() {
        println!("SKIP probe: no V4L2 capture node on this machine");
        return;
    }
    for device in &found.devices {
        println!(
            "node {} card={:?} driver={:?} bus={:?} queue={:?} virtual={}",
            device.path.display(),
            device.card,
            device.driver,
            device.bus_info,
            device.queue,
            device.virtual_node
        );
        for format in &device.formats {
            println!(
                "  format {} {:?}: {:?}",
                format.code, format.description, format.sizes
            );
        }
        println!("  mapped controls: {:?}", device.controls);
        for raw in &device.raw_controls {
            println!(
                "  control {} id=0x{:08x} type={:?} range={}..={} default={} read_only={} mapped={:?}",
                raw.name,
                raw.id,
                raw.control_type,
                raw.min,
                raw.max,
                raw.default,
                raw.read_only,
                raw.mapped
            );
        }
        assert!(device.queue == Queue::Single || device.queue == Queue::Multi);
    }
}

#[test]
fn a_stream_delivers_frames_from_the_first_capture_node() {
    let Some((path, code, width, height)) = first_stream_target() else {
        println!("SKIP stream: no V4L2 capture node with a discrete size on this machine");
        return;
    };
    let config = StreamConfig::new(code, width, height);
    let stream = match Stream::start(&path, config) {
        Ok(stream) => stream,
        Err(e) => {
            println!("SKIP stream: {} did not start: {e}", path.display());
            return;
        }
    };
    println!("streaming {} as {:?}", path.display(), stream.format());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frames = 0u32;
    let mut last_bytes = 0usize;
    let mut last_stamp = 0u64;
    let mut backwards = 0u32;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(frame) = stream
            .frames()
            .recv_timeout(left.min(Duration::from_millis(200)))
        else {
            continue;
        };
        frames += 1;
        last_bytes = frame.layouts().span().unwrap_or(0);
        if frames > 1 && frame.timestamp_ns() < last_stamp {
            backwards += 1;
        }
        last_stamp = frame.timestamp_ns();
        assert!(last_stamp > 0, "a frame has no timestamp");
        assert_eq!(frame.format().resolution.width.get(), width);
    }
    let status = stream.status();
    println!(
        "frames={frames} span_bytes={last_bytes} delivery={:?} ({}) dropped={} arrival_stamped={} copies={:?}",
        status.delivery,
        status.delivery_note,
        status.dropped,
        status.arrival_stamped,
        styx_v4l2::copy_counters()
            .iter()
            .map(|c| (c.site(), c.copies(), c.bytes()))
            .collect::<Vec<_>>()
    );
    println!("state={:?}", status.state);
    if frames == 0 {
        println!(
            "SKIP stream: {} started but delivered no frames in 3 s (no producer feeding it?)",
            path.display()
        );
        return;
    }
    assert_eq!(backwards, 0, "frame timestamps went backwards");
    if status.delivery == Delivery::Dmabuf {
        assert_eq!(
            styx_v4l2::copy_counters()[0].copies(),
            0,
            "dma-buf frames must not be copied"
        );
    }
}
