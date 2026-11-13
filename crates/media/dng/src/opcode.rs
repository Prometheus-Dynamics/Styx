//! DNG opcode lists (`OpcodeList1/2/3`, always big-endian) and the `GainMap` opcode, which
//! carries lens shading: a grid of gains over the image, per CFA channel.

use alloc::vec::Vec;

use crate::raw::CfaPattern;
use crate::{Result, malformed};

/// `GainMap`'s opcode id.
pub const GAIN_MAP: u32 = 9;
/// DNG version 1.3.0.0, when `GainMap` was introduced.
const VERSION_1_3: u32 = 0x0103_0000;
/// Flag: readers that do not know the opcode may skip it.
pub const FLAG_OPTIONAL: u32 = 1;
/// Flag: may be skipped for previews.
pub const FLAG_PREVIEW_SKIP: u32 = 2;

/// A grid of gains applied to an area of the image (DNG 1.3 opcode 9).
#[derive(Clone, Debug, PartialEq)]
pub struct GainMap {
    /// The area: top row.
    pub top: u32,
    /// Left column.
    pub left: u32,
    /// Bottom row (exclusive).
    pub bottom: u32,
    /// Right column (exclusive).
    pub right: u32,
    /// First image plane.
    pub plane: u32,
    /// Image planes.
    pub planes: u32,
    /// Rows stepped (2 for one CFA channel of a Bayer frame).
    pub row_pitch: u32,
    /// Columns stepped.
    pub col_pitch: u32,
    /// Map points down.
    pub points_v: u32,
    /// Map points across.
    pub points_h: u32,
    /// Spacing of map points down, relative to the image height (0..1).
    pub spacing_v: f64,
    /// Spacing across, relative to the image width.
    pub spacing_h: f64,
    /// First map point's row, relative.
    pub origin_v: f64,
    /// First map point's column, relative.
    pub origin_h: f64,
    /// Planes in the map.
    pub map_planes: u32,
    /// Gains, `[v][h][plane]`.
    pub gains: Vec<f32>,
}

impl GainMap {
    /// The map's gain at relative position (`v`, `h`) in `0..1` (bilinear between points,
    /// clamped at the edges), for map plane `p`.
    pub fn gain_at(&self, v: f64, h: f64, p: usize) -> f32 {
        let (nv, nh) = (self.points_v as usize, self.points_h as usize);
        if nv == 0 || nh == 0 || self.gains.is_empty() {
            return 1.0;
        }
        let pos = |x: f64, origin: f64, spacing: f64, n: usize| {
            let f = if spacing > 0.0 {
                (x - origin) / spacing
            } else {
                0.0
            };
            f.clamp(0.0, (n - 1) as f64)
        };
        let (fv, fh) = (
            pos(v, self.origin_v, self.spacing_v, nv),
            pos(h, self.origin_h, self.spacing_h, nh),
        );
        let (v0, h0) = (fv as usize, fh as usize);
        let (v1, h1) = ((v0 + 1).min(nv - 1), (h0 + 1).min(nh - 1));
        let (tv, th) = ((fv - v0 as f64) as f32, (fh - h0 as f64) as f32);
        let mp = self.map_planes.max(1) as usize;
        let p = p.min(mp - 1);
        let g = |r: usize, c: usize| {
            self.gains
                .get((r * nh + c) * mp + p)
                .copied()
                .unwrap_or(1.0)
        };
        let top = g(v0, h0) * (1.0 - th) + g(v0, h1) * th;
        let bottom = g(v1, h0) * (1.0 - th) + g(v1, h1) * th;
        top * (1.0 - tv) + bottom * tv
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for v in [
            self.top,
            self.left,
            self.bottom,
            self.right,
            self.plane,
            self.planes,
            self.row_pitch,
            self.col_pitch,
            self.points_v,
            self.points_h,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in [self.spacing_v, self.spacing_h, self.origin_v, self.origin_h] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&self.map_planes.to_be_bytes());
        for g in &self.gains {
            out.extend_from_slice(&g.to_be_bytes());
        }
    }

    fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < 76 {
            return Err(malformed("GainMap shorter than its header"));
        }
        let u = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let f = |i: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[i..i + 8]);
            f64::from_be_bytes(a)
        };
        let (points_v, points_h, map_planes) = (u(32), u(36), u(72));
        let n = (points_v as usize)
            .checked_mul(points_h as usize)
            .and_then(|n| n.checked_mul(map_planes as usize))
            .ok_or_else(|| malformed("GainMap size overflow"))?;
        let gains_bytes = b
            .get(76..)
            .filter(|g| g.len() / 4 >= n)
            .ok_or_else(|| malformed("GainMap shorter than its gains"))?;
        Ok(Self {
            top: u(0),
            left: u(4),
            bottom: u(8),
            right: u(12),
            plane: u(16),
            planes: u(20),
            row_pitch: u(24),
            col_pitch: u(28),
            points_v,
            points_h,
            spacing_v: f(40),
            spacing_h: f(48),
            origin_v: f(56),
            origin_h: f(64),
            map_planes,
            gains: gains_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .take(n)
                .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        })
    }
}

