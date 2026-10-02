//! Caps <-> Styx formats: which GStreamer caps a Styx camera can produce (asking the Styx
//! planner for every mode and output format) and the [`FrameRequirements`] for negotiated caps.

use std::collections::BTreeMap;

use gst::prelude::*;
use gst_video::VideoFormat;
use styx::planner::plan_frames_with;
use styx::prelude::*;

/// What a fixed caps structure asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction; `None` for variable (`0/1`) or unspecified.
    pub fps: Option<(i32, i32)>,
    /// `video/x-raw(memory:DMABuf)`: buffers must be dma-bufs.
    pub dmabuf: bool,
    /// DRM fourcc and modifier when negotiated as `DMA_DRM`.
    pub drm: Option<(u32, u64)>,
}

impl Negotiated {
    /// One frame's duration, when the rate is known.
    pub fn frame_duration(&self) -> Option<gst::ClockTime> {
        let (num, den) = self.fps?;
        if num <= 0 || den <= 0 {
            return None;
        }
        gst::ClockTime::SECOND.mul_div_floor(den as u64, num as u64)
    }

    /// Requirements that pin this size and format; the planner picks mode and route.
    pub fn requirements(&self, exact_size: bool) -> FrameRequirements {
        let mut req =
            FrameRequirements::formats([self.fourcc]).output_resolution(self.width, self.height);
        if exact_size {
            req = req
                .min_resolution(self.width, self.height)
                .max_resolution(self.width, self.height);
        }
        if let Some((num, den)) = self.fps
            && num > 0
            && den > 0
        {
            // Round down so 30000/1001 asks for 29 fps and the 29.97 interval qualifies.
            req = req.min_fps((num / den).max(1) as u32);
        }
        req
    }
}

/// Raw video formats with a GStreamer equivalent, Styx fourcc to GStreamer format.
const RAW: [(FourCc, VideoFormat); 13] = [
    (FourCc::YUYV, VideoFormat::Yuy2),
    (FourCc::UYVY, VideoFormat::Uyvy),
    (FourCc::YVYU, VideoFormat::Yvyu),
    (FourCc::NV12, VideoFormat::Nv12),
    (FourCc::NV21, VideoFormat::Nv21),
    (FourCc::YU12, VideoFormat::I420),
    (FourCc::I420, VideoFormat::I420),
    (FourCc::YV12, VideoFormat::Yv12),
    (FourCc::GREY, VideoFormat::Gray8),
    (FourCc::RG24, VideoFormat::Rgb),
    (FourCc::BG24, VideoFormat::Bgr),
    (FourCc::RGBA, VideoFormat::Rgba),
    (FourCc::BGRA, VideoFormat::Bgra),
];

/// The GStreamer raw format for a Styx fourcc.
pub fn video_format(code: FourCc) -> Option<VideoFormat> {
    let code = match code {
        FourCc::R8 => FourCc::GREY,
        FourCc::RGB3 => FourCc::RG24,
        FourCc::BGR3 => FourCc::BG24,
        other => other,
    };
    RAW.iter().find(|(c, _)| *c == code).map(|(_, f)| *f)
}

/// The Styx fourcc for a GStreamer raw format.
pub fn fourcc_for(format: VideoFormat) -> Option<FourCc> {
    RAW.iter().find(|(_, f)| *f == format).map(|(c, _)| *c)
}

/// Output formats the element offers, in preference order after the camera's own.
fn output_fourccs() -> impl Iterator<Item = FourCc> {
    [
        FourCc::YUYV,
        FourCc::NV12,
        FourCc::YU12,
        FourCc::UYVY,
        FourCc::RG24,
        FourCc::BG24,
        FourCc::RGBA,
        FourCc::BGRA,
        FourCc::GREY,
        FourCc::MJPG,
        FourCc::H264,
        FourCc::H265,
    ]
    .into_iter()
}

