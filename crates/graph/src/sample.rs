//! Example graphs of real hardware, for tests, docs and tools that want a reference shape.
//!
//! Formats and costs are illustrative: providers build the real graphs from the kernel's media
//! controller and the sensor description.

use crate::caps::{
    BusCode, Capabilities, CostHint, FormatCaps, FourCc, Fraction, IntervalRange, MemoryDomains,
    Size, SizeRange,
};
use crate::graph::{
    DeviceGraph, LinkFlags, Node, NodeKind, Port, PortPurpose, PortRef, ReceiverKind,
};

fn bus(code: BusCode) -> Capabilities {
    Capabilities::new().format(FormatCaps::new(code))
}

fn memory(codes: &[FourCc], sizes: SizeRange) -> Capabilities {
    codes
        .iter()
        .fold(Capabilities::new(), |caps, code| {
            caps.format(FormatCaps::new(*code).size(sizes))
        })
        .memory(MemoryDomains::CPU | MemoryDomains::DMABUF)
}

fn port(g: &DeviceGraph, node: &str, port: &str) -> PortRef {
    g.find_port(node, port)
        .unwrap_or_else(|| panic!("sample graph has no {node}:{port}"))
}

/// A Raspberry Pi CM5 with an OV9782 on CSI-2, through the PiSP front end and back end:
///
/// ```text
/// ov9782 -> csi2 -+-> csi2_ch0                      (raw frames to memory)
///                 +-> pisp-fe -+-> fe_stats         (statistics)
///                              +-> fe_image0 ==memory==> pispbe -+-> be_output0
///                                                               +-> be_output1
/// ```
pub fn cm5_ov9782() -> DeviceGraph {
    let raw_bus = BusCode::SGRBG10_1X10;
    let full = SizeRange::stepwise(Size::new(16, 16), Size::new(1280, 800), Size::new(2, 2));
    let scaled = SizeRange::stepwise(Size::new(64, 64), Size::new(4096, 4096), Size::new(2, 2));
    let raw_mem = [FourCc::SGRBG10, FourCc::PISP_COMP1_RGGB];
    let processed = [FourCc::NV12, FourCc::YU12, FourCc::RGB3, FourCc::GREY];

    let mut g = DeviceGraph::new();
    g.add_node(
        Node::new("ov9782", NodeKind::Sensor)
            .device("/dev/v4l-subdev2")
            .property("bridge", "styx-sensor-bridge")
            .port(Port::output(
                "out",
                Capabilities::new().format(
                    FormatCaps::new(raw_bus)
                        .size(SizeRange::discrete(1280, 800))
                        .size(SizeRange::discrete(1280, 720))
                        .size(SizeRange::discrete(640, 400))
                        .interval(IntervalRange::fps(1, 120)),
                ),
            )),
    );
    g.add_node(
        Node::new("csi2", NodeKind::Receiver(ReceiverKind::Csi2))
            .device("/dev/v4l-subdev0")
            .port(Port::input("sink", bus(raw_bus)))
            .port(Port::output("ch0", bus(raw_bus)))
            .port(Port::output("fe", bus(raw_bus))),
    );
    g.add_node(
        Node::new("csi2_ch0", NodeKind::Sink)
            .device("/dev/video0")
            .port(Port::input("in", memory(&raw_mem, full)))
            .port(Port::output("out", memory(&raw_mem, full))),
    );
    g.add_node(
        Node::new("pisp-fe", NodeKind::IspStage)
            .device("/dev/v4l-subdev1")
            .cost(CostHint::fixed(1.0, 0.0))
            .port(Port::input("sink", bus(raw_bus)))
            .port(Port::input("config", Capabilities::new()).purpose(PortPurpose::Params))
            .port(Port::output("image0", bus(raw_bus)))
            .port(Port::output("stats", Capabilities::new()).purpose(PortPurpose::Stats)),
    );
    g.add_node(
        Node::new("fe_image0", NodeKind::Sink)
            .device("/dev/video4")
            .port(Port::input("in", memory(&raw_mem, full)))
            .port(Port::output("out", memory(&raw_mem, full))),
    );
    g.add_node(
        Node::new("fe_stats", NodeKind::Sink)
            .device("/dev/video6")
            .port(
                Port::input(
                    "in",
                    Capabilities::new()
                        .format(FormatCaps::new(FourCc::PISP_FE_STATS))
                        .memory(MemoryDomains::CPU),
                )
                .purpose(PortPurpose::Stats),
            ),
    );
    g.add_node(
        Node::new("pispbe", NodeKind::IspStage)
            .property("driver", "pispbe")
            .cost(CostHint {
                fixed: crate::Cost::new(1.0, 0.2),
                per_megapixel: crate::Cost::new(5.5, 0.0),
            })
            .port(Port::input("input", memory(&raw_mem, full)))
            .port(
                Port::input(
                    "config",
                    Capabilities::new()
                        .format(FormatCaps::new(FourCc::PISP_BE_CONFIG))
                        .memory(MemoryDomains::CPU),
                )
                .purpose(PortPurpose::Params),
            )
            .port(Port::output("output0", memory(&processed, scaled)))
            .port(Port::output("output1", memory(&processed, scaled))),
    );
    for (name, dev) in [
        ("be_output0", "/dev/video20"),
        ("be_output1", "/dev/video21"),
    ] {
        g.add_node(
            Node::new(name, NodeKind::Sink)
                .device(dev)
                .port(Port::input("in", memory(&processed, scaled))),
        );
    }

    let links = [
        (("ov9782", "out"), ("csi2", "sink"), LinkFlags::IMMUTABLE),
        (("csi2", "ch0"), ("csi2_ch0", "in"), LinkFlags::ENABLED),
        (("csi2", "fe"), ("pisp-fe", "sink"), LinkFlags::ENABLED),
        (
            ("pisp-fe", "image0"),
            ("fe_image0", "in"),
            LinkFlags::ENABLED,
        ),
        (("pisp-fe", "stats"), ("fe_stats", "in"), LinkFlags::ENABLED),
        (
            ("pispbe", "output0"),
            ("be_output0", "in"),
            LinkFlags::ENABLED,
        ),
        (
            ("pispbe", "output1"),
            ("be_output1", "in"),
            LinkFlags::ENABLED,
        ),
    ];
    for ((a, ap), (b, bp), flags) in links {
        let (from, to) = (port(&g, a, ap), port(&g, b, bp));
        g.link(from, to, flags).expect("sample link");
    }
    let (from, to) = (port(&g, "fe_image0", "out"), port(&g, "pispbe", "input"));
    g.memory_link(from, to, LinkFlags::DISABLED)
        .expect("sample link");
    g
}

