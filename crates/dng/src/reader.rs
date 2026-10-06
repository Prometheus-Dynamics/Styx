//! Reading DNG files: the raw image and what calibration needs from the metadata.

use alloc::{format, string::String, vec, vec::Vec};

use crate::color::Matrix3;
use crate::opcode::{self, Opcode};
use crate::raw::CfaPattern;
use crate::tiff::tag;
use crate::tiff_read::{Ifd, Order, Tiff};
use crate::{DngError, Result, malformed};

/// Colour calibration under one light, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    /// `CalibrationIlluminant` code (see [`crate::Illuminant::from_code`]).
    pub illuminant: u16,
    /// XYZ → camera.
    pub color_matrix: Option<Matrix3>,
    /// White-balanced camera → XYZ D50.
    pub forward_matrix: Option<Matrix3>,
    /// Per-unit camera calibration (applied before `color_matrix`).
    pub camera_calibration: Option<Matrix3>,
}

/// A DNG's raw image and metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct DngFile {
    /// `DNGVersion`.
    pub version: [u8; 4],
    /// Width of the raw image.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Stored bits per sample.
    pub bits_per_sample: u16,
    /// Samples per pixel (1 for CFA and monochrome data, 3 for linear RGB).
    pub samples_per_pixel: u16,
    /// `PhotometricInterpretation` (32803 CFA, 34892 linear raw).
    pub photometric: u16,
    /// The Bayer pattern, if the CFA is a 2x2 Bayer pattern.
    pub cfa: Option<CfaPattern>,
    /// CFA repeat size (rows, columns) and colours (0 red, 1 green, 2 blue, ...).
    pub cfa_dims: (u16, u16),
    /// CFA colours, row-major over `cfa_dims`.
    pub cfa_colors: Vec<u8>,
    /// Samples, row-major (`width * height * samples_per_pixel`), with any linearization
    /// table applied.
    pub samples: Vec<u16>,
    /// Black level repeat size (rows, columns).
    pub black_repeat: (u16, u16),
    /// Black levels over the repeat pattern (row-major; per sample for `samples_per_pixel` > 1).
    pub black_level: Vec<f64>,
    /// Black level offsets per column / per row (`BlackLevelDeltaH` / `V`).
    pub black_delta_h: Vec<f64>,
    /// Per row.
    pub black_delta_v: Vec<f64>,
    /// White level per sample.
    pub white_level: Vec<u32>,
    /// Active area (top, left, bottom, right).
    pub active_area: Option<[u32; 4]>,
    /// As-shot neutral (camera space).
    pub as_shot_neutral: Option<Vec<f64>>,
    /// As-shot white (xy), the alternative to the neutral.
    pub as_shot_white_xy: Option<(f64, f64)>,
    /// Analogue balance.
    pub analog_balance: Option<Vec<f64>>,
    /// Calibrations (one or two).
    pub calibrations: Vec<Calibration>,
    /// Baseline exposure (stops).
    pub baseline_exposure: Option<f64>,
    /// Exposure time (seconds).
    pub exposure_time: Option<f64>,
    /// ISO speed.
    pub iso: Option<u32>,
    /// F-number.
    pub f_number: Option<f64>,
    /// Make.
    pub make: Option<String>,
    /// Model.
    pub model: Option<String>,
    /// Unique camera model.
    pub unique_camera_model: Option<String>,
    /// `DateTimeOriginal`.
    pub date_time_original: Option<String>,
    /// Image description.
    pub description: Option<String>,
    /// Orientation.
    pub orientation: u16,
    /// Opcode lists 1 (raw, before linearization), 2 (after black/white), 3 (after demosaic).
    pub opcode_lists: [Vec<Opcode>; 3],
}

impl DngFile {
    /// Sample at (`x`, `y`), plane `s`.
    pub fn sample(&self, x: u32, y: u32, s: u16) -> u16 {
        let spp = usize::from(self.samples_per_pixel);
        self.samples[(y as usize * self.width as usize + x as usize) * spp + usize::from(s)]
    }

    /// Black level at (`x`, `y`), plane `s` (repeat pattern from the active area's corner,
    /// plus the row and column deltas).
    pub fn black_at(&self, x: u32, y: u32, s: u16) -> f64 {
        let (top, left) = self.active_area.map_or((0, 0), |a| (a[0], a[1]));
        let (rx, ry) = (x.saturating_sub(left), y.saturating_sub(top));
        let (rows, cols) = (
            u32::from(self.black_repeat.0.max(1)),
            u32::from(self.black_repeat.1.max(1)),
        );
        let spp = usize::from(self.samples_per_pixel.max(1));
        let i = (((ry % rows) * cols + rx % cols) as usize) * spp + usize::from(s);
        let base = self
            .black_level
            .get(i)
            .or(self.black_level.first())
            .copied()
            .unwrap_or(0.0);
        base + self.black_delta_h.get(rx as usize).copied().unwrap_or(0.0)
            + self.black_delta_v.get(ry as usize).copied().unwrap_or(0.0)
    }