/// The empty caps structure for `code` (no size or rate), `None` if GStreamer has no name for it.
pub fn structure_for(code: FourCc) -> Option<gst::Structure> {
    match code {
        FourCc::MJPG | FourCc::JPEG => Some(gst::Structure::new_empty("image/jpeg")),
        FourCc::H264 => Some(
            gst::Structure::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ),
        FourCc::H265 | FourCc::HEVC => Some(
            gst::Structure::builder("video/x-h265")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ),
        other => video_format(other).map(|format| {
            gst::Structure::builder("video/x-raw")
                .field("format", format.to_str().as_str())
                .build()
        }),
    }
}

/// Everything the element can produce, before a camera is chosen.
pub fn template_caps() -> gst::Caps {
    let mut caps = gst::Caps::new_empty();
    let caps_mut = caps.get_mut().expect("new caps are writable");
    let mut raw: Vec<&str> = Vec::new();
    for (_, format) in RAW {
        if !raw.contains(&format.to_str().as_str()) {
            raw.push(format.to_str().as_str());
        }
    }
    let size = gst::IntRange::new(1, i32::MAX);
    let rate = gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1));
    caps_mut.append_structure(
        gst::Structure::builder("video/x-raw")
            .field("format", gst::List::new(raw))
            .field("width", size)
            .field("height", size)
            .field("framerate", rate)
            .build(),
    );
    for name in ["image/jpeg", "video/x-h264", "video/x-h265"] {
        let mut s = gst::Structure::builder(name)
            .field("width", size)
            .field("height", size)
            .field("framerate", rate)
            .build();
        if name != "image/jpeg" {
            s.set("stream-format", "byte-stream");
            s.set("alignment", "au");
        }
        caps_mut.append_structure(s);
    }
    caps_mut.append_structure_full(
        gst::Structure::builder("video/x-raw")
            .field("format", "DMA_DRM")
            .field("width", size)
            .field("height", size)
            .field("framerate", rate)
            .build(),
        Some(gst::CapsFeatures::new([
            gst_allocators::CAPS_FEATURE_MEMORY_DMABUF,
        ])),
    );
    caps
}

/// One producible output: a format at a size, at these rates.
#[derive(Debug, Clone)]
struct Output {
    rates: Vec<gst::Fraction>,
    /// The camera produces this format itself (camera buffers, no conversion).
    native: bool,
    area: u64,
}

/// The caps a camera can produce: every mode's size in every format the planner can deliver
/// there. Formats the camera produces come first (largest size first), then converted ones;
/// dma-buf variants of the camera's own raw formats come last.
pub fn device_caps(device: &ProbedDevice) -> gst::Caps {
    let registry = match CodecRegistry::with_enabled_codecs() {
        Ok(registry) => registry.handle(),
        Err(_) => return gst::Caps::new_empty(),
    };
    let mut outputs: BTreeMap<(FourCc, u32, u32), Output> = BTreeMap::new();
    for backend in &device.backends {
        for mode in &backend.descriptor.modes {
            let res = mode.format.resolution;
            let (w, h) = (res.width.get(), res.height.get());
            let mut rates: Vec<gst::Fraction> = mode
                .intervals
                .iter()
                .map(|i| gst::Fraction::new(i.denominator.get() as i32, i.numerator.get() as i32))
                .collect();
            rates.sort_by(|a, b| b.cmp(a));
            for code in std::iter::once(mode.format.code).chain(output_fourccs()) {
                if structure_for(code).is_none() {
                    continue;
                }
                let key = (code, w, h);
                let native = code == mode.format.code;
                if !native && !outputs.contains_key(&key) {
                    let req = FrameRequirements::formats([code])
                        .min_resolution(w, h)
                        .max_resolution(w, h);
                    match plan_frames_with(device, &req, &registry) {
                        Ok(plan) if plan.output_resolution() == (w, h) => {}
                        _ => continue,
                    }
                }
                let entry = outputs.entry(key).or_insert_with(|| Output {
                    rates: Vec::new(),
                    native,
                    area: u64::from(w) * u64::from(h),
                });
                entry.native |= native;
                for rate in &rates {
                    if !entry.rates.contains(rate) {
                        entry.rates.push(*rate);
                    }
                }
            }
        }
    }
    let mut order: Vec<_> = outputs.into_iter().collect();
    order.sort_by(|((ca, wa, ha), a), ((cb, wb, hb), b)| {
        b.native
            .cmp(&a.native)
            .then(b.area.cmp(&a.area))
            .then(fourcc_rank(*ca).cmp(&fourcc_rank(*cb)))
            .then((wb, hb).cmp(&(wa, ha)))
    });
    let mut caps = gst::Caps::new_empty();
    let caps_mut = caps.get_mut().expect("new caps are writable");
    for ((code, w, h), output) in &order {
        if let Some(s) = sized(*code, *w, *h, &output.rates) {
            caps_mut.append_structure(s);
        }
    }
    for ((code, w, h), output) in &order {
        if !output.native || !dmabuf_capable(device) {
            continue;
        }
        if let Some(s) = drm_structure(*code, *w, *h, &output.rates) {
            caps_mut.append_structure_full(
                s,
                Some(gst::CapsFeatures::new([
                    gst_allocators::CAPS_FEATURE_MEMORY_DMABUF,
                ])),
            );
        }
    }
    caps
}

