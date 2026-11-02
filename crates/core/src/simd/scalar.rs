//! Scalar kernels: the correctness oracle every SIMD leaf is tested against, and the fallback
//! where no leaf applies. Row kernels take `width` pixels; slices must hold at least that many.

/// BGR ↔ RGB: swap bytes 0 and 2 of each 3-byte pixel. `src` and `dst` may be the same row.
pub fn swap_rb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src[..width * 3].as_chunks::<3>().0)
    {
        let (r, g, b) = (s[2], s[1], s[0]);
        d[0] = r;
        d[1] = g;
        d[2] = b;
    }
}

/// BGRA ↔ RGBA: swap bytes 0 and 2 of each 4-byte pixel, keeping byte 3.
pub fn swap_rb32_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 4]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src[..width * 4].as_chunks::<4>().0)
    {
        d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
    }
}

/// 4-byte pixels to 3-byte pixels, dropping byte 3; `swap` also exchanges bytes 0 and 2
/// (BGRA/BGRX → RGB).
pub fn x32_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    for (d, s) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src[..width * 4].as_chunks::<4>().0)
    {
        let (a, c) = if swap { (s[2], s[0]) } else { (s[0], s[2]) };
        d[0] = a;
        d[1] = s[1];
        d[2] = c;
    }
}

/// Byte order of the colour pixels [`rgb_to_luma_row`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLayout {
    Rgb24,
    Bgr24,
    Rgba32,
    Bgra32,
}

impl ColorLayout {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb24 | Self::Bgr24 => 3,
            Self::Rgba32 | Self::Bgra32 => 4,
        }
    }

    /// Offsets of R, G and B within a pixel.
    pub const fn rgb_offsets(self) -> [usize; 3] {
        match self {
            Self::Rgb24 | Self::Rgba32 => [0, 1, 2],
            Self::Bgr24 | Self::Bgra32 => [2, 1, 0],
        }
    }
}

/// Luma from colour pixels: `(77 R + 150 G + 29 B) >> 8`.
pub fn rgb_to_luma_row(src: &[u8], dst: &mut [u8], width: usize, layout: ColorLayout) {
    let bpp = layout.bytes_per_pixel();
    let [r, g, b] = layout.rgb_offsets();
    for (d, s) in dst[..width]
        .iter_mut()
        .zip(src[..width * bpp].chunks_exact(bpp))
    {
        *d = ((77 * s[r] as u32 + 150 * s[g] as u32 + 29 * s[b] as u32) >> 8) as u8;
    }
}

/// RGB24 to RGBA with alpha 255.
pub fn rgb24_to_rgba_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 4]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src[..width * 3].as_chunks::<3>().0)
    {
        d.copy_from_slice(&[s[0], s[1], s[2], 255]);
    }
}

/// Grey to RGB: each byte three times.
pub fn gray8_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, &g) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(&src[..width])
    {
        d.fill(g);
    }
}

/// Little-endian 16-bit grey to RGB: the high byte three times.
pub fn gray16le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src[..width * 2].as_chunks::<2>().0)
    {
        d.fill(s[1]);
    }
}

/// Little-endian 16-bit RGB (or BGR with `swap`) to RGB24: the high byte of each channel.
pub fn rgb48le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    for (d, s) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src[..width * 6].as_chunks::<6>().0)
    {
        let (a, c) = if swap { (s[5], s[1]) } else { (s[1], s[5]) };
        d[0] = a;
        d[1] = s[3];
        d[2] = c;
    }
}

/// YUYV (Y0 U Y1 V) to luma: the even bytes.
pub fn yuyv_luma_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width]
        .iter_mut()
        .zip(src[..width * 2].as_chunks::<2>().0)
    {
        *d = s[0];
    }
}

/// YUYV (Y0 U Y1 V) chroma of two rows averaged, rounding half up: `width` bytes of UV pairs
/// (NV12's chroma row) from the odd bytes.
pub fn yuyv_chroma_row(a: &[u8], b: &[u8], dst: &mut [u8], width: usize) {
    for ((d, x), y) in dst[..width]
        .iter_mut()
        .zip(a[..width * 2].as_chunks::<2>().0)
        .zip(b[..width * 2].as_chunks::<2>().0)
    {
        *d = (u16::from(x[1]) + u16::from(y[1])).div_ceil(2) as u8;
    }
}

/// 2x2 box average of two rows into `width` outputs, rounding half up.
pub fn box2_row(top: &[u8], bottom: &[u8], dst: &mut [u8], width: usize) {
    for ((d, t), b) in dst[..width]
        .iter_mut()
        .zip(top[..width * 2].as_chunks::<2>().0)
        .zip(bottom[..width * 2].as_chunks::<2>().0)
    {
        let sum = t[0] as u16 + t[1] as u16 + b[0] as u16 + b[1] as u16;
        *d = ((sum + 2) >> 2) as u8;
    }
}

