//! Raw frames with odd sizes, odd strides and short planes (a camera reporting a wrong
//! `bytesperline`, a short USB frame) through every raw decoder and converter: an error or a
//! picture, never a panic or a read outside the planes. Inputs found by the `codec_raw` fuzz
//! target (`docs/fuzzing.md`) are added here.

use std::sync::Arc;

use smallvec::SmallVec;
use styx_core::prelude::*;

use crate::{Codec, CodecRegistry};

fn raw_codecs() -> Vec<Arc<dyn Codec>> {
    let registry = CodecRegistry::with_enabled_codecs_for_max(256, 256).unwrap();
    let handle = registry.handle();
    let compressed = [FourCc::MJPG, FourCc::JPEG, FourCc::H264, FourCc::H265];
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
    all
}

/// A frame of `fourcc` with the given planes as `(offset, len, stride)`, bytes set to `fill`.
fn frame(fourcc: FourCc, width: u32, height: u32, planes: &[(usize, usize, usize)]) -> FrameLease {
    let res = Resolution::new(width, height).unwrap();
    let meta = FrameMeta::new(MediaFormat::new(fourcc, res, ColorSpace::Srgb), 0);
    let mut buffers: SmallVec<[BufferLease; 3]> = SmallVec::new();
    let mut layouts: SmallVec<[PlaneLayout; 3]> = SmallVec::new();
    for &(offset, len, stride) in planes {
        let mut buf = BufferPool::with_limits(1, (offset + len).max(1), 0).lease();
        buf.resize(offset + len);
        buf.as_mut_slice().fill(0x80);
        buffers.push(buf);
        layouts.push(PlaneLayout {
            offset,
            len,
            stride,
        });
    }
    FrameLease::multi_plane(meta, buffers, layouts)
}

#[test]
fn odd_sizes_strides_and_short_planes_never_panic() {
    let codecs = raw_codecs();
    assert!(!codecs.is_empty());
    for codec in &codecs {
        let fourcc = codec.descriptor().input;
        for (w, h) in [(1, 1), (3, 2), (5, 3), (7, 1), (2, 5)] {
            let full = w as usize * h as usize * 8;
            for stride in [0, 1, w as usize, w as usize * 2 + 1, w as usize * 4] {
                for len in [0, 1, full / 3, full] {
                    for planes in 1..=3 {
                        let p = vec![(0, len, stride); planes];
                        if let Ok(out) = codec.process(frame(fourcc, w, h, &p)) {
                            let _ = out.to_visible_vec();
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn odd_width_packed_422_reads_whole_groups() {
    // UYVY (and YVYU, VYUY) 3x2 with a 6-byte stride: the last pair of each line (Y U Y V)
    // was read past the line, and past the frame on the last line.
    for codec in raw_codecs()
        .iter()
        .filter(|c| c.descriptor().impl_name.ends_with("-cpu"))
        .filter(|c| {
            [b"UYVY", b"YVYU", b"VYUY"].contains(&&c.descriptor().input.to_u32().to_le_bytes())
        })
    {
        let fourcc = codec.descriptor().input;
        assert!(codec.process(frame(fourcc, 3, 2, &[(0, 12, 6)])).is_err());
        assert!(codec.process(frame(fourcc, 3, 2, &[(0, 16, 8)])).is_ok());
    }
}

#[test]
fn odd_width_nv12_in_one_plane_has_room_for_its_chroma() {
    // NV12 3x1 in one 1-byte-stride plane: the chroma row (2 bytes) was read with a stride
    // the length check had not counted.
    for codec in raw_codecs()
        .iter()
        .filter(|c| c.descriptor().input == FourCc::NV12)
    {
        let _ = codec.process(frame(FourCc::NV12, 3, 1, &[(0, 4, 1)]));
        let _ = codec.process(frame(FourCc::NV12, 1, 1, &[(0, 2, 1)]));
    }
}
