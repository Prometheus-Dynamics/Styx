//! Re-exposing recorded raw frames, `no_std`: a recorded frame scaled to the exposure and
//! gain a frame asked for (light above black scales linearly, clipped at full scale), the
//! virtual sensor's arithmetic ([`crate::replay::VirtualSensor`] on a host, a replay receiver
//! in firmware tests) written once so both give the same samples bit for bit.
//!
//! Exact for exposure steps down to the recorded level and for pixels the recording did not
//! clip; brighter re-exposures of clipped pixels stay clipped (as they would on the sensor),
//! darker ones keep the clipped value's scaled level.

#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;
use styx_softisp::RawPacking;

use crate::controller::SensorValues;

/// Unpacks one row of `width` samples to 16-bit values (at their bit depth, not shifted).
pub fn unpack_row(packing: RawPacking, row: &[u8], width: usize, out: &mut [u16]) {
    match packing {
        RawPacking::U8 => {
            for (o, &b) in out[..width].iter_mut().zip(row) {
                *o = u16::from(b);
            }
        }
        RawPacking::U16Le { .. } => {
            for (o, c) in out[..width].iter_mut().zip(row.as_chunks::<2>().0) {
                *o = u16::from_le_bytes([c[0], c[1]]);
            }
        }
        RawPacking::Csi2Raw10 => {
            for (i, o) in out[..width].iter_mut().enumerate() {
                let g = (i / 4) * 5;
                let lsb = (row[g + 4] >> ((i % 4) * 2)) & 3;
                *o = (u16::from(row[g + i % 4]) << 2) | u16::from(lsb);
            }
        }
        RawPacking::Csi2Raw12 => {
            for (i, o) in out[..width].iter_mut().enumerate() {
                let g = (i / 2) * 3;
                let lsb = (row[g + 2] >> ((i % 2) * 4)) & 0xf;
                *o = (u16::from(row[g + i % 2]) << 4) | u16::from(lsb);
            }
        }
    }
}

/// The light of a frame produced with `current` relative to one recorded with `recorded`
/// (total exposures), times the scene's `brightness`.
pub fn exposure_ratio(current: &SensorValues, recorded: &SensorValues, brightness: f64) -> f64 {
    current.total_exposure() / recorded.total_exposure().max(1e-12) * brightness
}

/// A recorded frame's geometry and levels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Recorded {
    /// How its rows store samples.
    pub packing: RawPacking,
    /// Bytes per row.
    pub stride: usize,
    /// Width.
    pub width: usize,
    /// Height.
    pub height: usize,
    /// Black level, normalised (full scale 1.0).
    pub black_level: f64,
}

/// Writes `src` (laid out as `rec` says) re-exposed by `k` ([`exposure_ratio`]) into `out` as
/// 16-bit little-endian samples at the recording's bit depth, `width * 2` bytes per row;
/// `row` is scratch of at least `width` samples.
pub fn re_expose(src: &[u8], rec: &Recorded, k: f64, out: &mut [u8], row: &mut [u16]) {
    let bits = rec.packing.bit_depth();
    let full = f64::from((1u32 << bits) - 1);
    let black = (rec.black_level * f64::from(1u32 << bits)).round();
    let w = rec.width;
    for y in 0..rec.height {
        unpack_row(rec.packing, &src[y * rec.stride..], w, row);
        let dst = &mut out[y * w * 2..(y + 1) * w * 2];
        for (x, &v) in row[..w].iter().enumerate() {
            let v = f64::from(v);
            let o = if v >= full {
                full
            } else {
                (black + (v - black) * k).clamp(0.0, full)
            };
            dst[2 * x..2 * x + 2].copy_from_slice(&(o.round() as u16).to_le_bytes());
        }
    }
}