fn fourcc_rank(code: FourCc) -> usize {
    output_fourccs()
        .position(|c| c == code)
        .unwrap_or(usize::MAX)
}

/// Backends whose camera buffers can be dma-bufs.
fn dmabuf_capable(device: &ProbedDevice) -> bool {
    device.backends.iter().any(|b| {
        matches!(
            b.kind,
            BackendKind::V4l2 | BackendKind::Native | BackendKind::Libcamera
        )
    })
}

fn rate_value(rates: &[gst::Fraction]) -> gst::glib::SendValue {
    match rates {
        [] => gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1))
            .to_send_value(),
        [one] => one.to_send_value(),
        many => gst::List::new(many.iter().copied()).to_send_value(),
    }
}

fn sized(code: FourCc, w: u32, h: u32, rates: &[gst::Fraction]) -> Option<gst::Structure> {
    let mut s = structure_for(code)?;
    s.set("width", w as i32);
    s.set("height", h as i32);
    s.set_value("framerate", rate_value(rates));
    Some(s)
}

fn drm_structure(code: FourCc, w: u32, h: u32, rates: &[gst::Fraction]) -> Option<gst::Structure> {
    let format = video_format(code)?;
    let drm = gst_video::dma_drm_fourcc_from_format(format).ok()?;
    let mut s = gst::Structure::builder("video/x-raw")
        .field("format", "DMA_DRM")
        .field(
            "drm-format",
            gst_video::dma_drm_fourcc_to_string(drm, 0).as_str(),
        )
        .field("width", w as i32)
        .field("height", h as i32)
        .build();
    s.set_value("framerate", rate_value(rates));
    Some(s)
}

