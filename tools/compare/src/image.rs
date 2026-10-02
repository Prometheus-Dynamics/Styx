//! Image statistics of a saved frame: mean level and per-channel means, normalised to 0..1, so
//! exposure and colour of the two paths can be compared.
//!
//! Raw Bayer frames are measured as delivered (no black level subtraction, linear); YUV frames
//! are the ISP's output (gamma-encoded, white balanced). The two are only comparable once both
//! paths process images.

use serde::{Deserialize, Serialize};
use styx::prelude::FourCc;
use styx_softisp::{CfaPattern, RawPacking, bayer_fourcc};

/// A plane of a frame: bytes and row stride.
pub type PlaneRef<'a> = (&'a [u8], usize);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageStats {
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    /// `bayer`, `yuv` or `unsupported`.
    pub kind: String,
    /// How sample values were scaled to 0..1.
    pub scaling: String,
    /// Mean of every sample (Bayer) or of luma (YUV).
    pub mean: f64,
    /// Red, green, blue means. Bayer: the mosaic's channels (greens averaged). YUV: converted
    /// from the mean Y, Cb, Cr with BT.601 limited-range coefficients.
    pub rgb_means: [f64; 3],
    /// Bayer: means of the 2x2 cell positions, row by row (top-left first).
    pub cell_means: Option<[f64; 4]>,
    /// YUV: means of Y, Cb, Cr.
    pub yuv_means: Option<[f64; 3]>,
    /// Fraction of samples at or above 98% of full scale.
    pub clipped_fraction: f64,
    /// Fraction of samples at or below 2% of full scale.
    pub dark_fraction: f64,
}

fn sample(packing: RawPacking, row: &[u8], x: usize) -> u32 {
    match packing {
        RawPacking::U8 => u32::from(row[x]),
        RawPacking::U16Le { .. } => u32::from(u16::from_le_bytes([row[2 * x], row[2 * x + 1]])),
        RawPacking::Csi2Raw10 => {
            let group = (x / 4) * 5;
            let lane = x % 4;
            (u32::from(row[group + lane]) << 2) | ((u32::from(row[group + 4]) >> (2 * lane)) & 3)
        }
        RawPacking::Csi2Raw12 => {
            let group = (x / 2) * 3;
            let lane = x % 2;
            (u32::from(row[group + lane]) << 4) | ((u32::from(row[group + 2]) >> (4 * lane)) & 15)
        }
    }
}

/// Statistics of a Bayer frame. `cfa` overrides the pattern the fourcc implies.
fn bayer_stats(
    code: FourCc,
    width: usize,
    height: usize,
    plane: PlaneRef<'_>,
    cfa: Option<CfaPattern>,
) -> Option<ImageStats> {
    let (pattern, packing) = bayer_fourcc(code)?;
    let pattern = cfa.unwrap_or(pattern);
    let (data, stride) = plane;
    let row_bytes = packing.row_bytes(width);
    if stride < row_bytes || data.len() < stride * (height - 1) + row_bytes {
        return None;
    }
    let mut sums = [0u64; 4];
    let mut counts = [0u64; 4];
    let mut max_seen = 0u32;
    let mut values = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = &data[y * stride..y * stride + row_bytes];
        for x in 0..width {
            let v = sample(packing, row, x);
            let cell = (y & 1) * 2 + (x & 1);
            sums[cell] += u64::from(v);
            counts[cell] += 1;
            max_seen = max_seen.max(v);
            values.push(v);
        }
    }
    let bits = u32::from(packing.bit_depth());
    // 16-bit containers may hold MSB-aligned samples (libcamera/PiSP `BYR2`) or LSB-aligned
    // ones (V4L2 `BG10`): samples above the declared depth mean MSB alignment.
    let (full, scaling) = match packing {
        RawPacking::U16Le { bits } if bits < 16 && max_seen >= 1 << bits => (
            65535.0,
            "16-bit, MSB-aligned (declared depth exceeded)".to_string(),
        ),
        _ => (
            ((1u64 << bits) - 1) as f64,
            format!("{bits}-bit full scale"),
        ),
    };
    let cells: [f64; 4] = std::array::from_fn(|i| sums[i] as f64 / counts[i].max(1) as f64 / full);
    let mut rgb = [0.0f64; 3];
    let mut rgb_n = [0.0f64; 3];
    for (i, mean) in cells.iter().enumerate() {
        let c = match pattern.channel_at(i & 1, i >> 1) {
            styx_softisp::Channel::Red => 0,
            styx_softisp::Channel::Green => 1,
            styx_softisp::Channel::Blue => 2,
        };
        rgb[c] += mean;
        rgb_n[c] += 1.0;
    }
    let n = values.len().max(1) as f64;
    let total: u64 = sums.iter().sum();
    Some(ImageStats {
        fourcc: code.to_string(),
        width: width as u32,
        height: height as u32,
        kind: format!("bayer {pattern:?}").to_lowercase(),
        scaling,
        mean: total as f64 / n / full,
        rgb_means: std::array::from_fn(|i| rgb[i] / rgb_n[i].max(1.0)),
        cell_means: Some(cells),
        yuv_means: None,
        clipped_fraction: values
            .iter()
            .filter(|&&v| f64::from(v) >= 0.98 * full)
            .count() as f64
            / n,
        dark_fraction: values
            .iter()
            .filter(|&&v| f64::from(v) <= 0.02 * full)
            .count() as f64
            / n,
    })
}

