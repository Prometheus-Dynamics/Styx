//! Tests against real devices. Each test skips cleanly when the device kind is absent.
//!
//! Everything here is read-only except the streaming test, which only runs when
//! `STYX_KERNEL_STREAM_DEVICE=/dev/videoN` names a capture device that is free to use.

#![cfg(target_os = "linux")]

use std::time::Duration;

use styx_kernel::event::{EventKind, EventType, Events, SubscribeFlags};
use styx_kernel::media::{LinkType, MediaDevice};
use styx_kernel::subdev::{Subdev, Which};
use styx_kernel::v4l2::{
    BufType, ControlWhich, Controls, Format, FrameSizes, Memory, QueueBuffer, VideoDevice,
    list_video_nodes,
};
use styx_kernel::{media, subdev};

fn tolerable(e: &styx_kernel::Error) -> bool {
    e.is_not_supported() || e.is_invalid_argument()
}

#[test]
fn video_nodes_answer_queries() {
    let nodes = list_video_nodes();
    if nodes.is_empty() {
        eprintln!("no /dev/video*; skipping");
        return;
    }
    for path in nodes {
        let dev = match VideoDevice::open_read_only(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{}: {e}; skipping", path.display());
                continue;
            }
        };
        let caps = dev.capabilities();
        assert!(
            !caps.driver.is_empty(),
            "{}: empty driver name",
            path.display()
        );
        for ty in caps.buffer_types() {
            let formats = dev
                .formats(ty)
                .unwrap_or_else(|e| panic!("{}: {ty:?}: {e}", path.display()));
            match dev.format(ty) {
                Ok(f) => assert_eq!(matches!(f, Format::Multi(_)), ty.is_multiplanar()),
                Err(e) => assert!(tolerable(&e), "{}: G_FMT {e}", path.display()),
            }
            if let Some(first) = formats.first().filter(|_| !ty.is_meta()) {
                match dev.frame_sizes(first.fourcc) {
                    Ok(FrameSizes::Discrete(sizes)) => {
                        if let Some(s) = sizes.first() {
                            let _ = dev.frame_intervals(first.fourcc, s.width, s.height);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => assert!(tolerable(&e), "{}: sizes {e}", path.display()),
                }
            }
        }
        let controls = dev.query_controls().expect("query controls");
        let readable: Vec<_> = controls.into_iter().filter(|c| c.is_readable()).collect();
        for c in &readable {
            let _ = dev.get_controls_for(ControlWhich::Current, std::slice::from_ref(c));
        }
    }
}

#[test]
fn media_devices_report_a_consistent_graph() {
    let devices = media::list_media_devices();
    if devices.is_empty() {
        eprintln!("no /dev/media*; skipping");
        return;
    }
    for path in devices {
        let dev = match MediaDevice::open_read_only(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{}: {e}; skipping", path.display());
                continue;
            }
        };
        let info = dev.device_info().expect("device info");
        assert!(!info.driver.is_empty());
        let topo = dev.topology().expect("topology");
        for link in &topo.links {
            match link.flags.link_type() {
                LinkType::Data => {
                    assert!(topo.pad(link.source_id).is_some(), "dangling source pad");
                    assert!(topo.pad(link.sink_id).is_some(), "dangling sink pad");
                }
                LinkType::Interface => {
                    assert!(topo.interfaces.iter().any(|i| i.id == link.source_id));
                    assert!(topo.entity(link.sink_id).is_some());
                }
                _ => {}
            }
        }
        for pad in &topo.pads {
            assert!(topo.entity(pad.entity_id).is_some(), "pad without entity");
        }
        for intf in &topo.interfaces {
            if let Some(node) = intf.devnode {
                let p = node.path().expect("devnode in sysfs");
                assert!(p.exists(), "{} missing", p.display());
            }
        }
    }
}

#[test]
fn subdevs_answer_queries() {
    let nodes = subdev::list_subdev_nodes();
    if nodes.is_empty() {
        eprintln!("no /dev/v4l-subdev*; skipping");
        return;
    }
    for path in nodes {
        let sd = match Subdev::open_read_only(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}; skipping", path.display());
                continue;
            }
        };
        match sd.format(0, Which::Active) {
            Ok(f) => {
                assert!(f.width > 0 && f.height > 0);
                if let Ok(codes) = sd.mbus_codes(0, Which::Active) {
                    for code in codes {
                        sd.frame_sizes(0, code, Which::Active).expect("frame sizes");
                    }
                }
            }
            Err(e) => assert!(tolerable(&e), "{}: G_FMT {e}", path.display()),
        }
        sd.query_controls().expect("query controls");
    }
}

