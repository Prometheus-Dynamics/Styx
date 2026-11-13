//! The DRM PRIME decode entry point through a software decoder: its frames are not DRM PRIME, so
//! the sink refuses them with the reason and holds nothing. Skips, printing why, when FFmpeg is
//! not installed. The hardware path (a decoder that outputs DRM PRIME) is not run here: see the
//! PR text.
#![cfg(all(feature = "ffmpeg", target_os = "linux"))]

use styx_codec_api::{CodecError, CodecKind, Implementation, Registry, Request};
use styx_format::{FourCc, MediaFormat, Resolution, plane_geometry};
use styx_frame::{BufferSize, FrameEntry, FramePool, PlaneLayout, packed_layouts};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

fn encoded_packet() -> Option<(FramePool, styx_frame::FrameLease)> {
    let mut registry: Registry = Registry::new();
    styx_ffmpeg::register(&mut registry);
    let mut encoder = match registry
        .select(Request::new(CodecKind::Encoder, FourCc::I420, FourCc::MJPG))
        .ok()?
        .into_implementation()
    {
        Implementation::Encoder(encoder) => encoder,
        _ => return None,
    };
    let res = Resolution::new(WIDTH, HEIGHT).unwrap();
    let raw_pool = FramePool::host(2, BufferSize::new(4 << 20, 0)).unwrap();
    let mut raw = raw_pool
        .acquire(FrameEntry::new(
            1,
            42,
            MediaFormat::with_defaults(FourCc::I420, res),
            packed_layouts(FourCc::I420, WIDTH, HEIGHT, None, None).unwrap(),
        ))
        .unwrap();
    let geometry = plane_geometry(FourCc::I420, WIDTH, HEIGHT).unwrap();
    {
        let mut planes = raw.write().unwrap();
        for i in 0..geometry.len() {
            let mut plane = planes.plane(i).unwrap();
            for row in 0..geometry[i].rows {
                let line = plane.row_bytes_mut(row, geometry[i].row_bytes).unwrap();
                line.fill(128 + i as u8 * 16);
            }
        }
    }
    let packet_pool = FramePool::host(2, BufferSize::new(1 << 20, 0)).unwrap();
    let mut packet = packet_pool
        .acquire(FrameEntry::new(
            0,
            0,
            MediaFormat::with_defaults(FourCc::MJPG, res),
            styx_frame::Layouts::new(&[PlaneLayout::new(0, 0, 1 << 20)]).unwrap(),
        ))
        .unwrap();
    encoder.encode(&raw, &mut packet).ok()?;
    drop(raw);
    Some((packet_pool, packet))
}

#[test]
fn software_output_is_refused_as_drm_prime() {
    if let Err(why) = styx_ffmpeg::available() {
        eprintln!("skipped software_output_is_refused_as_drm_prime: {why}");
        return;
    }
    let Some((_packet_pool, packet)) = encoded_packet() else {
        eprintln!("skipped software_output_is_refused_as_drm_prime: no MJPEG encoder");
        return;
    };
    let mut decoder = styx_ffmpeg::open_decoder(FourCc::MJPG, FourCc::I420).expect("decoder");
    let payload = 64 * 48 * 3 / 2;
    let mut sink = decoder.prime_sink::<()>(payload, 2, false).expect("sink");

    let refused = decoder
        .decode_prime(&packet, &mut sink)
        .expect_err("a software frame is not DRM PRIME");
    match &refused {
        CodecError::Rejected(why) => assert!(why.contains("not DRM PRIME"), "{why}"),
        other => panic!("expected a rejection, got {other}"),
    }
    eprintln!("refused as expected: {refused}");
    assert_eq!(sink.held(), 0);
    assert_eq!(sink.buffers(), 0);
    assert_eq!(sink.stats().acquired, 0);
}
