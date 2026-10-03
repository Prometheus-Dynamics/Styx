//! What stills are delivered as: JPEG encoding (the codec registry, a given encoder, or the
//! `image` crate), NV12 ↔ RGB, DNG previews, and stills from a capture's own frames.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use styx_capture::prelude::*;
use styx_codec::{Codec, CodecRegistry, CodecRegistryHandle};

use super::handle::CaptureHandle;
use super::request::CaptureError;
use super::still::{
    StillCapture, StillExposure, StillFormat, StillImage, StillMeta, StillRequest, StillShot,
};

fn err(msg: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("still: {msg}"))
}

/// The process-wide registry with the enabled codecs, built on first use.
fn registry() -> Option<&'static CodecRegistryHandle> {
    static REGISTRY: OnceLock<Option<CodecRegistryHandle>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            CodecRegistry::with_enabled_codecs()
                .ok()
                .map(|r| r.handle())
        })
        .as_ref()
}

/// A frame's planes, visible rows only, one after the other.
pub(crate) fn packed(frame: &FrameLease) -> Result<Vec<u8>, CaptureError> {
    let mut out = Vec::new();
    for plane in frame.planes_visible().map_err(err)? {
        for row in plane.iter() {
            out.extend_from_slice(row.data());
        }
    }
    Ok(out)
}

fn frame_of(code: FourCc, w: u32, h: u32, bytes: &[u8]) -> Result<FrameLease, CaptureError> {
    let res = Resolution::new(w, h).ok_or_else(|| err("empty image"))?;
    FrameLease::from_visible_bytes(MediaFormat::new(code, res, ColorSpace::Srgb), 0, bytes)
        .map_err(err)
}

/// RGB24 (tightly packed) as a JPEG file: `encoder`, else the registry's RG24 → MJPG encoder,
/// else (feature `image`) the `image` crate's.
pub(crate) fn encode_jpeg(
    rgb: &[u8],
    (w, h): (u32, u32),
    quality: u8,
    encoder: Option<&Arc<dyn Codec>>,
) -> Result<Vec<u8>, CaptureError> {
    let codec = encoder
        .cloned()
        .or_else(|| registry().and_then(|r| r.lookup_for_output(FourCc::RG24, FourCc::MJPG).ok()));
    if let Some(c) = codec {
        let out = c.process(frame_of(FourCc::RG24, w, h, rgb)?).map_err(err)?;
        let planes = out.planes();
        return planes
            .first()
            .map(|p| p.data().to_vec())
            .ok_or_else(|| err("encoder gave no data"));
    }
    #[cfg(feature = "image")]
    {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
            .encode(rgb, w, h, image::ExtendedColorType::Rgb8)
            .map_err(err)?;
        Ok(out)
    }
    #[cfg(not(feature = "image"))]
    {
        let _ = quality;
        Err(err(
            "no JPEG encoder: enable codec-mozjpeg, codec-turbojpeg, codec-ffmpeg or image, \
             or pass StillRequest::encoder",
        ))
    }
}