/// What fixed caps ask for; `None` for caps Styx cannot produce.
pub fn negotiated(caps: &gst::CapsRef) -> Option<Negotiated> {
    let s = caps.structure(0)?;
    let dmabuf = caps
        .features(0)
        .is_some_and(|f| f.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF));
    let width = u32::try_from(s.get::<i32>("width").ok()?).ok()?;
    let height = u32::try_from(s.get::<i32>("height").ok()?).ok()?;
    let fps = s
        .get::<gst::Fraction>("framerate")
        .ok()
        .map(|f| (f.numer(), f.denom()))
        .filter(|(n, _)| *n > 0);
    let mut drm = None;
    let fourcc = match s.name().as_str() {
        "image/jpeg" => FourCc::MJPG,
        "video/x-h264" => FourCc::H264,
        "video/x-h265" => FourCc::H265,
        "video/x-raw" => {
            let format = s.get::<&str>("format").ok()?;
            if format == "DMA_DRM" {
                let info = gst_video::VideoInfoDmaDrm::from_caps(caps).ok()?;
                drm = Some((info.fourcc(), info.modifier()));
                // Camera buffers are linear; other modifiers are not produced.
                if info.modifier() != 0 {
                    return None;
                }
                let format = gst_video::dma_drm_fourcc_to_format(info.fourcc()).ok()?;
                fourcc_for(format)?
            } else {
                fourcc_for(VideoFormat::from_string(format))?
            }
        }
        _ => return None,
    };
    Some(Negotiated {
        fourcc,
        width,
        height,
        fps,
        dmabuf,
        drm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip() {
        for (code, format) in RAW {
            if code == FourCc::I420 {
                continue; // GStreamer's I420 maps back to V4L2's name, YU12
            }
            assert_eq!(video_format(code), Some(format));
            assert_eq!(fourcc_for(format), Some(code));
        }
        assert_eq!(video_format(FourCc::RGB3), Some(VideoFormat::Rgb));
        assert_eq!(video_format(FourCc::MJPG), None);
    }

    #[test]
    fn negotiated_from_raw_and_jpeg_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "YUY2")
            .field("width", 640)
            .field("height", 480)
            .field("framerate", gst::Fraction::new(30, 1))
            .build();
        let n = negotiated(&caps).unwrap();
        assert_eq!(n.fourcc, FourCc::YUYV);
        assert_eq!((n.width, n.height), (640, 480));
        assert_eq!(
            n.frame_duration(),
            Some(gst::ClockTime::from_nseconds(33_333_333))
        );
        assert!(!n.dmabuf);
        let req = n.requirements(true);
        assert_eq!(req.min_resolution, Some((640, 480)));
        assert_eq!(req.min_fps, Some(30));

        let caps = gst::Caps::builder("image/jpeg")
            .field("width", 1280)
            .field("height", 720)
            .field("framerate", gst::Fraction::new(30000, 1001))
            .build();
        let n = negotiated(&caps).unwrap();
        assert_eq!(n.fourcc, FourCc::MJPG);
        assert_eq!(n.requirements(false).min_fps, Some(29));
    }

    #[test]
    fn dma_drm_caps_negotiate() {
        gst::init().unwrap();
        let s = drm_structure(FourCc::NV12, 640, 480, &[gst::Fraction::new(30, 1)]).unwrap();
        let mut caps = gst::Caps::new_empty();
        caps.get_mut().unwrap().append_structure_full(
            s,
            Some(gst::CapsFeatures::new([
                gst_allocators::CAPS_FEATURE_MEMORY_DMABUF,
            ])),
        );
        let n = negotiated(&caps).unwrap();
        assert_eq!(n.fourcc, FourCc::NV12);
        assert!(n.dmabuf);
        assert_eq!(n.drm.map(|(_, m)| m), Some(0));
    }

    #[test]
    fn virtual_camera_caps_list_native_and_converted_formats() {
        gst::init().unwrap();
        let device = CaptureRequest::virtual_source(
            VirtualSourceConfig::new()
                .name("virtual")
                .format(FourCc::YUYV)
                .resolution(320, 240)
                .fps(30),
        )
        .into_device();
        let caps = device_caps(&device);
        let first = caps.structure(0).unwrap();
        assert_eq!(first.get::<&str>("format").unwrap(), "YUY2");
        let formats: Vec<_> = caps
            .iter()
            .filter_map(|s| s.get::<&str>("format").ok())
            .collect();
        assert!(
            formats.contains(&"RGB") && formats.contains(&"GRAY8"),
            "{formats:?}"
        );
        // Virtual frames are heap memory: no dma-buf variants.
        assert!(
            caps.iter_with_features()
                .all(|(_, f)| !f.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF))
        );
        assert_eq!(first.get::<i32>("width").unwrap(), 320);
        assert!(template_caps().can_intersect(&caps));
    }
}