/// One opcode of a list.
#[derive(Clone, Debug, PartialEq)]
pub enum Opcode {
    /// A gain map.
    GainMap {
        /// The map.
        map: GainMap,
        /// Opcode flags ([`FLAG_OPTIONAL`], [`FLAG_PREVIEW_SKIP`]).
        flags: u32,
    },
    /// Any other opcode, kept as bytes.
    Other {
        /// Opcode id.
        id: u32,
        /// DNG version it needs.
        version: u32,
        /// Flags.
        flags: u32,
        /// Parameters.
        data: Vec<u8>,
    },
}

/// An opcode list's bytes.
pub fn encode_list(ops: &[Opcode]) -> Vec<u8> {
    let mut out = (ops.len() as u32).to_be_bytes().to_vec();
    for op in ops {
        let (id, version, flags, data) = match op {
            Opcode::GainMap { map, flags } => {
                let mut d = Vec::with_capacity(76 + map.gains.len() * 4);
                map.encode(&mut d);
                (GAIN_MAP, VERSION_1_3, *flags, d)
            }
            Opcode::Other {
                id,
                version,
                flags,
                data,
            } => (*id, *version, *flags, data.clone()),
        };
        for v in [id, version, flags, data.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&data);
    }
    out
}

/// An opcode list from its bytes.
pub fn decode_list(b: &[u8]) -> Result<Vec<Opcode>> {
    let u = |i: usize| {
        b.get(i..i + 4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .ok_or_else(|| malformed("opcode list truncated"))
    };
    let count = u(0)? as usize;
    let mut ops = Vec::new();
    let mut at = 4;
    for _ in 0..count {
        let (id, version, flags, len) = (u(at)?, u(at + 4)?, u(at + 8)?, u(at + 12)? as usize);
        let data = b
            .get(at + 16..at + 16 + len)
            .ok_or_else(|| malformed("opcode truncated"))?;
        ops.push(if id == GAIN_MAP {
            Opcode::GainMap {
                map: GainMap::decode(data)?,
                flags,
            }
        } else {
            Opcode::Other {
                id,
                version,
                flags,
                data: data.to_vec(),
            }
        });
        at += 16 + len;
    }
    Ok(ops)
}

/// Lens shading for a `width` x `height` Bayer frame as four `GainMap`s, one per CFA channel
/// (red, the two greens, blue): `grid_w` x `grid_h` gains per colour (row-major, cell
/// centres evenly over the frame, as ISP lens shading tables are laid out), bilinear between
/// them and clamped at the edges.
pub fn bayer_gain_maps(
    pattern: CfaPattern,
    (width, height): (u32, u32),
    (grid_w, grid_h): (u32, u32),
    [r, g, b]: [&[f64]; 3],
) -> Vec<Opcode> {
    let n = (grid_w * grid_h) as usize;
    if grid_w == 0 || grid_h == 0 || [r, g, b].iter().any(|t| t.len() != n) {
        return Vec::new();
    }
    let tables = [r, g, g, b];
    pattern
        .offsets()
        .iter()
        .zip(tables)
        .map(|(&(top, left), t)| Opcode::GainMap {
            map: GainMap {
                top,
                left,
                bottom: height,
                right: width,
                plane: 0,
                planes: 1,
                row_pitch: 2,
                col_pitch: 2,
                points_v: grid_h,
                points_h: grid_w,
                spacing_v: 1.0 / f64::from(grid_h),
                spacing_h: 1.0 / f64::from(grid_w),
                origin_v: 0.5 / f64::from(grid_h),
                origin_h: 0.5 / f64::from(grid_w),
                map_planes: 1,
                gains: t.iter().map(|&v| v as f32).collect(),
            },
            flags: FLAG_OPTIONAL,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_maps_round_trip_and_interpolate() {
        let r: Vec<f64> = (0..6).map(|i| 1.0 + f64::from(i)).collect();
        let ones = vec![1.0; 6];
        let ops = bayer_gain_maps(CfaPattern::Bggr, (64, 32), (3, 2), [&r, &ones, &ones]);
        assert_eq!(ops.len(), 4);
        let bytes = encode_list(&ops);
        let back = decode_list(&bytes).unwrap();
        assert_eq!(back, ops);
        let Opcode::GainMap { map, flags } = &back[0] else {
            panic!("not a gain map");
        };
        assert_eq!(*flags, FLAG_OPTIONAL);
        // Red of BGGR starts at (1, 1).
        assert_eq!((map.top, map.left, map.row_pitch), (1, 1, 2));
        // Cell centres: first at 1/6 across, 1/4 down; clamped outside, bilinear inside.
        assert_eq!(map.gain_at(0.0, 0.0, 0), 1.0);
        assert_eq!(map.gain_at(0.25, 1.0 / 6.0 + 1.0 / 3.0, 0), 2.0);
        assert!((map.gain_at(0.5, 1.0 / 6.0, 0) - 2.5).abs() < 1e-6);
        assert_eq!(map.gain_at(1.0, 1.0, 0), 6.0);
        assert!(bayer_gain_maps(CfaPattern::Bggr, (8, 8), (3, 2), [&r, &r, &ones[..5]]).is_empty());
    }

    #[test]
    fn unknown_opcodes_survive_and_truncation_is_an_error() {
        let ops = vec![Opcode::Other {
            id: 1,
            version: VERSION_1_3,
            flags: 0,
            data: vec![1, 2, 3],
        }];
        let bytes = encode_list(&ops);
        assert_eq!(decode_list(&bytes).unwrap(), ops);
        assert!(decode_list(&bytes[..bytes.len() - 1]).is_err());
        assert!(decode_list(&[0, 0]).is_err());
    }
}