    /// CFA colour of pixel (`x`, `y`) (0 red, 1 green, 2 blue), for CFA images.
    pub fn color_at(&self, x: u32, y: u32) -> Option<u8> {
        let (rows, cols) = (u32::from(self.cfa_dims.0), u32::from(self.cfa_dims.1));
        if rows == 0 || cols == 0 {
            return None;
        }
        let (top, left) = self.active_area.map_or((0, 0), |a| (a[0], a[1]));
        let (rx, ry) = (x.saturating_sub(left), y.saturating_sub(top));
        self.cfa_colors
            .get(((ry % rows) * cols + rx % cols) as usize)
            .copied()
    }

    /// The `GainMap`s of opcode list 2 (lens shading), if any.
    pub fn gain_maps(&self) -> Vec<&opcode::GainMap> {
        self.opcode_lists[1]
            .iter()
            .filter_map(|o| match o {
                Opcode::GainMap { map, .. } => Some(map),
                Opcode::Other { .. } => None,
            })
            .collect()
    }
}

fn matrix(ifd: &Ifd, t: u16) -> Option<Matrix3> {
    let v = ifd.f64s(t)?;
    (v.len() >= 9).then(|| core::array::from_fn(|i| v[i]))
}

/// The IFD holding the full-resolution raw image: `NewSubFileType` 0 with CFA or linear raw
/// data, in IFD 0, its SubIFDs or the IFDs chained after it.
fn find_raw(t: &Tiff<'_>) -> Result<Ifd> {
    let mut queue = vec![t.first];
    let mut seen = Vec::new();
    while let Some(off) = queue.pop() {
        if off == 0 || seen.contains(&off) || seen.len() > 64 {
            continue;
        }
        seen.push(off);
        let ifd = t.ifd(off)?;
        let kind = ifd.u32(tag::NEW_SUBFILE_TYPE).unwrap_or(0);
        let photometric = ifd.u32(tag::PHOTOMETRIC).unwrap_or(0);
        if kind == 0 && (photometric == 32803 || photometric == 34892) {
            return Ok(ifd);
        }
        if let Some(subs) = ifd.u32s(tag::SUB_IFDS) {
            queue.extend(subs.into_iter().rev().map(|o| o as usize));
        }
        if let Ok(next) = t.next_ifd(off) {
            queue.insert(0, next);
        }
    }
    Err(malformed(
        "no raw image (NewSubFileType 0, CFA or linear raw)",
    ))
}

/// Unpacks `count` samples of `bits` bits from `src` (8 and 16 in the file's byte order,
/// others packed most significant bit first, as DNG stores them).
fn unpack(src: &[u8], count: usize, bits: u16, order: Order) -> Vec<u16> {
    match bits {
        8 => src.iter().take(count).map(|&b| u16::from(b)).collect(),
        16 => src
            .as_chunks::<2>()
            .0
            .iter()
            .take(count)
            .map(|c| order.u16(c))
            .collect(),
        _ => {
            let mut out = Vec::with_capacity(count);
            let (mut acc, mut n) = (0u64, 0u32);
            let b = u32::from(bits);
            for &byte in src {
                acc = (acc << 8) | u64::from(byte);
                n += 8;
                while n >= b && out.len() < count {
                    out.push(((acc >> (n - b)) & ((1 << b) - 1)) as u16);
                    n -= b;
                }
                if out.len() == count {
                    break;
                }
            }
            out
        }
    }
}

/// Decodes one strip or tile of `tw` x `th` pixels.
fn decode_block(
    data: &[u8],
    compression: u32,
    (tw, th, spp): (usize, usize, usize),
    bits: u16,
    order: Order,
) -> Result<Vec<u16>> {
    match compression {
        1 => {
            // Rows start on byte boundaries.
            let row_bytes = (tw * spp * usize::from(bits)).div_ceil(8);
            let mut out = Vec::with_capacity(tw * th * spp);
            for r in 0..th {
                let row = data.get(r * row_bytes..(r + 1) * row_bytes).unwrap_or(&[]);
                let mut v = unpack(row, tw * spp, bits, order);
                v.resize(tw * spp, 0);
                out.extend_from_slice(&v);
            }
            Ok(out)
        }
        7 => {
            let d = crate::ljpeg::decode(data)?;
            let mut v = d.samples;
            v.resize(tw * th * spp, 0);
            Ok(v)
        }
        c => Err(DngError::Unsupported(format!("compression {c}"))),
    }
}