#[test]
fn control_events_deliver_the_initial_value() {
    // Subscribing is local to our file handle; it changes nothing on the device.
    for path in list_video_nodes()
        .into_iter()
        .chain(subdev::list_subdev_nodes())
    {
        let dev = match VideoDevice::open_read_only(&path) {
            Ok(d) => Box::new(d) as Box<dyn Probe>,
            Err(_) => match Subdev::open_read_only(&path) {
                Ok(s) => Box::new(s),
                Err(_) => continue,
            },
        };
        let Some(ctrl) = dev
            .controls()
            .into_iter()
            .find(|c| c.is_readable() && !c.has_payload())
        else {
            continue;
        };
        if let Err(e) =
            dev.events()
                .subscribe(EventType::Ctrl, ctrl.id, SubscribeFlags::SEND_INITIAL)
        {
            assert!(tolerable(&e), "{}: subscribe {e}", path.display());
            continue;
        }
        let ev = dev
            .events()
            .wait_event(Some(Duration::from_secs(1)))
            .expect("wait event")
            .expect("initial control event");
        assert_eq!(ev.id, ctrl.id);
        assert!(matches!(ev.kind, EventKind::Ctrl(_)), "{:?}", ev.kind);
        assert!(dev.events().dequeue_event().expect("dequeue").is_none());
        dev.events().unsubscribe_all().expect("unsubscribe");
        return;
    }
    eprintln!("no device with readable controls and events; skipping");
}

trait Probe {
    fn controls(&self) -> Vec<styx_kernel::v4l2::ControlInfo>;
    fn events(&self) -> &dyn Events;
}

impl Probe for VideoDevice {
    fn controls(&self) -> Vec<styx_kernel::v4l2::ControlInfo> {
        self.query_controls().unwrap_or_default()
    }
    fn events(&self) -> &dyn Events {
        self
    }
}

impl Probe for Subdev {
    fn controls(&self) -> Vec<styx_kernel::v4l2::ControlInfo> {
        self.query_controls().unwrap_or_default()
    }
    fn events(&self) -> &dyn Events {
        self
    }
}

/// Streams a few frames with MMAP buffers, maps and exports them. Opt-in only.
#[test]
fn streams_from_an_opted_in_capture_device() {
    let Ok(path) = std::env::var("STYX_KERNEL_STREAM_DEVICE") else {
        eprintln!("STYX_KERNEL_STREAM_DEVICE not set; skipping");
        return;
    };
    let dev = VideoDevice::open(&path).expect("open");
    let ty = dev
        .capabilities()
        .buffer_types()
        .into_iter()
        .find(|t| matches!(t, BufType::VideoCapture | BufType::VideoCaptureMplane))
        .expect("a capture queue");
    let fmt = dev.format(ty).expect("format");
    let bufs = dev.request_buffers(ty, Memory::Mmap, 4).expect("reqbufs");
    assert!(bufs.count >= 1);
    let maps: Vec<_> = (0..bufs.count)
        .map(|i| dev.map_buffer(ty, i).expect("map"))
        .collect();
    let dmabuf = dev.export_buffer(ty, 0, 0);
    eprintln!("expbuf: {:?}", dmabuf.as_ref().map(|_| "ok"));
    for i in 0..bufs.count {
        dev.queue(&QueueBuffer::mmap(ty, i)).expect("qbuf");
    }
    assert!(
        dev.dequeue(ty, Memory::Mmap).expect("dqbuf").is_none(),
        "nothing before STREAMON"
    );
    dev.stream_on(ty).expect("streamon");
    let mut frames = Vec::new();
    while frames.len() < 5 {
        let ready = dev.wait(Some(Duration::from_secs(3))).expect("poll");
        assert!(ready.readable, "timed out waiting for a frame");
        if let Some(buf) = dev.dequeue(ty, Memory::Mmap).expect("dqbuf") {
            assert!(buf.bytes_used() > 0);
            assert!(maps[buf.index as usize][0].len() >= buf.planes[0].0 as usize);
            frames.push((buf.sequence, buf.timestamp));
            dev.queue(&QueueBuffer::mmap(ty, buf.index))
                .expect("requeue");
        }
    }
    dev.stream_off(ty).expect("streamoff");
    drop(maps);
    drop(dmabuf);
    dev.free_buffers(ty, Memory::Mmap).expect("free");
    eprintln!("format {fmt:?}");
    for w in frames.windows(2) {
        assert!(w[1].0 > w[0].0, "sequence increases");
        assert!(w[1].1 > w[0].1, "timestamp increases");
        eprintln!("seq {} dt {:?}", w[1].0, w[1].1 - w[0].1);
    }
}
