use super::*;

/// Logitech C270 (046d:0825), recorded from sysfs on the CM5.
const C270: &[u8] = include_bytes!("../tests/data/c270.bin");

#[test]
fn parses_the_c270() {
    let f = UvcFunction::parse(C270).unwrap();
    assert_eq!((f.device.vendor_id, f.device.product_id), (0x046d, 0x0825));
    assert_eq!(f.configuration, 1);
    let c = &f.control;
    assert_eq!((c.number, c.uvc_version), (0, 0x0100));
    assert_eq!(c.clock_frequency, 48_000_000);
    assert_eq!(c.status_endpoint, Some(0x87));
    assert_eq!(c.camera_terminal(), Some((1, 0x0e)));
    assert_eq!(c.processing_unit(), Some((2, 0x175b)));
    let xus = c
        .entities
        .iter()
        .filter(|e| matches!(e, Entity::ExtensionUnit { .. }))
        .count();
    assert_eq!(xus, 4);

    assert_eq!(f.streaming.len(), 1);
    let vs = &f.streaming[0];
    assert_eq!((vs.number, vs.endpoint, vs.terminal_link), (1, 0x81, 5));
    assert_eq!(vs.transfer(), TransferType::Isochronous);
    assert_eq!(vs.alts.len(), 12);
    assert!(vs.alts[0].endpoints.is_empty());
    // Alt 11: 1020 bytes × 3 transactions per microframe.
    assert_eq!(vs.alts[11].endpoints[0].bytes_per_interval(), 3060);

    let yuyv = vs.format(1).unwrap();
    assert_eq!(yuyv.fourcc(), Some(*b"YUYV"));
    assert_eq!(yuyv.frames.len(), 19);
    let vga = yuyv.frame(1).unwrap();
    assert_eq!(
        (vga.width, vga.height, vga.max_frame_size),
        (640, 480, 614_400)
    );
    assert_eq!(yuyv.frame_bytes(vga), Some(614_400));
    assert_eq!(
        vga.intervals,
        Intervals::Discrete(vec![
            333_333, 400_000, 500_000, 666_666, 1_000_000, 2_000_000
        ])
    );
    assert_eq!(
        yuyv.color,
        Some(ColorFormat {
            primaries: 1,
            transfer: 1,
            matrix: 4
        })
    );
    // 1280x720 YUYV only reaches 7.5 fps; MJPEG does 30.
    assert_eq!(
        yuyv.frame(18).unwrap().intervals.closest(333_333),
        Some(1_333_333)
    );
    let mjpg = vs.format(2).unwrap();
    assert_eq!(mjpg.fourcc(), Some(*b"MJPG"));
    let hd = mjpg.frame(18).unwrap();
    assert_eq!((hd.width, hd.height), (1280, 720));
    assert_eq!(hd.intervals.closest(333_333), Some(333_333));
    assert_eq!(mjpg.frame_bytes(hd), None);
}

#[test]
fn picks_the_smallest_alt_that_carries_the_payload() {
    let f = UvcFunction::parse(C270).unwrap();
    let vs = &f.streaming[0];
    let pick = |n| {
        vs.alt_for_bandwidth(n)
            .map(|(a, e)| (a, e.bytes_per_interval()))
    };
    assert_eq!(pick(100), Some((1, 192)));
    assert_eq!(pick(800), Some((5, 800)));
    // 640x480 YUYV at 30 fps needs 2304 bytes per microframe.
    assert_eq!(pick(2304), Some((10, 2688)));
    // More than any alt: the largest.
    assert_eq!(pick(10_000), Some((11, 3060)));
}

#[test]
fn finds_modes() {
    let f = UvcFunction::parse(C270).unwrap();
    let m = crate::find_mode(&f, *b"MJPG", 1280, 720, Some(333_333)).unwrap();
    assert_eq!(
        (m.interface, m.format_index, m.frame_index, m.interval),
        (1, 2, 18, 333_333)
    );
    let m = crate::find_mode(&f, *b"YUYV", 640, 480, None).unwrap();
    assert_eq!((m.format_index, m.frame_index, m.interval), (1, 1, 333_333));
    // The closest supported rate.
    let m = crate::find_mode(&f, *b"YUYV", 640, 480, Some(350_000)).unwrap();
    assert_eq!(m.interval, 333_333);
    assert!(crate::find_mode(&f, *b"NV12", 640, 480, None).is_none());
    assert!(crate::find_mode(&f, *b"YUYV", 641, 480, None).is_none());
}