/// Most samples a raw image may have (1 G).
const MAX_SAMPLES: usize = 1 << 30;

fn image(t: &Tiff<'_>, ifd: &Ifd, (w, h, spp): (usize, usize, usize)) -> Result<Vec<u16>> {
    let bits = ifd.u32(tag::BITS_PER_SAMPLE).unwrap_or(8) as u16;
    if !(1..=16).contains(&bits) {
        return Err(DngError::Unsupported(format!("{bits} bits per sample")));
    }
    let compression = ifd.u32(tag::COMPRESSION).unwrap_or(1);
    let total = w
        .checked_mul(h)
        .and_then(|v| v.checked_mul(spp))
        .filter(|&v| v <= MAX_SAMPLES && v > 0)
        .ok_or_else(|| malformed(format!("image {w}x{h}x{spp}")))?;
    let mut out = vec![0u16; total];
    let (tw, th, offsets, counts) = if let Some(tw) = ifd.u32(tag::TILE_WIDTH) {
        (
            tw as usize,
            ifd.u32(tag::TILE_LENGTH).unwrap_or(0) as usize,
            ifd.u32s(tag::TILE_OFFSETS),
            ifd.u32s(tag::TILE_BYTE_COUNTS),
        )
    } else {
        (
            w,
            (ifd.u32(tag::ROWS_PER_STRIP).unwrap_or(h as u32) as usize).min(h),
            ifd.u32s(tag::STRIP_OFFSETS),
            ifd.u32s(tag::STRIP_BYTE_COUNTS),
        )
    };
    let (offsets, counts) = offsets
        .zip(counts)
        .ok_or_else(|| malformed("no strip or tile offsets"))?;
    if tw == 0 || th == 0 || tw.saturating_mul(th) > MAX_SAMPLES {
        return Err(malformed(format!("tiles of {tw}x{th}")));
    }
    let across = w.div_ceil(tw);
    let blocks = across * h.div_ceil(th);
    if offsets.len() < blocks || counts.len() < blocks {
        return Err(malformed(format!(
            "{} blocks for {blocks} needed",
            offsets.len()
        )));
    }
    for i in 0..blocks {
        let data = t.slice(offsets[i] as usize, counts[i] as usize)?;
        let block = decode_block(data, compression, (tw, th, spp), bits, t.order)?;
        let (x0, y0) = ((i % across) * tw, (i / across) * th);
        let cols = tw.min(w - x0) * spp;
        for r in 0..th.min(h - y0) {
            let dst = ((y0 + r) * w + x0) * spp;
            out[dst..dst + cols].copy_from_slice(&block[r * tw * spp..r * tw * spp + cols]);
        }
    }
    if let Some(table) = ifd.u32s(tag::LINEARIZATION_TABLE).filter(|t| !t.is_empty()) {
        let last = table.len() - 1;
        for v in &mut out {
            *v = table[usize::from(*v).min(last)].min(65535) as u16;
        }
    }
    Ok(out)
}

