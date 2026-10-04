//! Scalar kernels: the correctness oracle every SIMD leaf is tested against, and the fallback
//! where no leaf applies. Row kernels take `width` pixels; slices must hold at least that many.

/// BGR ↔ RGB: swap bytes 0 and 2 of each 3-byte pixel. `src` and `dst` may be the same row.
pub fn swap_rb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 3]
        .chunks_exact_mut(3)
        .zip(src[..width * 3].chunks_exact(3))
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
        .chunks_exact_mut(4)
        .zip(src[..width * 4].chunks_exact(4))
    {
        d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
    }
}

/// 4-byte pixels to 3-byte pixels, dropping byte 3; `swap` also exchanges bytes 0 and 2
/// (BGRA/BGRX → RGB).
pub fn x32_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    for (d, s) in dst[..width * 3]
        .chunks_exact_mut(3)
        .zip(src[..width * 4].chunks_exact(4))
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
        .chunks_exact_mut(4)
        .zip(src[..width * 3].chunks_exact(3))
    {
        d.copy_from_slice(&[s[0], s[1], s[2], 255]);
    }
}

/// Grey to RGB: each byte three times.
pub fn gray8_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, &g) in dst[..width * 3].chunks_exact_mut(3).zip(&src[..width]) {
        d.fill(g);
    }
}

/// Little-endian 16-bit grey to RGB: the high byte three times.
pub fn gray16le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) {
    for (d, s) in dst[..width * 3]
        .chunks_exact_mut(3)
        .zip(src[..width * 2].chunks_exact(2))
    {
        d.fill(s[1]);
    }
}

/// Little-endian 16-bit RGB (or BGR with `swap`) to RGB24: the high byte of each channel.
pub fn rgb48le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) {
    for (d, s) in dst[..width * 3]
        .chunks_exact_mut(3)
        .zip(src[..width * 6].chunks_exact(6))
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
        .zip(src[..width * 2].chunks_exact(2))
    {
        *d = s[0];
    }
}

/// 2x2 box average of two rows into `width` outputs, rounding half up.
pub fn box2_row(top: &[u8], bottom: &[u8], dst: &mut [u8], width: usize) {
    for ((d, t), b) in dst[..width]
        .iter_mut()
        .zip(top[..width * 2].chunks_exact(2))
        .zip(bottom[..width * 2].chunks_exact(2))
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