/// Builds a descriptor set: a UVC 1.5 SuperSpeed camera with an NV12 format (continuous
/// intervals), an H.264 frame-based format, a bulk streaming endpoint, and an IAD.
fn uvc15_bulk() -> Vec<u8> {
    let mut d = Vec::new();
    let mut push = |b: &[u8]| {
        d.push(b.len() as u8 + 1);
        d.extend_from_slice(b);
    };
    // Device: USB 3.2, vendor 0x1234, product 0x5678.
    push(&[
        1, 0x20, 0x03, 0xef, 2, 1, 9, 0x34, 0x12, 0x78, 0x56, 0, 1, 1, 2, 3, 1,
    ]);
    // Configuration 1 (the total length is not checked by the parser).
    push(&[2, 0, 0, 2, 1, 0, 0x80, 50]);
    // IAD: video function, interfaces 0-1.
    push(&[0x0b, 0, 2, 0x0e, 0x03, 0, 0]);
    // VC interface 0.
    push(&[4, 0, 0, 1, 0x0e, 0x01, 0x01, 0]);
    // VC header: UVC 1.5, clock 1 MHz, one streaming interface.
    push(&[0x24, 0x01, 0x50, 0x01, 0, 0, 0x40, 0x42, 0x0f, 0, 1, 1]);
    // Camera terminal 1: exposure (bit 3) and focus auto (bit 17).
    push(&[
        0x24, 0x02, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0x08, 0x00, 0x02,
    ]);
    // Processing unit 2: brightness, gain.
    push(&[0x24, 0x05, 2, 1, 0, 0, 2, 0x01, 0x02, 0, 0, 0]);
    // Output terminal 3 from 2.
    push(&[0x24, 0x03, 3, 0x01, 0x01, 0, 2, 0]);
    // VS interface 1, alt 0, one bulk endpoint.
    push(&[4, 1, 0, 1, 0x0e, 0x02, 0x01, 0]);
    // VS input header: 2 formats, endpoint 0x82, terminal 3.
    push(&[0x24, 0x01, 2, 0, 0, 0x82, 0, 3, 0, 0, 0, 1, 0, 0]);
    // NV12 uncompressed format 1, one frame.
    let mut fmt = vec![0x24, 0x04, 1, 1];
    fmt.extend_from_slice(b"NV12\x00\x00\x10\x00\x80\x00\x00\xaa\x00\x38\x9b\x71");
    fmt.extend_from_slice(&[12, 1, 0, 0, 0, 0]);
    push(&fmt);
    // 1920x1080 NV12, continuous intervals 1/60 s to 1/5 s in steps of 1/600 s.
    let mut fr = vec![0x24, 0x05, 1, 0];
    fr.extend_from_slice(&1920u16.to_le_bytes());
    fr.extend_from_slice(&1080u16.to_le_bytes());
    fr.extend_from_slice(&[0; 8]);
    fr.extend_from_slice(&(1920u32 * 1080 * 3 / 2).to_le_bytes());
    fr.extend_from_slice(&333_333u32.to_le_bytes());
    fr.push(0);
    for v in [166_666u32, 2_000_000, 16_666] {
        fr.extend_from_slice(&v.to_le_bytes());
    }
    push(&fr);
    // H.264 frame-based format 2, variable size, one frame.
    let mut fb = vec![0x24, 0x10, 2, 1];
    fb.extend_from_slice(b"H264\x00\x00\x10\x00\x80\x00\x00\xaa\x00\x38\x9b\x71");
    fb.extend_from_slice(&[16, 1, 0, 0, 0, 0, 1]);
    push(&fb);
    let mut ff = vec![0x24, 0x11, 1, 0];
    ff.extend_from_slice(&1280u16.to_le_bytes());
    ff.extend_from_slice(&720u16.to_le_bytes());
    ff.extend_from_slice(&[0; 8]);
    ff.extend_from_slice(&333_333u32.to_le_bytes());
    ff.push(2);
    ff.extend_from_slice(&0u32.to_le_bytes());
    for v in [333_333u32, 666_666] {
        ff.extend_from_slice(&v.to_le_bytes());
    }
    push(&ff);
    // Colour matching: BT.709.
    push(&[0x24, 0x0d, 1, 1, 4]);
    // Bulk IN endpoint 0x82, 1024 bytes, SuperSpeed companion with a burst of 16.
    push(&[5, 0x82, 0x02, 0x00, 0x04, 0]);
    push(&[0x30, 15, 0, 0, 0]);
    d
}