/// Pixel order reversed: `dst[i] = src[width - 1 - i]` for `bpp`-byte pixels.
pub fn reverse_row(src: &[u8], dst: &mut [u8], width: usize, bpp: usize) {
    for (i, d) in dst[..width * bpp].chunks_exact_mut(bpp).enumerate() {
        let s = (width - 1 - i) * bpp;
        d.copy_from_slice(&src[s..s + bpp]);
    }
}

/// Transpose an 8x8 tile of `N`-byte pixels: output row `k` is input column `k`, reversed when
/// `reverse`. Output rows are `dst_stride` bytes apart (negative to fill upwards).
///
/// # Safety
/// `src` must be readable for 8 rows of 8 pixels at `src_stride`, and `dst` writable for 8
/// rows of 8 pixels at `dst_stride`.
pub unsafe fn transpose_tile<const N: usize>(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: isize,
    reverse: bool,
) {
    for k in 0..8 {
        // SAFETY: row `k` of the destination tile is in bounds per the caller.
        let out = unsafe { dst.offset(k as isize * dst_stride) };
        for i in 0..8 {
            let j = if reverse { 7 - i } else { i };
            // SAFETY: pixel (row i, column k) of the source tile and pixel j of output row k.
            unsafe {
                core::ptr::copy_nonoverlapping(src.add(i * src_stride + k * N), out.add(j * N), N)
            };
        }
    }
}

/// Integer YUV → RGB in 8-bit fixed point: `c = max(y - y_offset, 0)`, `d = u - 128`,
/// `e = v - 128`; red `y_scale·c + r_v·e`, green `y_scale·c - g_u·d - g_v·e`, blue
/// `y_scale·c + b_u·d`, each plus 128, shifted right by 8 and clamped to 0..=255.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YuvMatrix {
    pub y_offset: u8,
    pub y_scale: u16,
    pub r_v: u16,
    pub g_u: u16,
    pub g_v: u16,
    pub b_u: u16,
}

impl YuvMatrix {
    /// Limited-range Rec.709.
    pub const BT709: Self = Self::limited(459, 55, 136, 541);
    /// Limited-range Rec.2020.
    pub const BT2020: Self = Self::limited(430, 48, 166, 549);
    /// Full-range Rec.601 (sYCC, JFIF).
    pub const BT601_FULL: Self = Self {
        y_offset: 0,
        y_scale: 256,
        r_v: 359,
        g_u: 88,
        g_v: 183,
        b_u: 454,
    };

    const fn limited(r_v: u16, g_u: u16, g_v: u16, b_u: u16) -> Self {
        Self {
            y_offset: 16,
            y_scale: 298,
            r_v,
            g_u,
            g_v,
            b_u,
        }
    }

    /// The matrix for a frame labelled `color`: `Srgb` YUV is full-range BT.601 (sYCC);
    /// `Unknown` is far more likely limited-range BT.709 than full-range BT.601.
    pub const fn for_color(color: crate::format::ColorSpace) -> Self {
        use crate::format::ColorSpace;
        match color {
            ColorSpace::Bt709 | ColorSpace::Unknown => Self::BT709,
            ColorSpace::Bt2020 => Self::BT2020,
            ColorSpace::Srgb => Self::BT601_FULL,
        }
    }

    /// One pixel.
    #[inline(always)]
    pub fn rgb(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let c = i32::from(self.y_scale) * i32::from(y.saturating_sub(self.y_offset)) + 128;
        let (d, e) = (i32::from(u) - 128, i32::from(v) - 128);
        let r = c + i32::from(self.r_v) * e;
        let g = c - i32::from(self.g_u) * d - i32::from(self.g_v) * e;
        let b = c + i32::from(self.b_u) * d;
        [r, g, b].map(|x| (x >> 8).clamp(0, 255) as u8)
    }
}

/// NV12 → RGB24: `width` luma bytes, `width.div_ceil(2)` UV pairs.
pub fn nv12_rgb_row(y: &[u8], uv: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, (d, &l)) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(&y[..width])
        .enumerate()
    {
        *d = m.rgb(l, uv[i / 2 * 2], uv[i / 2 * 2 + 1]);
    }
}

/// I420 (or YV12 with `u` and `v` swapped) → RGB24: `width` luma bytes, `width.div_ceil(2)`
/// of each chroma.
pub fn i420_rgb_row(y: &[u8], u: &[u8], v: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, (d, &l)) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(&y[..width])
        .enumerate()
    {
        *d = m.rgb(l, u[i / 2], v[i / 2]);
    }
}

/// YUYV → RGB24: `width.div_ceil(2)` 4-byte Y0 U Y1 V groups.
pub fn yuyv_rgb_row(src: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, d) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
    {
        let g = &src[i / 2 * 4..][..4];
        *d = m.rgb(g[i % 2 * 2], g[1], g[3]);
    }
}