fn mean_u8(
    data: &[u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
    step: usize,
    off: usize,
) -> f64 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in 0..rows {
        let Some(row) = data.get(y * stride..y * stride + row_bytes) else {
            break;
        };
        for v in row.iter().skip(off).step_by(step) {
            sum += u64::from(*v);
            n += 1;
        }
    }
    sum as f64 / n.max(1) as f64
}

fn yuv_to_rgb([y, u, v]: [f64; 3]) -> [f64; 3] {
    // BT.601 limited range, on 0..255 inputs.
    let c = y - 16.0;
    let d = u - 128.0;
    let e = v - 128.0;
    let clamp = |x: f64| (x / 255.0).clamp(0.0, 1.0);
    [
        clamp(1.164 * c + 1.596 * e),
        clamp(1.164 * c - 0.392 * d - 0.813 * e),
        clamp(1.164 * c + 2.017 * d),
    ]
}

/// Statistics of an 8-bit YUV frame (NV12, NV21, YU12, YUYV, UYVY).
fn yuv_stats(code: FourCc, w: usize, h: usize, planes: &[PlaneRef<'_>]) -> Option<ImageStats> {
    let (y_plane, y_stride) = *planes.first()?;
    let tag = code.to_u32().to_le_bytes();
    let (luma, yuv): (Vec<u8>, [f64; 3]) = match &tag {
        b"NV12" | b"NV21" | b"YU12" => {
            let luma: Vec<u8> = (0..h)
                .flat_map(|y| y_plane.get(y * y_stride..y * y_stride + w).unwrap_or(&[]))
                .copied()
                .collect();
            // Chroma: a second plane, or after the luma rows in a single one.
            let rest = |idx: usize, offset: usize| -> Option<PlaneRef<'_>> {
                planes
                    .get(idx)
                    .copied()
                    .or_else(|| Some((y_plane.get(offset..)?, y_stride)))
            };
            let (cu, cv) = if &tag == b"YU12" {
                let (u, us) = rest(1, y_stride * h)?;
                let cstride = if planes.len() > 1 { us } else { y_stride / 2 };
                let (v, _) = match planes.get(2) {
                    Some(p) => *p,
                    None => (u.get(cstride * h / 2..)?, cstride),
                };
                (
                    mean_u8(u, cstride, w / 2, h / 2, 1, 0),
                    mean_u8(v, cstride, w / 2, h / 2, 1, 0),
                )
            } else {
                let (uv, s) = rest(1, y_stride * h)?;
                let a = mean_u8(uv, s, w, h / 2, 2, 0);
                let b = mean_u8(uv, s, w, h / 2, 2, 1);
                if &tag == b"NV12" { (a, b) } else { (b, a) }
            };
            let ym =
                luma.iter().map(|&v| u64::from(v)).sum::<u64>() as f64 / luma.len().max(1) as f64;
            (luma, [ym, cu, cv])
        }
        b"YUYV" | b"UYVY" => {
            let (yo, uo, vo) = if &tag == b"YUYV" {
                (0, 1, 3)
            } else {
                (1, 0, 2)
            };
            let luma: Vec<u8> = (0..h)
                .flat_map(|y| {
                    y_plane
                        .get(y * y_stride..y * y_stride + 2 * w)
                        .unwrap_or(&[])
                        .iter()
                        .skip(yo)
                        .step_by(2)
                })
                .copied()
                .collect();
            let ym =
                luma.iter().map(|&v| u64::from(v)).sum::<u64>() as f64 / luma.len().max(1) as f64;
            (
                luma,
                [
                    ym,
                    mean_u8(y_plane, y_stride, 2 * w, h, 4, uo),
                    mean_u8(y_plane, y_stride, 2 * w, h, 4, vo),
                ],
            )
        }
        _ => return None,
    };
    if luma.is_empty() {
        return None;
    }
    let n = luma.len() as f64;
    Some(ImageStats {
        fourcc: code.to_string(),
        width: w as u32,
        height: h as u32,
        kind: "yuv".into(),
        scaling: "8-bit luma / 255".into(),
        mean: yuv[0] / 255.0,
        rgb_means: yuv_to_rgb(yuv),
        cell_means: None,
        yuv_means: Some(yuv.map(|v| v / 255.0)),
        clipped_fraction: luma.iter().filter(|&&v| v >= 250).count() as f64 / n,
        dark_fraction: luma.iter().filter(|&&v| v <= 5).count() as f64 / n,
    })
}

