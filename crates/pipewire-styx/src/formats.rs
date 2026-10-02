//! What a Styx camera offers PipeWire (asking the Styx planner for every mode and output
//! format) and copying frames into PipeWire's packed layout. No PipeWire types here, so this is
//! tested without the PipeWire libraries (`cargo test --no-default-features`).

use std::collections::BTreeMap;

use styx::planner::plan_frames_with;
use styx::prelude::*;

/// Formats PipeWire consumers (browsers, OBS) take, in preference order after the camera's own.
pub const FORMATS: [FourCc; 9] = [
    FourCc::YUYV,
    FourCc::UYVY,
    FourCc::NV12,
    FourCc::YU12,
    FourCc::RG24,
    FourCc::BG24,
    FourCc::RGBA,
    FourCc::BGRA,
    FourCc::GREY,
];

/// One format a camera can deliver at one size, and its frame rates (fps fractions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    pub rates: Vec<(u32, u32)>,
}

/// Everything `device` can deliver in PipeWire formats: the camera's own formats first, then
/// what the planner can convert to, largest sizes first.
pub fn offers(device: &ProbedDevice) -> Vec<Offer> {
    let Ok(registry) = CodecRegistry::with_enabled_codecs() else {
        return Vec::new();
    };
    let registry = registry.handle();
    let mut found: BTreeMap<(FourCc, u32, u32), (bool, Vec<(u32, u32)>)> = BTreeMap::new();
    for backend in &device.backends {
        for mode in &backend.descriptor.modes {
            let res = mode.format.resolution;
            let (w, h) = (res.width.get(), res.height.get());
            let rates: Vec<(u32, u32)> = mode
                .intervals
                .iter()
                .map(|i| (i.denominator.get(), i.numerator.get()))
                .collect();
            for code in FORMATS {
                let native = code == mode.format.code;
                let key = (code, w, h);
                if !native && !found.contains_key(&key) {
                    let req = FrameRequirements::formats([code])
                        .min_resolution(w, h)
                        .max_resolution(w, h);
                    match plan_frames_with(device, &req, &registry) {
                        Ok(plan) if plan.output_resolution() == (w, h) => {}
                        _ => continue,
                    }
                }
                let entry = found.entry(key).or_insert((native, Vec::new()));
                entry.0 |= native;
                for rate in &rates {
                    if !entry.1.contains(rate) {
                        entry.1.push(*rate);
                    }
                }
            }
        }
    }
    let mut offers: Vec<(bool, Offer)> = found
        .into_iter()
        .map(|((fourcc, width, height), (native, mut rates))| {
            rates.sort_by(|a, b| (b.0 * a.1).cmp(&(a.0 * b.1)));
            (
                native,
                Offer {
                    fourcc,
                    width,
                    height,
                    rates,
                },
            )
        })
        .collect();
    offers.sort_by(|(na, a), (nb, b)| {
        nb.cmp(na).then(
            (u64::from(b.width) * u64::from(b.height))
                .cmp(&(u64::from(a.width) * u64::from(a.height))),
        )
    });
    offers.into_iter().map(|(_, offer)| offer).collect()
}

/// Bytes per row of the first plane, and of the whole frame, packed tightly.
pub fn packed_size(fourcc: FourCc, width: u32, height: u32) -> Option<(u32, u32)> {
    let (w, h) = (width, height);
    Some(match fourcc {
        FourCc::YUYV | FourCc::UYVY => (w * 2, w * 2 * h),
        FourCc::RG24 | FourCc::BG24 => (w * 3, w * 3 * h),
        FourCc::RGBA | FourCc::BGRA => (w * 4, w * 4 * h),
        FourCc::GREY => (w, w * h),
        FourCc::NV12 | FourCc::YU12 => (w, w * h + 2 * (w.div_ceil(2) * h.div_ceil(2))),
        _ => return None,
    })
}

/// Copy `frame`'s visible pixels into `dst` packed tightly (rows of `packed_size`'s stride,
/// planes one after another). Returns the bytes written, `None` if `dst` is too small.
pub fn copy_packed(frame: &FrameLease, dst: &mut [u8]) -> Option<usize> {
    let meta = frame.meta();
    let res = meta.format.resolution;
    let (w, h) = (res.width.get() as usize, res.height.get() as usize);
    let (row, total) = packed_size(meta.format.code, w as u32, h as u32)?;
    if dst.len() < total as usize {
        return None;
    }
    // (rows, bytes per row) for each plane.
    let shapes: &[(usize, usize)] = match meta.format.code {
        FourCc::NV12 => &[(h, w), (h.div_ceil(2), 2 * w.div_ceil(2))],
        FourCc::YU12 => &[
            (h, w),
            (h.div_ceil(2), w.div_ceil(2)),
            (h.div_ceil(2), w.div_ceil(2)),
        ],
        _ => &[(h, row as usize)],
    };
    let planes = frame.planes();
    let mut at = 0;
    for (plane, &(rows, bytes)) in planes.iter().zip(shapes) {
        let (data, stride) = (plane.data(), plane.stride());
        for y in 0..rows {
            let src = data.get(y * stride..y * stride + bytes)?;
            dst[at..at + bytes].copy_from_slice(src);
            at += bytes;
        }
    }
    (at == total as usize).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_sizes() {
        assert_eq!(packed_size(FourCc::YUYV, 640, 480), Some((1280, 614_400)));
        assert_eq!(packed_size(FourCc::NV12, 640, 480), Some((640, 460_800)));
        assert_eq!(packed_size(FourCc::MJPG, 640, 480), None);
    }

    #[test]
    fn copies_strided_planes_packed() {
        // NV12 4x2 with 8-byte strides: packed output drops the padding.
        let format = MediaFormat::new(
            FourCc::NV12,
            Resolution::new(4, 2).unwrap(),
            ColorSpace::Bt709,
        );
        let y: Vec<u8> = (0..16).collect();
        let uv: Vec<u8> = (100..108).collect();
        let layouts = [(16, 8), (8, 8)]
            .into_iter()
            .map(|(len, stride)| PlaneLayout {
                offset: 0,
                len,
                stride,
            })
            .collect();
        let frame = FrameLease::multi_plane(
            FrameMeta::new(format, 0),
            [y, uv]
                .into_iter()
                .map(|bytes| {
                    let mut lease = BufferPool::with_capacity(1, bytes.len()).lease();
                    lease.replace_owned(bytes);
                    lease
                })
                .collect(),
            layouts,
        );
        let mut dst = [0u8; 12];
        assert_eq!(copy_packed(&frame, &mut dst), Some(12));
        assert_eq!(dst, [0, 1, 2, 3, 8, 9, 10, 11, 100, 101, 102, 103]);
        assert_eq!(copy_packed(&frame, &mut [0u8; 11]), None);
    }

    #[test]
    fn virtual_camera_offers_its_format_first() {
        let device = CaptureRequest::virtual_source(
            VirtualSourceConfig::new()
                .name("virtual")
                .format(FourCc::YUYV)
                .resolution(320, 240)
                .fps(30),
        )
        .into_device();
        let offers = offers(&device);
        assert_eq!(offers[0].fourcc, FourCc::YUYV);
        assert_eq!((offers[0].width, offers[0].height), (320, 240));
        assert!(offers.iter().any(|o| o.fourcc == FourCc::RG24));
    }
}
