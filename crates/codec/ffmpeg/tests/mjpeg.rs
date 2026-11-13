//! MJPEG through FFmpeg: an encode and a decode with the libraries present. Skips, printing why,
//! when FFmpeg is not installed.
#![cfg(all(feature = "ffmpeg", target_os = "linux"))]

use styx_codec_api::{CodecKind, Implementation, Registry, Request};
use styx_format::{FourCc, MediaFormat, Resolution, plane_geometry};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, PlaneLayout, packed_layouts};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

/// A raw I420 frame with a smooth gradient in Y and flat chroma, written through the lease.
fn raw_frame(pool: &FramePool, id: u64, timestamp_ns: u64) -> FrameLease {
    let res = Resolution::new(WIDTH, HEIGHT).unwrap();
    let layouts = packed_layouts(FourCc::I420, WIDTH, HEIGHT, None, None).unwrap();
    let mut lease = pool
        .acquire(FrameEntry::new(
            id,
            timestamp_ns,
            MediaFormat::with_defaults(FourCc::I420, res),
            layouts,
        ))
        .unwrap();
    let geometry = plane_geometry(FourCc::I420, WIDTH, HEIGHT).unwrap();
    let mut planes = lease.write().unwrap();
    let mut i = 0;
    while let Some(shape) = geometry.get(i) {
        let mut plane = planes.plane(i).unwrap();
        for row in 0..shape.rows {
            let line = plane.row_bytes_mut(row, shape.row_bytes).unwrap();
            for (col, byte) in line.iter_mut().enumerate() {
                *byte = if i == 0 {
                    (col * 3 + row * 2) as u8
                } else {
                    128
                };
            }
        }
        i += 1;
    }
    lease
}

fn pool(payload: usize) -> FramePool {
    FramePool::host(4, BufferSize::new(payload, 0)).unwrap()
}

#[test]
fn mjpeg_round_trip_through_ffmpeg() {
    if let Err(why) = styx_ffmpeg::available() {
        eprintln!("skipped mjpeg_round_trip_through_ffmpeg: {why}");
        return;
    }
    let mut registry: Registry = Registry::new();
    styx_ffmpeg::register(&mut registry);

    let mut encoder = match registry
        .select(Request::new(CodecKind::Encoder, FourCc::I420, FourCc::MJPG))
        .expect("FFmpeg MJPEG encoder")
        .into_implementation()
    {
        Implementation::Encoder(encoder) => encoder,
        other => panic!("expected an encoder, got {other:?}"),
    };
    let mut decoder = match registry
        .select(Request::new(CodecKind::Decoder, FourCc::MJPG, FourCc::I420))
        .expect("FFmpeg MJPEG decoder")
        .into_implementation()
    {
        Implementation::Decoder(decoder) => decoder,
        other => panic!("expected a decoder, got {other:?}"),
    };

    let raw_pool = pool(4 * 1024 * 1024);
    let input = raw_frame(&raw_pool, 7, 123_456_789);
    let res = Resolution::new(WIDTH, HEIGHT).unwrap();

    // Encode into a caller-acquired compressed buffer.
    let packet_pool = FramePool::host(2, BufferSize::new(1 << 20, 0)).unwrap();
    let mut packet = packet_pool
        .acquire(FrameEntry::new(
            0,
            0,
            MediaFormat::with_defaults(FourCc::MJPG, res),
            styx_frame::Layouts::new(&[PlaneLayout::new(0, 0, 1 << 20)]).unwrap(),
        ))
        .unwrap();
    encoder.encode(&input, &mut packet).expect("encode");
    let packet_len = packet.layouts()[0].len;
    eprintln!("MJPEG packet: {packet_len} bytes for {WIDTH}x{HEIGHT} I420");
    assert!(packet_len > 0);
    assert_eq!(packet.id(), 7);
    assert_eq!(packet.timestamp_ns(), 123_456_789);

    // Decode it back into a caller-acquired raw frame.
    let out_pool = pool(4 * 1024 * 1024);
    let mut output = out_pool
        .acquire(FrameEntry::new(
            0,
            0,
            MediaFormat::with_defaults(FourCc::I420, res),
            packed_layouts(FourCc::I420, WIDTH, HEIGHT, None, None).unwrap(),
        ))
        .unwrap();
    decoder.decode(&packet, &mut output).expect("decode");
    assert_eq!(output.id(), 7);
    assert_eq!(
        output.timestamp_ns(),
        123_456_789,
        "pts survives the round trip"
    );
    assert_eq!(output.format().code, FourCc::I420);

    // Lossy codec: the decoded luma must be close to the source.
    let src = input.read().unwrap();
    let dst = output.read().unwrap();
    let (a, b) = (src.plane(0).unwrap(), dst.plane(0).unwrap());
    let mut total = 0u64;
    for row in 0..HEIGHT as usize {
        let x = a.row_bytes(row, WIDTH as usize).unwrap();
        let y = b.row_bytes(row, WIDTH as usize).unwrap();
        total += x
            .iter()
            .zip(y)
            .map(|(p, q)| p.abs_diff(*q) as u64)
            .sum::<u64>();
    }
    let mean = total as f64 / (WIDTH as f64 * HEIGHT as f64);
    eprintln!("decoded luma mean absolute difference: {mean:.2}");
    assert!(mean < 6.0, "mean difference {mean}");

    let counters = styx_ffmpeg::copy_counters();
    eprintln!(
        "copies into FFmpeg: {} ({} bytes); out of FFmpeg: {} ({} bytes)",
        counters[0].copies(),
        counters[0].bytes(),
        counters[1].copies(),
        counters[1].bytes()
    );
}
