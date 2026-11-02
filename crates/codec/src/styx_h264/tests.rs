//! The encoder against FFmpeg's decoder: every picture decodes to exactly the encoder's own
//! reconstruction (no deblocking, so any syntax or prediction mismatch shows), over sizes
//! that crop, QPs, I and P pictures, motion and the three inputs.

use std::io::Write as _;
use std::process::{Command, Stdio};

use super::picture::Plane;
use super::*;

/// A plane's samples as rows.
pub(super) fn plane_rows(p: &Plane, (w, h): (usize, usize)) -> Vec<u8> {
    (0..h)
        .flat_map(|y| p.data[y * p.w..][..w].iter().copied())
        .collect()
}

/// `w` x `h` planes of a scene: gradients, a texture and a square moving by `shift` pixels.
pub(super) fn scene(w: usize, h: usize, shift: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut s = 0x9e37_79b9u32;
    let mut noise = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s >> 28) as usize
    };
    let y = (0..w * h)
        .map(|i| {
            let (x, r) = (i % w, i / w);
            let inside = (x + w - shift % w) % w < w / 3 && (r + shift) % h < h / 2;
            let base = if inside {
                200
            } else {
                (x * 255 / w + r * 64 / h) % 256
            };
            (base + noise()).min(255) as u8
        })
        .collect();
    let (cw, ch) = (w / 2, h / 2);
    let u = (0..cw * ch)
        .map(|i| (64 + (i % cw) * 128 / cw) as u8)
        .collect();
    let v = (0..cw * ch)
        .map(|i| (192 - (i / cw) * 128 / ch) as u8)
        .collect();
    (y, u, v)
}

/// FFmpeg's decode of `stream` as I420 pictures, or `None` without an `ffmpeg` binary.
pub(super) fn ffmpeg_decode(stream: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-v", "error", "-f", "h264", "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "yuv420p",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let data = stream.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(out.stdout)
}

/// Encodes `frames` pictures of the scene, `step` pixels of motion each, and checks the
/// decode against the reconstructions; returns the bytes per picture.
fn round_trip(
    w: usize,
    h: usize,
    qp: u8,
    gop: u32,
    frames: usize,
    step: usize,
    kind: u8,
) -> Vec<usize> {
    let mut enc = StyxH264::new(qp, gop);
    let (mut stream, mut recon, mut sizes) = (Vec::new(), Vec::new(), Vec::new());
    let mut out = Vec::new();
    for f in 0..frames {
        let (y, u, v) = scene(w, h, f * step);
        let uv: Vec<u8> = u.iter().zip(&v).flat_map(|(&a, &b)| [a, b]).collect();
        let input = match kind {
            0 => StyxH264Input::I420 {
                y: &y,
                u: &u,
                v: &v,
                width: w,
                height: h,
                y_stride: w,
                c_stride: w / 2,
            },
            1 => StyxH264Input::Nv12 {
                y: &y,
                uv: &uv,
                width: w,
                height: h,
                y_stride: w,
                uv_stride: w,
            },
            _ => StyxH264Input::Gray {
                y: &y,
                width: w,
                height: h,
                stride: w,
            },
        };
        let idr = enc.encode(&input, &mut out).unwrap();
        assert_eq!(idr, (f as u32).is_multiple_of(gop), "frame {f}");
        stream.extend_from_slice(&out);
        sizes.push(out.len());
        let r = &enc.reference;
        recon.extend(plane_rows(&r.y, (w, h)));
        recon.extend(plane_rows(&r.u, (w / 2, h / 2)));
        recon.extend(plane_rows(&r.v, (w / 2, h / 2)));
    }
    let Some(decoded) = ffmpeg_decode(&stream) else {
        return sizes;
    };
    assert_eq!(decoded.len(), recon.len(), "{w}x{h} qp {qp}: picture count");
    let pic = w * h * 3 / 2;
    for (f, (a, b)) in decoded.chunks(pic).zip(recon.chunks(pic)).enumerate() {
        let first = a.iter().zip(b).position(|(p, q)| p != q);
        assert!(
            first.is_none(),
            "{w}x{h} qp {qp} kind {kind}: picture {f} differs at byte {first:?}"
        );
    }
    sizes
}

#[test]
fn intra_pictures_decode_to_the_reconstruction() {
    for (w, h) in [(16, 16), (34, 18), (64, 48), (176, 144)] {
        for qp in [4, 26, 45] {
            round_trip(w, h, qp, 1, 3, 2, 0);
        }
    }
}

#[test]
fn p_pictures_decode_to_the_reconstruction() {
    for (w, h) in [(48, 32), (176, 144), (130, 66)] {
        for qp in [12, 28, 40] {
            let sizes = round_trip(w, h, qp, 8, 10, 3, 0);
            // P pictures of a moving scene cost less than its I pictures.
            let p = sizes[1..8].iter().sum::<usize>() / 7;
            assert!(p < sizes[0], "{w}x{h} qp {qp}: {sizes:?}");
        }
    }
}

#[test]
fn still_scenes_skip() {
    let sizes = round_trip(176, 144, 26, 30, 5, 0, 0);
    // The first P picture refines what the I picture lost; after it, a still scene is skips
    // only: a few bytes.
    assert!(sizes[1] < sizes[0] / 10, "{sizes:?}");
    assert!(sizes[2..].iter().all(|&s| s < 32), "{sizes:?}");
}

#[test]
fn nv12_and_grey_inputs() {
    round_trip(96, 64, 22, 4, 6, 2, 1);
    round_trip(96, 64, 22, 4, 6, 2, 2);
}

#[test]
fn quality_tracks_qp() {
    let (w, h) = (176, 144);
    let (y, u, v) = scene(w, h, 0);
    let input = StyxH264Input::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: w / 2,
    };
    let mut last = f64::INFINITY;
    for qp in [10, 22, 34, 46] {
        let mut enc = StyxH264::new(qp, 1);
        let mut out = Vec::new();
        enc.encode(&input, &mut out).unwrap();
        let rec = plane_rows(&enc.reference.y, (w, h));
        let mse = y
            .iter()
            .zip(&rec)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / (w * h) as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        assert!(psnr < last, "qp {qp}: {psnr:.1} dB");
        if qp == 22 {
            assert!(psnr > 35.0, "qp 22: {psnr:.1} dB");
        }
        last = psnr;
    }
}

#[test]
fn bad_sizes_are_refused() {
    let y = [0u8; 64];
    let mut enc = StyxH264::new(26, 1);
    let mut out = Vec::new();
    for (w, h) in [(7, 8), (8, 7), (0, 8)] {
        let input = StyxH264Input::Gray {
            y: &y,
            width: w,
            height: h,
            stride: 8,
        };
        assert!(enc.encode(&input, &mut out).is_err(), "{w}x{h}");
    }
}