#[test]
fn parses_a_uvc15_bulk_camera() {
    let f = UvcFunction::parse(&uvc15_bulk()).unwrap();
    assert_eq!(f.device.bcd_usb, 0x0320);
    assert_eq!(f.control.uvc_version, 0x0150);
    assert_eq!(f.control.clock_frequency, 1_000_000);
    assert_eq!(f.control.camera_terminal(), Some((1, 0x0002_0008)));
    assert_eq!(f.control.processing_unit(), Some((2, 0x0201)));
    let vs = &f.streaming[0];
    assert_eq!(vs.transfer(), TransferType::Bulk);
    assert_eq!(vs.alt_for_bandwidth(1000), None);
    let ep = vs.alts[0].endpoints[0];
    assert_eq!(
        (ep.address, ep.max_packet, ep.transactions),
        (0x82, 1024, 16)
    );
    let nv12 = vs.format(1).unwrap();
    assert_eq!(nv12.fourcc(), Some(*b"NV12"));
    let fr = nv12.frame(1).unwrap();
    assert_eq!(nv12.frame_bytes(fr), Some(1920 * 1080 * 3 / 2));
    assert_eq!(
        fr.intervals,
        Intervals::Continuous {
            min: 166_666,
            max: 2_000_000,
            step: 16_666
        }
    );
    assert_eq!(fr.intervals.closest(333_333), Some(333_326));
    assert_eq!(fr.intervals.closest(1), Some(166_666));
    assert_eq!(
        fr.intervals.listed(333_333),
        vec![166_666, 333_333, 2_000_000]
    );
    let h264 = vs.format(2).unwrap();
    assert_eq!(h264.fourcc(), Some(*b"H264"));
    assert!(matches!(
        h264.kind,
        FormatKind::FrameBased {
            variable_size: true,
            ..
        }
    ));
    assert_eq!(
        h264.frame(1).unwrap().intervals,
        Intervals::Discrete(vec![333_333, 666_666])
    );
    assert_eq!(h264.color.unwrap().matrix, 4);
}

#[test]
fn rejects_broken_descriptors() {
    // Truncated in the middle of a descriptor.
    assert!(matches!(
        UvcFunction::parse(&C270[..C270.len() - 3]),
        Err(UvcError::Descriptor(_))
    ));
    // A zero-length descriptor would loop forever.
    assert!(UvcFunction::parse(&[0, 0, 0]).is_err());
    // A device that is not a camera.
    assert!(UvcFunction::parse(&C270[..18]).is_err());
}

#[test]
fn continuous_intervals_from_a_broken_device_do_not_panic() {
    // min above max (clamp panicked) and steps that overflow u32 arithmetic.
    let backwards = Intervals::Continuous {
        min: 666_666,
        max: 333_333,
        step: 1,
    };
    assert_eq!(backwards.closest(0), Some(333_333));
    assert_eq!(backwards.closest(u32::MAX), Some(666_666));
    let huge = Intervals::Continuous {
        min: 0,
        max: u32::MAX,
        step: u32::MAX,
    };
    assert_eq!(huge.closest(u32::MAX - 1), Some(u32::MAX));
    let zero = Intervals::Continuous {
        min: 0,
        max: 0,
        step: 0,
    };
    assert_eq!(zero.closest(333_333), Some(0));
}
