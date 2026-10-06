//! Byte order of the 32-bit RGB formats through every decoder path: one known pixel, red
//! (`0x11`), green (`0x22`), blue (`0x33`), laid out as V4L2 / DRM define each code, must come
//! out as RGB `11 22 33`. `XR24` (V4L2 `XBGR32`, DRM `XRGB8888`) is bytes B, G, R, x; `XB24`
//! (V4L2 `RGBX32`, DRM `XBGR8888`) is R, G, B, x; Styx's own `RGBA` / `BGRA` are R, G, B, A
//! and B, G, R, A.

use styx_core::prelude::*;

use crate::CodecRegistry;

const RGB: [u8; 3] = [0x11, 0x22, 0x33];

/// `(code, the pixel's bytes in memory)`.
const PIXELS: [(FourCc, [u8; 4]); 4] = [
    (FourCc::XR24, [0x33, 0x22, 0x11, 0x00]),
    (FourCc::XB24, [0x11, 0x22, 0x33, 0x00]),
    (FourCc::RGBA, [0x11, 0x22, 0x33, 0xff]),
    (FourCc::BGRA, [0x33, 0x22, 0x11, 0xff]),
];

/// A 3x2 frame of `pixel`, rows padded to 16 bytes.
fn frame(code: FourCc, pixel: [u8; 4]) -> FrameLease {
    let (w, h, stride) = (3usize, 2usize, 16usize);
    let len = stride * h;
    let mut data = vec![0xeeu8; len];
    for row in data.chunks_mut(stride) {
        for px in row[..w * 4].chunks_mut(4) {
            px.copy_from_slice(&pixel);
        }
    }
    let mut buf = BufferPool::with_limits(1, len, 1).lease();
    buf.replace_owned(data);
    let res = Resolution::new(w as u32, h as u32).unwrap();
    FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(code, res, ColorSpace::Srgb), 0),
        buf,
        len,
        stride,
    )
}

fn assert_rgb(code: FourCc, path: &str, rgb: &[u8]) {
    assert_eq!(rgb.len(), 3 * 2 * 3, "{code} via {path}");
    for px in rgb.chunks(3) {
        assert_eq!(px, RGB, "{code} via {path}");
    }
}

/// The registry's raw decoders (`xr24-strip`, `xb24-strip`, `rgba-strip`, `bgra-strip`).
#[test]
fn registry_decoders_read_v4l2_byte_order() {
    let registry = CodecRegistry::with_enabled_codecs_for_max(64, 64).unwrap();
    let handle = registry.handle();
    for (code, pixel) in PIXELS {
        let codec = handle.lookup_for_output(code, FourCc::RG24).unwrap();
        let out = codec.process(frame(code, pixel)).unwrap();
        assert_eq!(out.meta().format.code, FourCc::RG24);
        let plane = out.planes().into_iter().next().unwrap();
        let stride = plane.stride().max(9);
        let rgb: Vec<u8> = plane
            .data()
            .chunks(stride)
            .take(2)
            .flat_map(|row| row[..9].to_vec())
            .collect();
        assert_rgb(code, codec.descriptor().impl_name, &rgb);
    }
}

/// `frame_to_dynamic_image` (the `image` crate bridge).
#[cfg(feature = "image")]
#[test]
fn dynamic_image_reads_v4l2_byte_order() {
    for (code, pixel) in PIXELS {
        let img = crate::decoder::frame_to_dynamic_image(&frame(code, pixel)).unwrap();
        assert_rgb(code, "frame_to_dynamic_image", img.to_rgb8().as_raw());
    }
}
