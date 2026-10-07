//! What a preview frame costs: JPEG encode time and size per encoder, size and quality, from
//! planar YUV 4:2:0 (no colour conversion) and, for comparison, from NV12 (chroma split first)
//! and RGB (converted by the encoder); then the whole preview path (scale a 1280x800 NV12
//! camera frame, encode) on the preview thread, with its CPU time per frame.
//!
//! (mozjpeg and libjpeg-turbo do not link into one binary: mozjpeg runs without `preview`.)
//!
//! The picture is the C270 fixture's first frame (a real scene), stretched to 1280x800, as NV12.
//!
//! ```text
//! cargo run --release -p styx-examples --features preview,image --bin preview_encode_bench
//! cargo run --release -p styx-examples --features preview-only,codec-mozjpeg --bin preview_encode_bench
//! cargo run --release -p styx-examples --features preview --bin preview_encode_bench -- --frames 300
//! ```
//!
//! Each line: `preview_encode <encoder> <input> <WxH> q<quality> p50_ms=.. p95_ms=.. bytes=..`.

use std::time::{Duration, Instant};

use styx::codec::Codec;
use styx::codec::jpeg_planar::{JpegBackend, JpegInput, PlanarJpegEncoder};
use styx::prelude::*;
use styx::preview::{Preview, PreviewConfig};

const FIXTURE: &[u8] = include_bytes!("../../testing/fixtures/c270_720p_rst.mjpeg");

/// The fixture's first JPEG as RGB24, stretched to 1280x800 (the OV9782's size).
fn scene() -> (Vec<u8>, usize, usize) {
    let end = FIXTURE
        .windows(2)
        .position(|w| w == [0xFF, 0xD9])
        .map_or(FIXTURE.len(), |i| i + 2);
    let img = image::load_from_memory_with_format(&FIXTURE[..end], image::ImageFormat::Jpeg)
        .expect("fixture decodes")
        .to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let raw = img.into_raw();
    let tall = 800;
    let rgb = (0..tall)
        .flat_map(|y| raw[y * h / tall * w * 3..][..w * 3].iter().copied())
        .collect();
    (rgb, w, tall)
}

/// Full-range BT.601 RGB to planar 4:2:0 (y, u, v), and NV12 bytes.
fn yuv(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (
            f32::from(rgb[i]),
            f32::from(rgb[i + 1]),
            f32::from(rgb[i + 2]),
        )
    };
    let mut luma = vec![0; w * h];
    let (mut u, mut v) = (vec![0; w / 2 * (h / 2)], vec![0; w / 2 * (h / 2)]);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            luma[y * w + x] = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (r, g, b) = px(2 * x, 2 * y);
            u[y * (w / 2) + x] = (128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b).round() as u8;
            v[y * (w / 2) + x] = (128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b).round() as u8;
        }
    }
    (luma, u, v)
}

/// Box-downscale a plane by an integer factor.
fn shrink(plane: &[u8], w: usize, h: usize, f: usize) -> Vec<u8> {
    let (ow, oh) = (w / f, h / f);
    let mut out = vec![0; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            let mut sum = 0u32;
            for dy in 0..f {
                for dx in 0..f {
                    sum += u32::from(plane[(y * f + dy) * w + x * f + dx]);
                }
            }
            out[y * ow + x] = (sum / (f * f) as u32) as u8;
        }
    }
    out
}

fn percentile(samples: &mut [Duration], q: f64) -> f64 {
    samples.sort_unstable();
    let i = ((samples.len() - 1) as f64 * q).round() as usize;
    samples[i].as_secs_f64() * 1000.0
}

fn time(frames: usize, mut f: impl FnMut() -> usize) -> (f64, f64, usize) {
    let mut bytes = 0;
    for _ in 0..frames.min(10) {
        bytes = f();
    }
    let mut samples: Vec<Duration> = (0..frames)
        .map(|_| {
            let t = Instant::now();
            bytes = f();
            t.elapsed()
        })
        .collect();
    (
        percentile(&mut samples, 0.5),
        percentile(&mut samples, 0.95),
        bytes,
    )
}

fn frame_of(code: FourCc, w: usize, h: usize, bytes: &[u8]) -> FrameLease {
    let format = MediaFormat::new(
        code,
        Resolution::new(w as u32, h as u32).unwrap(),
        ColorSpace::Srgb,
    );
    FrameLease::from_visible_bytes(format, 0, bytes).unwrap()
}