/// Reads a DNG file from its bytes.
pub fn read_dng(bytes: &[u8]) -> Result<DngFile> {
    let t = Tiff::parse(bytes)?;
    let ifd0 = t.ifd(t.first)?;
    let version = ifd0
        .bytes(tag::DNG_VERSION)
        .filter(|b| b.len() >= 4)
        .map(|b| [b[0], b[1], b[2], b[3]])
        .ok_or_else(|| malformed("no DNGVersion: not a DNG file"))?;
    let raw = find_raw(&t)?;
    let width = raw
        .u32(tag::IMAGE_WIDTH)
        .ok_or_else(|| malformed("no width"))?;
    let height = raw
        .u32(tag::IMAGE_LENGTH)
        .ok_or_else(|| malformed("no height"))?;
    let spp = raw.u32(tag::SAMPLES_PER_PIXEL).unwrap_or(1).clamp(1, 4) as u16;
    let samples = image(
        &t,
        &raw,
        (width as usize, height as usize, usize::from(spp)),
    )?;
    let photometric = raw.u32(tag::PHOTOMETRIC).unwrap_or(0) as u16;
    let (cfa_dims, cfa_colors) = if photometric == 32803 {
        let d = raw.u32s(tag::CFA_REPEAT_PATTERN_DIM).unwrap_or(vec![2, 2]);
        let dims = (
            *d.first().unwrap_or(&2) as u16,
            *d.get(1).unwrap_or(&2) as u16,
        );
        let colors = raw
            .bytes(tag::CFA_PATTERN)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        (dims, colors)
    } else {
        ((0, 0), Vec::new())
    };
    let cfa = (cfa_dims == (2, 2))
        .then(|| CfaPattern::from_colors(&cfa_colors))
        .flatten();
    let repeat = raw.u32s(tag::BLACK_LEVEL_REPEAT_DIM).unwrap_or(vec![1, 1]);
    let exif = ifd0.u32(tag::EXIF_IFD).and_then(|o| t.ifd(o as usize).ok());
    // EXIF tags sometimes sit in IFD 0 itself.
    let exif_f64 = |tg: u16| {
        exif.as_ref()
            .and_then(|e| e.f64s(tg))
            .or_else(|| ifd0.f64s(tg))
            .and_then(|v| v.first().copied())
    };
    let calibrations = [
        (
            tag::CALIBRATION_ILLUMINANT_1,
            tag::COLOR_MATRIX_1,
            tag::FORWARD_MATRIX_1,
            tag::CAMERA_CALIBRATION_1,
        ),
        (
            tag::CALIBRATION_ILLUMINANT_2,
            tag::COLOR_MATRIX_2,
            tag::FORWARD_MATRIX_2,
            tag::CAMERA_CALIBRATION_2,
        ),
    ]
    .iter()
    .filter_map(|&(ill, cm, fm, cc)| {
        let color_matrix = matrix(&ifd0, cm);
        let forward_matrix = matrix(&ifd0, fm);
        (color_matrix.is_some() || forward_matrix.is_some()).then(|| Calibration {
            illuminant: ifd0.u32(ill).unwrap_or(0) as u16,
            color_matrix,
            forward_matrix,
            camera_calibration: matrix(&ifd0, cc),
        })
    })
    .collect();
    let opcodes = |tg: u16| {
        raw.bytes(tg)
            .map(opcode::decode_list)
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let white = raw
        .u32s(tag::WHITE_LEVEL)
        .unwrap_or_else(|| vec![(1u32 << raw.u32(tag::BITS_PER_SAMPLE).unwrap_or(16)) - 1]);
    Ok(DngFile {
        version,
        width,
        height,
        bits_per_sample: raw.u32(tag::BITS_PER_SAMPLE).unwrap_or(8) as u16,
        samples_per_pixel: spp,
        photometric,
        cfa,
        cfa_dims,
        cfa_colors,
        samples,
        black_repeat: (
            *repeat.first().unwrap_or(&1) as u16,
            *repeat.get(1).unwrap_or(&1) as u16,
        ),
        black_level: raw.f64s(tag::BLACK_LEVEL).unwrap_or(vec![0.0]),
        black_delta_h: raw.f64s(tag::BLACK_LEVEL_DELTA_H).unwrap_or_default(),
        black_delta_v: raw.f64s(tag::BLACK_LEVEL_DELTA_V).unwrap_or_default(),
        white_level: white,
        active_area: raw
            .u32s(tag::ACTIVE_AREA)
            .filter(|a| a.len() >= 4)
            .map(|a| [a[0], a[1], a[2], a[3]]),
        as_shot_neutral: ifd0.f64s(tag::AS_SHOT_NEUTRAL),
        as_shot_white_xy: ifd0
            .f64s(tag::AS_SHOT_WHITE_XY)
            .filter(|v| v.len() >= 2)
            .map(|v| (v[0], v[1])),
        analog_balance: ifd0.f64s(tag::ANALOG_BALANCE),
        calibrations,
        baseline_exposure: ifd0
            .f64s(tag::BASELINE_EXPOSURE)
            .and_then(|v| v.first().copied()),
        exposure_time: exif_f64(tag::EXPOSURE_TIME),
        iso: exif_f64(tag::ISO_SPEED_RATINGS).map(|v| v as u32),
        f_number: exif_f64(tag::F_NUMBER),
        make: ifd0.ascii(tag::MAKE),
        model: ifd0.ascii(tag::MODEL),
        unique_camera_model: ifd0.ascii(tag::UNIQUE_CAMERA_MODEL),
        date_time_original: exif.as_ref().and_then(|e| e.ascii(tag::DATE_TIME_ORIGINAL)),
        description: ifd0.ascii(tag::IMAGE_DESCRIPTION),
        orientation: ifd0.u32(tag::ORIENTATION).unwrap_or(1) as u16,
        opcode_lists: [
            opcodes(tag::OPCODE_LIST_1)?,
            opcodes(tag::OPCODE_LIST_2)?,
            opcodes(tag::OPCODE_LIST_3)?,
        ],
    })
}

#[cfg(test)]
mod tests;
