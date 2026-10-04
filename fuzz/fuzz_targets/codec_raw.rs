//! Raw frames through every raw decoder and converter (Bayer through the software ISP, YUV,
//! NV12, mono, RGB variants) with odd sizes, strides and short planes, as a camera that
//! reports a wrong `bytesperline` or sends a short frame would deliver them.

#![no_main]

use std::sync::{Arc, OnceLock};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smallvec::SmallVec;
use styx_codec::{Codec, CodecRegistry};
use styx_core::prelude::*;

/// Every codec that takes uncompressed frames.
fn codecs() -> &'static [Arc<dyn Codec>] {
    static CODECS: OnceLock<Vec<Arc<dyn Codec>>> = OnceLock::new();
    CODECS.get_or_init(|| {
        let registry = CodecRegistry::with_enabled_codecs_for_max(4096, 4096).expect("registry");
        let handle = registry.handle();
        let compressed = [FourCc::MJPG, FourCc::new(*b"JPEG"), FourCc::new(*b"H264")];
        let mut all = Vec::new();
        for (fourcc, descs) in handle.list_registered() {
            if compressed.contains(&fourcc) {
                continue;
            }
            for d in descs {
                if let Ok(c) = handle.lookup_for_output_where(fourcc, d.output, |x| {
                    x.name == d.name && x.impl_name == d.impl_name && x.kind == d.kind
                }) {
                    all.push(c);
                }
            }
        }
        assert!(!all.is_empty());
        all
    })
}

#[derive(Arbitrary, Debug)]
struct Plane {
    offset: u8,
    len: u16,
    stride: u16,
}

#[derive(Arbitrary, Debug)]
struct Input {
    codec: u16,
    width: u8,
    height: u8,
    planes: Vec<Plane>,
    fill: u8,
}

fuzz_target!(|input: Input| {
    let codecs = codecs();
    let codec = &codecs[usize::from(input.codec) % codecs.len()];
    let fourcc = codec.descriptor().input;
    let (Some(w), Some(h)) = (
        std::num::NonZeroU32::new(u32::from(input.width)),
        std::num::NonZeroU32::new(u32::from(input.height)),
    ) else {
        return;
    };
    let res = Resolution {
        width: w,
        height: h,
    };
    let meta = FrameMeta::new(MediaFormat::new(fourcc, res, ColorSpace::Srgb), 0);
    let mut buffers: SmallVec<[BufferLease; 3]> = SmallVec::new();
    let mut layouts: SmallVec<[PlaneLayout; 3]> = SmallVec::new();
    for p in input.planes.iter().take(3) {
        let total = usize::from(p.offset) + usize::from(p.len);
        let mut buf = BufferPool::with_limits(1, total.max(1), 0).lease();
        buf.resize(total);
        buf.as_mut_slice().fill(input.fill);
        buffers.push(buf);
        layouts.push(PlaneLayout {
            offset: usize::from(p.offset),
            len: usize::from(p.len),
            stride: usize::from(p.stride),
        });
    }
    let frame = FrameLease::multi_plane(meta, buffers, layouts);
    if let Ok(out) = codec.process(frame) {
        let _ = out.validate_plane_layouts();
        let _ = out.to_visible_vec();
    }
});