fn main() {
    let frames: usize = std::env::args()
        .skip_while(|a| a != "--frames")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let (rgb, w, h) = scene();
    let (y, u, v) = yuv(&rgb, w, h);
    println!(
        "# scene {w}x{h} (C270 fixture), {frames} frames per line, encoders {:?}",
        JpegBackend::available()
    );

    for (factor, label) in [(2usize, "640x400"), (4, "320x200")] {
        let (sw, sh) = (w / factor, h / factor);
        let (sy, su, sv) = (
            shrink(&y, w, h, factor),
            shrink(&u, w / 2, h / 2, factor),
            shrink(&v, w / 2, h / 2, factor),
        );
        let planar = JpegInput::I420 {
            y: &sy,
            u: &su,
            v: &sv,
            width: sw,
            height: sh,
            y_stride: sw,
            c_stride: sw / 2,
        };
        let gray = JpegInput::Gray {
            y: &sy,
            width: sw,
            height: sh,
            stride: sw,
        };
        for quality in [60u8, 70, 75] {
            for backend in JpegBackend::available() {
                let mut encoder = PlanarJpegEncoder::new(backend, quality).unwrap();
                let mut out = Vec::new();
                for (input, name) in [(&planar, "i420"), (&gray, "grey")] {
                    let (p50, p95, bytes) = time(frames, || {
                        encoder.encode(input, &mut out).unwrap();
                        out.len()
                    });
                    println!(
                        "preview_encode {} {name} {label} q{quality} p50_ms={p50:.3} \
                         p95_ms={p95:.3} bytes={bytes}",
                        backend.name()
                    );
                }
            }
            // The codec registry's encoders, for comparison: NV12 (chroma split per frame) and
            // RGB (colour converted by libjpeg-turbo).
            #[cfg(feature = "codec-turbojpeg")]
            {
                use styx::codec::mjpeg_turbojpeg::TurbojpegEncoder;
                let mut nv12 = sy.clone();
                for (a, b) in su.iter().zip(&sv) {
                    nv12.extend([*a, *b]);
                }
                let srgb = shrink_rgb(&rgb, w, h, factor);
                for (code, bytes, name) in
                    [(FourCc::NV12, &nv12, "nv12"), (FourCc::RG24, &srgb, "rgb")]
                {
                    let codec = TurbojpegEncoder::new(code, i32::from(quality));
                    let frame = frame_of(code, sw, sh, bytes).into_shareable();
                    let (p50, p95, size) = time(frames, || {
                        let out = codec.process(frame.share().unwrap()).unwrap();
                        out.planes()[0].data().len()
                    });
                    println!(
                        "preview_encode turbojpeg-codec {name} {label} q{quality} \
                         p50_ms={p50:.3} p95_ms={p95:.3} bytes={size}"
                    );
                }
            }
        }
    }

    // The whole path on the preview thread: a 1280x800 NV12 (or RGB) frame scaled and encoded.
    let mut nv12 = y.clone();
    for (a, b) in u.iter().zip(&v) {
        nv12.extend([*a, *b]);
    }
    let nv12 = frame_of(FourCc::NV12, w, h, &nv12).into_shareable();
    let rgb = frame_of(FourCc::RG24, w, h, &rgb).into_shareable();
    for (camera, input, size, quality) in [
        (&nv12, "nv12", (640, 400), 70u8),
        (&nv12, "nv12", (320, 200), 70),
        (&nv12, "nv12", (640, 400), 60),
        (&rgb, "rgb", (640, 400), 70),
    ] {
        let preview = Preview::new(
            PreviewConfig::new()
                .size(size.0, size.1)
                .quality(quality)
                .max_fps(0.0)
                .name(format!("bench-{}x{}", size.0, size.1)),
        )
        .unwrap();
        let mut viewer = preview.subscribe();
        for _ in 0..frames + 10 {
            preview.offer(camera);
            viewer.recv(Duration::from_secs(5)).expect("encoded");
        }
        let m = preview.metrics();
        println!(
            "preview_path {} {input}-1280x800->{}x{} q{quality} scale_p50_ms={:.3} \
             encode_p50_ms={:.3} encode_p95_ms={:.3} bytes={} cpu_ms_per_frame={:.3}",
            m.encoder,
            m.size.0,
            m.size.1,
            m.scale.p50_ms.unwrap_or(0.0),
            m.encode.p50_ms.unwrap_or(0.0),
            m.encode.p95_ms.unwrap_or(0.0),
            m.bytes_per_frame.unwrap_or(0),
            m.cpu_ns as f64 / 1e6 / m.encoded.max(1) as f64,
        );
    }
}

#[cfg(feature = "codec-turbojpeg")]
fn shrink_rgb(rgb: &[u8], w: usize, h: usize, f: usize) -> Vec<u8> {
    let channel = |c: usize| {
        let plane: Vec<u8> = rgb.iter().skip(c).step_by(3).copied().collect();
        shrink(&plane, w, h, f)
    };
    let (r, g, b) = (channel(0), channel(1), channel(2));
    r.iter()
        .zip(&g)
        .zip(&b)
        .flat_map(|((r, g), b)| [*r, *g, *b])
        .collect()
}
