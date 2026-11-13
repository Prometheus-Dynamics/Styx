//! Byte parity with the frozen reference: the bytes this encoder gives for the planes below
//! match, for every size, quality and format in the table, the bytes of dev's `StyxJpeg` (the
//! reference encoder before this crate; its table was produced by running dev's code on the same
//! planes, see the E2 PR for the harness).

use styx_jpeg::{JpegEncoder, PlanarInput};

/// The planes of one case, as the reference harness builds them: a deterministic pattern with
/// 2-bit noise, the chroma planes of `w.div_ceil(2)` by `h.div_ceil(2)`, and `pad` bytes at the
/// end of every row (the padding is garbage, which the encoder must not read).
#[derive(Debug)]
pub struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    uv: Vec<u8>,
    ys: usize,
    cs: usize,
    uvs: usize,
}

fn rng(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

fn plane(w: usize, h: usize, stride: usize, seed: u32, a: usize, b: usize) -> Vec<u8> {
    let mut out = vec![0u8; stride * h];
    let mut s = seed;
    for y in 0..h {
        for x in 0..w {
            out[y * stride + x] = ((x * a + y * b) as u8).wrapping_add((rng(&mut s) & 31) as u8);
        }
    }
    out
}

pub fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn planes(w: usize, h: usize, pad: usize) -> Planes {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ys, cs, uvs) = (w + pad, cw + pad, 2 * cw + pad);
    let y = plane(w, h, ys, 1, 3, 5);
    let u = plane(cw, ch, cs, 2, 7, 1);
    let v = plane(cw, ch, cs, 3, 1, 9);
    let mut uv = vec![0u8; uvs * ch];
    for r in 0..ch {
        for x in 0..cw {
            uv[r * uvs + 2 * x] = u[r * cs + x];
            uv[r * uvs + 2 * x + 1] = v[r * cs + x];
        }
    }
    Planes {
        y,
        u,
        v,
        uv,
        ys,
        cs,
        uvs,
    }
}

impl Planes {
    pub fn i420(&self, w: usize, h: usize) -> PlanarInput<'_> {
        PlanarInput::I420 {
            y: &self.y,
            u: &self.u,
            v: &self.v,
            width: w,
            height: h,
            y_stride: self.ys,
            c_stride: self.cs,
        }
    }

    pub fn nv12(&self, w: usize, h: usize) -> PlanarInput<'_> {
        PlanarInput::Nv12 {
            y: &self.y,
            uv: &self.uv,
            width: w,
            height: h,
            y_stride: self.ys,
            uv_stride: self.uvs,
        }
    }

    pub fn gray(&self, w: usize, h: usize) -> PlanarInput<'_> {
        PlanarInput::Gray {
            y: &self.y,
            width: w,
            height: h,
            stride: self.ys,
        }
    }
}

/// (width, height, quality, format, length, FNV-1a 64) of styx-codec's `StyxJpeg` on the same planes.
const DEV_BYTES: [(usize, usize, u8, &str, usize, u64); 45] = [
    (1, 1, 1, "I420", 615, 0x106057798047ad1a),
    (1, 1, 1, "NV12", 615, 0x106057798047ad1a),
    (1, 1, 1, "GREY", 328, 0x0e4e4bc69e48397e),
    (1, 1, 75, "I420", 618, 0x89eadcb8916d430d),
    (1, 1, 75, "NV12", 618, 0x89eadcb8916d430d),
    (1, 1, 75, "GREY", 328, 0x370bfdfeab946791),
    (1, 1, 100, "I420", 621, 0x1eca563f6bb55da6),
    (1, 1, 100, "NV12", 621, 0x1eca563f6bb55da6),
    (1, 1, 100, "GREY", 329, 0x7514a5f0b0bc6395),
    (7, 5, 1, "I420", 614, 0x061bd7bf7fef0260),
    (7, 5, 1, "NV12", 614, 0x061bd7bf7fef0260),
    (7, 5, 1, "GREY", 328, 0x41f25d9aadc1dc7b),
    (7, 5, 75, "I420", 647, 0x2a4f717055f1ea4e),
    (7, 5, 75, "NV12", 647, 0x2a4f717055f1ea4e),
    (7, 5, 75, "GREY", 339, 0x7f4bbfaff6c3bc02),
    (7, 5, 100, "I420", 785, 0x82a4d74b3f0cd802),
    (7, 5, 100, "NV12", 785, 0x82a4d74b3f0cd802),
    (7, 5, 100, "GREY", 374, 0x1299fd6fbbbb2970),
    (17, 33, 1, "I420", 643, 0x46063c416d370cdf),
    (17, 33, 1, "NV12", 643, 0x46063c416d370cdf),
    (17, 33, 1, "GREY", 340, 0xa8be3b109055ae73),
    (17, 33, 75, "I420", 903, 0x0116213723879bbb),
    (17, 33, 75, "NV12", 903, 0x0116213723879bbb),
    (17, 33, 75, "GREY", 488, 0xea3f09d360fe1dc3),
    (17, 33, 100, "I420", 1592, 0x80921d418dc408fb),
    (17, 33, 100, "NV12", 1592, 0x80921d418dc408fb),
    (17, 33, 100, "GREY", 858, 0x718aaca0204f5fa8),
    (33, 17, 1, "I420", 643, 0x3040c83bd44ad2bc),
    (33, 17, 1, "NV12", 643, 0x3040c83bd44ad2bc),
    (33, 17, 1, "GREY", 341, 0xc4cc2d3e528cf434),
    (33, 17, 75, "I420", 906, 0xef41724cb52d3c23),
    (33, 17, 75, "NV12", 906, 0xef41724cb52d3c23),
    (33, 17, 75, "GREY", 491, 0x0af009d4c5b62500),
    (33, 17, 100, "I420", 1595, 0x5261123f614e3537),
    (33, 17, 100, "NV12", 1595, 0x5261123f614e3537),
    (33, 17, 100, "GREY", 862, 0x4bc741a53263c2a8),
    (321, 241, 1, "I420", 5298, 0xa2c3a737df2da7d4),
    (321, 241, 1, "NV12", 5298, 0xa2c3a737df2da7d4),
    (321, 241, 1, "GREY", 3468, 0xd177516a689dd7cc),
    (321, 241, 75, "I420", 33198, 0x831259b806cb35b2),
    (321, 241, 75, "NV12", 33198, 0x831259b806cb35b2),
    (321, 241, 75, "GREY", 23884, 0x1a9f61bf9401aac3),
    (321, 241, 100, "I420", 125976, 0x9e31975f4c3e2464),
    (321, 241, 100, "NV12", 125976, 0x9e31975f4c3e2464),
    (321, 241, 100, "GREY", 81312, 0x808edd5be31da235),
];

#[test]
fn bytes_match_the_reference_encoder_for_every_case() {
    for (w, h, q, format, len, hash) in DEV_BYTES {
        let pad = if q == 75 { 5 } else { 0 };
        let pl = planes(w, h, pad);
        let input = match format {
            "I420" => pl.i420(w, h),
            "NV12" => pl.nv12(w, h),
            _ => pl.gray(w, h),
        };
        let mut out = Vec::new();
        JpegEncoder::new(q).encode(&input, &mut out).unwrap();
        assert_eq!((out.len(), fnv(&out)), (len, hash), "{w}x{h} q{q} {format}");
    }
}