/// A USB (UVC) camera: the camera, the USB video function and one capture node offering MJPEG
/// and YUYV.
pub fn uvc_camera() -> DeviceGraph {
    let mut g = DeviceGraph::new();
    let sensor = g.add_node(
        Node::new("uvc-sensor", NodeKind::Sensor).port(Port::output("out", bus(BusCode::FIXED))),
    );
    let usb = g.add_node(
        Node::new("uvc", NodeKind::Receiver(ReceiverKind::Usb))
            .property("usb", "046d:0825")
            // Exposure, encoding and transfer: about 0.8 frame intervals (see planner/cost.rs).
            .cost(CostHint::fixed(33.0, 0.0))
            .port(Port::input("in", bus(BusCode::FIXED)))
            .port(Port::output("out", bus(BusCode::FIXED))),
    );
    let fps = |f| IntervalRange::Discrete(Fraction::from_fps(f));
    let video = g.add_node(
        Node::new("video0", NodeKind::Sink)
            .device("/dev/video0")
            .port(Port::input(
                "in",
                Capabilities::new()
                    .format(
                        FormatCaps::new(FourCc::MJPG)
                            .size(SizeRange::discrete(1280, 720))
                            .size(SizeRange::discrete(640, 480))
                            .interval(fps(30))
                            .interval(fps(15)),
                    )
                    .format(
                        FormatCaps::new(FourCc::YUYV)
                            .size(SizeRange::discrete(640, 480))
                            .interval(fps(30)),
                    )
                    .memory(MemoryDomains::CPU | MemoryDomains::DMABUF),
            )),
    );
    g.link(sensor.port(0), usb.port(0), LinkFlags::IMMUTABLE)
        .expect("sample link");
    g.link(usb.port(1), video.port(0), LinkFlags::IMMUTABLE)
        .expect("sample link");
    g
}