/// Statistics of a frame, or a placeholder naming the unsupported format.
pub fn image_stats(
    code: FourCc,
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>],
    cfa: Option<CfaPattern>,
) -> ImageStats {
    let (w, h) = (width as usize, height as usize);
    let stats = if w == 0 || h == 0 || planes.is_empty() {
        None
    } else if bayer_fourcc(code).is_some() {
        bayer_stats(code, w, h, planes[0], cfa)
    } else {
        yuv_stats(code, w, h, planes)
    };
    stats.unwrap_or_else(|| ImageStats {
        fourcc: code.to_string(),
        width,
        height,
        kind: "unsupported".into(),
        ..ImageStats::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_raw10_bggr_channels() {
        // 4x2 BGGR, RAW10 packed: row 0 B G B G, row 1 G R G R.
        let (b, g, r) = (100u32, 512u32, 1000u32);
        let pack = |s: [u32; 4]| {
            let mut out: Vec<u8> = s.iter().map(|v| (v >> 2) as u8).collect();
            out.push(
                s.iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, v)| acc | (((v & 3) as u8) << (2 * i))),
            );
            out
        };
        let mut data = pack([b, g, b, g]);
        data.extend(pack([g, r, g, r]));
        let st = image_stats(FourCc::new(*b"pBAA"), 4, 2, &[(&data, 5)], None);
        assert_eq!(st.kind, "bayer bggr");
        let full = 1023.0;
        assert!(
            (st.rgb_means[0] - f64::from(r) / full).abs() < 1e-9,
            "{st:?}"
        );
        assert!((st.rgb_means[1] - f64::from(g) / full).abs() < 1e-9);
        assert!((st.rgb_means[2] - f64::from(b) / full).abs() < 1e-9);
        assert_eq!(st.clipped_fraction, 0.0);
    }

    #[test]
    fn msb_aligned_16_bit_is_detected() {
        let data: Vec<u8> = [0xFFC0u16, 0x8000, 0x8000, 0x4000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let st = image_stats(
            FourCc::new(*b"BG10"),
            2,
            2,
            &[(&data[..4], 4), (&[], 0)],
            None,
        );
        // Only the first row is valid with a 4-byte stride of a 2-row frame: too short.
        assert_eq!(st.kind, "unsupported");
        let st = image_stats(FourCc::new(*b"BG10"), 2, 2, &[(&data, 4)], None);
        assert!(st.scaling.contains("MSB"), "{st:?}");
        assert!((st.rgb_means[2] - f64::from(0xFFC0u16) / 65535.0).abs() < 1e-9);
    }

    #[test]
    fn nv12_grey_is_neutral() {
        let (w, h) = (4usize, 2usize);
        let mut data = vec![126u8; w * h];
        data.extend(vec![128u8; w * h / 2]);
        let st = image_stats(
            FourCc::new(*b"NV12"),
            w as u32,
            h as u32,
            &[(&data, w)],
            None,
        );
        assert_eq!(st.kind, "yuv");
        assert!((st.mean - 126.0 / 255.0).abs() < 1e-9);
        let [r, g, b] = st.rgb_means;
        assert!((r - g).abs() < 1e-9 && (g - b).abs() < 1e-9, "{st:?}");
    }

    #[test]
    fn cfa_override_swaps_channels() {
        let data = [10u8, 20, 30, 40];
        let st = image_stats(
            FourCc::new(*b"BA81"),
            2,
            2,
            &[(&data, 2)],
            Some(CfaPattern::Rggb),
        );
        assert!((st.rgb_means[0] - 10.0 / 255.0).abs() < 1e-9);
        assert!((st.rgb_means[2] - 40.0 / 255.0).abs() < 1e-9);
    }
}