/// Full-range BT.601 NV12 (as the PiSP and the software ISP make it) to RGB24.
pub(crate) fn nv12_to_rgb(nv12: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    let (y, uv) = nv12.split_at((w * h).min(nv12.len()));
    for r in 0..h {
        for c in 0..w {
            let yy = f32::from(y.get(r * w + c).copied().unwrap_or(0));
            let i = (r / 2) * w + (c & !1);
            let u = f32::from(uv.get(i).copied().unwrap_or(128)) - 128.0;
            let v = f32::from(uv.get(i + 1).copied().unwrap_or(128)) - 128.0;
            let px = &mut out[(r * w + c) * 3..][..3];
            px[0] = (yy + 1.402 * v).round().clamp(0.0, 255.0) as u8;
            px[1] = (yy - 0.344_136 * u - 0.714_136 * v)
                .round()
                .clamp(0.0, 255.0) as u8;
            px[2] = (yy + 1.772 * u).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// RGB24 to full-range BT.601 NV12 (even sizes).
pub(crate) fn rgb_to_nv12(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![128u8; w * h + w * h.div_ceil(2)];
    let px = |r: usize, c: usize| {
        let p = &rgb[(r * w + c) * 3..][..3];
        (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]))
    };
    for r in 0..h {
        for c in 0..w {
            let (rr, g, b) = px(r, c);
            out[r * w + c] = (0.299 * rr + 0.587 * g + 0.114 * b)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
    }
    for r in (0..h).step_by(2) {
        for c in (0..w).step_by(2) {
            let (rr, g, b) = px(r, c);
            let i = w * h + (r / 2) * w + c;
            out[i] = (128.0 - 0.168_736 * rr - 0.331_264 * g + 0.5 * b)
                .round()
                .clamp(0.0, 255.0) as u8;
            if c + 1 < w {
                out[i + 1] = (128.0 + 0.5 * rr - 0.418_688 * g - 0.081_312 * b)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// A thumbnail of RGB24 at most `max_w` wide (box filter over whole factors).
pub(crate) fn thumbnail(rgb: &[u8], w: usize, h: usize, max_w: usize) -> (Vec<u8>, u32, u32) {
    let f = w.div_ceil(max_w.max(1)).max(1);
    let (tw, th) = (w / f, h / f);
    let mut out = Vec::with_capacity(tw * th * 3);
    for ty in 0..th {
        for tx in 0..tw {
            for ch in 0..3 {
                let mut sum = 0u32;
                for dy in 0..f {
                    for dx in 0..f {
                        sum += u32::from(rgb[((ty * f + dy) * w + tx * f + dx) * 3 + ch]);
                    }
                }
                out.push((sum / (f * f) as u32) as u8);
            }
        }
    }
    (out, tw as u32, th as u32)
}

/// The 16-bit Bayer code of a 2x2 pattern (`RG`, `BG`, `GR`, `GB` + bits).
pub(crate) fn bayer16_code(colors: [u8; 4], bits: u8) -> FourCc {
    let p = match colors {
        [0, 1, 1, 2] => b"RG",
        [2, 1, 1, 0] => b"BG",
        [1, 0, 2, 1] => b"GR",
        _ => b"GB",
    };
    let d = format!("{:02}", bits.clamp(8, 16));
    let d = d.as_bytes();
    FourCc::new([p[0], p[1], d[0], d[1]])
}

/// A still from the capture's own next frame (captures without a still path).
pub(crate) fn from_stream(
    handle: &CaptureHandle,
    request: &StillRequest,
    started: Instant,
) -> Result<StillCapture, CaptureError> {
    if request.exposure != StillExposure::Current {
        return Err(err(
            "fixed and bracketed exposures need a processed native camera",
        ));
    }
    let deadline = started + request.timeout;
    let frame = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(err("no frame before the timeout"));
        }
        match handle.recv_blocking(left.min(Duration::from_millis(200))) {
            RecvOutcome::Data(f) => break f,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => return Err(err("the capture closed")),
        }
    };
    let t = Instant::now();
    let fmt = frame.meta().format;
    let (w, h) = (fmt.resolution.width.get(), fmt.resolution.height.get());
    let rgb = |frame: FrameLease| -> Result<Vec<u8>, CaptureError> {
        match fmt.code {
            FourCc::RG24 => packed(&frame),
            FourCc::NV12 => Ok(nv12_to_rgb(&packed(&frame)?, w as usize, h as usize)),
            code => {
                let c = registry()
                    .and_then(|r| r.lookup_for_output(code, FourCc::RG24).ok())
                    .ok_or_else(|| err(format!("no converter from {code} to RG24")))?;
                packed(&c.process(frame).map_err(err)?)
            }
        }
    };
    let mut meta = StillMeta {
        sequence: frame.meta().sequence().map_or(0, u64::from),
        timestamp_ns: frame.meta().timestamp,
        isp: "stream",
        landed: true,
        ..StillMeta::default()
    };
    if let Some(n) = frame.meta().native() {
        meta.exposure = Duration::from_nanos(n.exposure_ns);
        meta.analogue_gain = f64::from(n.analog_gain);
        meta.sensor_digital_gain = f64::from(n.digital_gain);
        meta.verified = n.verified;
    }
    let (data, code) = match request.format {
        StillFormat::Jpeg { quality } => match fmt.code {
            FourCc::MJPG | FourCc::JPEG => (packed(&frame)?, FourCc::MJPG),
            _ => (
                encode_jpeg(&rgb(frame)?, (w, h), quality, request.encoder.as_ref())?,
                FourCc::MJPG,
            ),
        },
        StillFormat::Rgb24 => (rgb(frame)?, FourCc::RG24),
        StillFormat::Nv12 => match fmt.code {
            FourCc::NV12 => (packed(&frame)?, FourCc::NV12),
            _ => (
                rgb_to_nv12(&rgb(frame)?, w as usize, h as usize),
                FourCc::NV12,
            ),
        },
        StillFormat::Raw => (packed(&frame)?, fmt.code),
    };
    meta.process_time = t.elapsed();
    Ok(StillCapture {
        shots: vec![StillShot {
            image: Some(StillImage {
                format: code,
                width: w,
                height: h,
                data,
            }),
            dng: None,
            meta,
        }],
        latency: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_conversions_round_trip() {
        let (w, h) = (4, 2);
        let rgb: Vec<u8> = [
            [200u8, 40, 40],
            [40, 200, 40],
            [40, 40, 200],
            [128, 128, 128],
        ]
        .iter()
        .cycle()
        .take(w * h)
        .flatten()
        .copied()
        .collect();
        // Flat 2x2 blocks survive the chroma subsampling.
        let flat: Vec<u8> = (0..w * h).flat_map(|_| [180u8, 90, 30]).collect();
        let back = nv12_to_rgb(&rgb_to_nv12(&flat, w, h), w, h);
        assert!(
            back.iter().zip(&flat).all(|(a, b)| a.abs_diff(*b) <= 2),
            "{back:?}"
        );
        assert_eq!(rgb_to_nv12(&rgb, w, h).len(), w * h * 3 / 2);
        let (t, tw, th) = thumbnail(&flat, 4, 2, 2);
        assert_eq!((tw, th, t.len()), (2, 1, 6));
        assert_eq!(&t[..3], &[180, 90, 30]);
        assert_eq!(bayer16_code([2, 1, 1, 0], 10), FourCc::new(*b"BG10"));
        assert_eq!(bayer16_code([0, 1, 1, 2], 16), FourCc::new(*b"RG16"));
    }

    #[test]
    fn stills_from_a_virtual_capture() {
        let device = crate::capture_api::CaptureRequest::virtual_source(
            crate::prelude::VirtualSourceConfig::new()
                .resolution(64, 32)
                .fps(30),
        )
        .into_device();
        let handle = crate::capture_api::CaptureRequest::new(&device)
            .start()
            .unwrap();
        let still = handle
            .capture_still(&StillRequest::format(StillFormat::Rgb24))
            .unwrap();
        let img = still.shots[0].image.as_ref().unwrap();
        assert_eq!(
            (img.width, img.height, img.data.len()),
            (64, 32, 64 * 32 * 3)
        );
        assert_eq!(still.shots[0].meta.isp, "stream");
        assert!(
            handle
                .capture_still(&StillRequest::default().bracket([0.0, 1.0]))
                .is_err()
        );
        handle.stop();
    }
}
