//! The register writes for one frame's 3A settings ([`Params`]): black level, white balance,
//! colour matrix, output gamma, lens shading, and the AE, histogram and AWB windows.
//!
//! The builder does not read the ISP: it produces an ordered list of [`RegWrite`]s whose
//! values are clamped to their fields. [`Params::apply`] writes them through a
//! [`RegisterAccess`], then sets the control bits (the gamma enable) by read-modify-write of
//! `isp_ctrl` and strobes the shadow update.

use alloc::vec::Vec;

// Float methods for builds without std (with std the inherent ones are used).
#[allow(unused_imports)]
use styx_math::Float as _;

use crate::RegisterAccess;
use crate::regs::{self as r, CC_COEFF_MASK, CT_OFFSET_MASK};

/// White balance gain 1.0 (`mrv_all_bits.h`: "gain value ... 100h = 1").
pub const GAIN_ONE: f32 = 256.0;
/// The largest gain a 10-bit field holds (3.996).
pub const GAIN_MAX: f32 = 1023.0 / GAIN_ONE;
/// Colour matrix coefficient 1.0 (`mrv_all_bits.h`: "0 is represented as 0x000 and a coefficient
/// value of 1 as 0x080": 7 fractional bits).
pub const CCM_ONE: f32 = 128.0;
/// The largest colour matrix coefficient: a 9-bit two's complement field (+1.99).
pub const CCM_MAX: i32 = 255;
/// The smallest colour matrix coefficient (-2.0).
pub const CCM_MIN: i32 = -256;
/// The largest cross-talk offset: a 12-bit two's complement field.
pub const CT_OFFSET_MAX: i32 = 2047;
/// The smallest cross-talk offset.
pub const CT_OFFSET_MIN: i32 = -2048;
/// Lens shading gain 1.0: 2 integer and 10 fractional bits (`mrv_all_bits.h`: "range 1..3.999").
pub const LSC_ONE: f64 = 1024.0;
/// Full scale of a lens shading sample (12 bits).
pub const LSC_MAX: f64 = 4095.0;
/// Sectors per axis of the lens shading: eight sector sizes and gradients per axis
/// (`isp_lsc_xsize_01` to `_67`), so nine vertices.
pub const LSC_SECTORS: usize = 8;
/// Table rows of the lens shading samples, the words per row, and the samples per row. The
/// table is 153 words per colour and bank (`LSC_R_RAM_ADDR_MAX_BANK0` is 152): 17 rows of
/// nine words, two samples to a word. Nine rows and nine columns of the 17 x 18 hold the
/// vertices, the rest stays at unity. This layout is a guess, confirm on a board.
pub const LSC_ROWS: usize = 17;
/// See [`LSC_ROWS`].
pub const LSC_WORDS_PER_ROW: usize = 9;
/// See [`LSC_ROWS`].
pub const LSC_COLS: usize = 2 * LSC_WORDS_PER_ROW;
/// The gamma-out curve's points: 16 equal segments (`equ_segm`), so 17 points.
pub const GAMMA_POINTS: usize = 17;
/// Full scale of the gamma-out points (10 bits).
pub const GAMMA_OUT_MAX: f32 = 1023.0;
/// The largest step between two gamma-out points: a 10-bit signed field, +511.
pub const GAMMA_DELTA_MAX: i32 = 511;
/// The smallest step between two gamma-out points (-512).
pub const GAMMA_DELTA_MIN: i32 = -512;
/// Largest black level subtracted (the 13-bit fixed field's positive range).
pub const BLS_MAX: i32 = 4095;
/// Smallest black level (the field's negative range).
pub const BLS_MIN: i32 = -4096;
/// Histogram step size: the header says 0 to 2 are not allowed, so the smallest, 3, samples
/// every third pixel of each sub-window. Unconfirmed.
pub const HIST_STEPSIZE: u32 = 3;

/// One register write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegWrite {
    /// Register address (offset within the register window).
    pub addr: u32,
    /// Value written.
    pub value: u32,
}

/// The register writes for one frame's 3A settings.
///
/// Each builder method appends its writes; a later call that touches the same register
/// overrides the earlier one in the device. [`Params::writes`] is the list; the control bits
/// that need a read-modify-write of `isp_ctrl` are kept apart and applied by
/// [`Params::apply`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    writes: Vec<RegWrite>,
    ctrl: u32,
}

impl Params {
    /// No writes yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same `Params` with no writes, its list's capacity kept: one value reused frame to
    /// frame builds each frame's writes without allocating once the list has grown.
    pub fn reset(mut self) -> Self {
        self.writes.clear();
        self.ctrl = 0;
        self
    }

    /// The writes, in the order they are applied.
    pub fn writes(&self) -> &[RegWrite] {
        &self.writes
    }

    /// The `isp_ctrl` bits [`Params::apply`] sets on top of the current value (the gamma
    /// enable); zero when none is needed.
    pub fn ctrl_bits(&self) -> u32 {
        self.ctrl
    }

    fn put(&mut self, addr: u32, value: u32) {
        self.writes.push(RegWrite { addr, value });
    }

    /// The statistics Styx's 3A reads over a `width` x `height` raw frame: the AE grid's 5x5
    /// blocks of luma means, the histogram of luma over the same blocks, and the AWB means over
    /// the whole frame (RGB measurement mode). Blocks are `width / 5` by `height / 5`, rounded
    /// down to even as the hardware needs.
    pub fn measure(mut self, width: u32, height: u32) -> Self {
        let block = |len: u32| (len / r::AE_GRID) & !1;
        let (bw, bh) = (block(width), block(height));
        self.put(r::AE_CTRL, r::AE_CTRL_LUMA_85_256);
        self.put(r::AE_H_OFFSET, 0);
        self.put(r::AE_V_OFFSET, 0);
        self.put(r::AE_H_SIZE, bw);
        self.put(r::AE_V_SIZE, bh);
        self.put(
            r::HIST_PROP,
            (HIST_STEPSIZE << r::HIST_STEPSIZE_SHIFT) | r::HIST_MODE_Y,
        );
        self.put(r::HIST_H_OFFS, 0);
        self.put(r::HIST_V_OFFS, 0);
        self.put(r::HIST_H_SIZE, bw);
        self.put(r::HIST_V_SIZE, bh);
        for n in 0..6 {
            self.put(r::HIST_WEIGHTS_0 + 4 * n, r::HIST_WEIGHT_ONE_PACKED);
        }
        self.put(r::HIST_WEIGHT_44, 1);
        self.put(r::ISP_AWB_PROP, r::AWB_PROP_RGB_MEAS);
        self.put(r::AWB_H_OFFS, 0);
        self.put(r::AWB_V_OFFS, 0);
        self.put(r::AWB_H_SIZE, width);
        self.put(r::AWB_V_SIZE, height);
        self
    }

    /// Black level to subtract, per Bayer position (top-left, top-right, bottom-left,
    /// bottom-right, i.e. R, Gr, Gb, B of an RGGB mosaic), in 12-bit sensor units. The levels
    /// are clamped to the 13-bit signed field.
    pub fn black_level(mut self, levels: [u16; 4]) -> Self {
        let field = |l: u16| (i32::from(l).clamp(BLS_MIN, BLS_MAX) as u32) & r::BLS_FIXED_MASK;
        self.put(r::BLS_CTRL, r::BLS_ENABLE);
        self.put(r::BLS_A_FIXED, field(levels[0]));
        self.put(r::BLS_B_FIXED, field(levels[1]));
        self.put(r::BLS_C_FIXED, field(levels[2]));
        self.put(r::BLS_D_FIXED, field(levels[3]));
        self
    }

    /// White balance gains (red, green, blue; 1.0 is none), clamped to 1/256..=[`GAIN_MAX`] (a
    /// zero gain would blank the channel, and the rkisp drivers refuse one, so it is written as
    /// the smallest step, as `styx-rkisp` does). The green gain goes to both green positions.
    pub fn white_balance(mut self, gains: [f32; 3]) -> Self {
        let q =
            |g: f32| ((g.clamp(0.0, GAIN_MAX) * GAIN_ONE + 0.5) as u32).clamp(1, r::AWB_GAIN_MASK);
        let (red, green, blue) = (q(gains[0]), q(gains[1]), q(gains[2]));
        self.put(r::ISP_AWB_GAIN_G, (green << r::AWB_GAIN_HI_SHIFT) | green);
        self.put(r::ISP_AWB_GAIN_RB, (red << r::AWB_GAIN_HI_SHIFT) | blue);
        self
    }

    /// The colour matrix (rows: output R, G, B; columns: input R, G, B) and its offsets, in
    /// the registers' units. Coefficients are clamped to the 9-bit field (-2.0 to +1.99); the
    /// offsets to the 12-bit field.
    pub fn colour_matrix(mut self, m: [[f32; 3]; 3], offsets: [f32; 3]) -> Self {
        let coeff = |c: f32| {
            let q = (c * CCM_ONE).round().clamp(CCM_MIN as f32, CCM_MAX as f32);
            (q as i32 as u32) & CC_COEFF_MASK
        };
        let offset = |o: f32| {
            let q = o.round().clamp(CT_OFFSET_MIN as f32, CT_OFFSET_MAX as f32);
            (q as i32 as u32) & CT_OFFSET_MASK
        };
        for (i, row) in m.iter().enumerate() {
            for (j, &c) in row.iter().enumerate() {
                self.put(r::ISP_CC_COEFF_0 + 4 * (3 * i + j) as u32, coeff(c));
            }
        }
        self.put(r::ISP_CT_OFFSET_R, offset(offsets[0]));
        self.put(r::ISP_CT_OFFSET_G, offset(offsets[1]));
        self.put(r::ISP_CT_OFFSET_B, offset(offsets[2]));
        self
    }

    /// Lens shading gains over a `width` x `height` raw frame, from tables of `grid.0` x
    /// `grid.1` cells (sampled at the cell centres, row by row) for red, green and blue:
    /// resampled bilinearly to the vertices of 8 x 8 sectors. The sector sizes cover the frame
    /// in pixels, split as evenly as they go; the gradients are `32768 / size`. A table of the
    /// wrong size changes nothing.
    pub fn lens_shading(
        mut self,
        width: u32,
        height: u32,
        grid: (usize, usize),
        [red, green, blue]: [&[f64]; 3],
    ) -> Self {
        let (gw, gh) = grid;
        if gw == 0 || gh == 0 || [red, green, blue].iter().any(|t| t.len() != gw * gh) {
            return self;
        }
        let tables = [
            (r::LSC_R_TABLE_ADDR, r::LSC_R_TABLE_DATA, red),
            (r::LSC_GR_TABLE_ADDR, r::LSC_GR_TABLE_DATA, green),
            (r::LSC_B_TABLE_ADDR, r::LSC_B_TABLE_DATA, blue),
            (r::LSC_GB_TABLE_ADDR, r::LSC_GB_TABLE_DATA, green),
        ];
        for (addr, data, src) in tables {
            let samples = lsc_samples(src, grid);
            self.put(addr, 0);
            for k in 0..LSC_ROWS * LSC_WORDS_PER_ROW {
                let (row, pair) = (k / LSC_WORDS_PER_ROW, k % LSC_WORDS_PER_ROW);
                let s0 = u32::from(samples[row][2 * pair]);
                let s1 = u32::from(samples[row][2 * pair + 1]);
                let word = (s0 & r::LSC_SAMPLE_MASK)
                    | ((s1 & r::LSC_SAMPLE_MASK) << r::LSC_SAMPLE_1_SHIFT);
                self.put(data, word);
            }
        }
        let (xs, xg) = sectors(width);
        let (ys, yg) = sectors(height);
        for (base, values) in [
            (r::LSC_XSIZE_0, &xs),
            (r::LSC_YSIZE_0, &ys),
            (r::LSC_XGRAD_0, &xg),
            (r::LSC_YGRAD_0, &yg),
        ] {
            for (n, pair) in values.chunks(2).enumerate() {
                let lo = u32::from(pair[0]);
                let hi = u32::from(pair[1]);
                self.put(base + 4 * n as u32, lo | (hi << r::LSC_PAIR_SHIFT));
            }
        }
        self.put(r::LSC_TABLE_SEL, 0);
        self.put(r::LSC_CTRL, r::LSC_EN);
        self
    }

    /// The output gamma: `curve` from 0..=1 to 0..=1, sampled at the 17 points of the equal
    /// segments (`x = i / 16`) and quantised to 10 bits. Each step is kept inside the
    /// register's signed 10-bit delta. Turns the gamma-out on in `isp_ctrl` at [`Params::apply`].
    pub fn gamma(mut self, curve: impl Fn(f32) -> f32) -> Self {
        self.put(r::ISP_GAMMA_OUT_MODE, r::GAMMA_OUT_EQU_SEGM);
        let mut prev: i32 = 0;
        for i in 0..GAMMA_POINTS {
            let x = i as f32 / (GAMMA_POINTS - 1) as f32;
            let want = (curve(x).clamp(0.0, 1.0) * GAMMA_OUT_MAX + 0.5) as i32;
            let y = if i == 0 {
                want.clamp(0, GAMMA_OUT_MAX as i32)
            } else {
                (prev + (want - prev).clamp(GAMMA_DELTA_MIN, GAMMA_DELTA_MAX))
                    .clamp(0, GAMMA_OUT_MAX as i32)
            };
            self.put(r::ISP_GAMMA_OUT_Y_0 + 4 * i as u32, y as u32);
            prev = y;
        }
        self.ctrl |= r::CTRL_GAMMA_OUT_ENABLE;
        self
    }

    /// Writes every register of this frame's settings to `regs`, then sets the control bits
    /// (read-modify-write of `isp_ctrl`) and strobes the vsync shadow update
    /// ([`r::CTRL_GEN_CFG_UPD`]).
    pub fn apply<R: RegisterAccess>(&self, regs: &mut R) -> Result<(), R::Error> {
        for w in &self.writes {
            regs.write(w.addr, w.value)?;
        }
        let ctrl = regs.read(r::ISP_CTRL)? | self.ctrl | r::CTRL_GEN_CFG_UPD;
        regs.write(r::ISP_CTRL, ctrl)
    }
}

/// The lens shading samples for `S` sectors per axis: the table's gains at the sector
/// vertices, bilinearly from `src` (`grid.0` x `grid.1` cells, the cell centres at
/// `(k + 0.5)` of the frame), in [`LSC_ONE`] units clamped to the 12-bit field. Rows and
/// columns beyond the vertices stay at unity.
fn lsc_samples(src: &[f64], grid: (usize, usize)) -> [[u16; LSC_COLS]; LSC_ROWS] {
    let (w, h) = grid;
    let mut out = [[LSC_ONE as u16; LSC_COLS]; LSC_ROWS];
    let at = |len: usize, i: usize| -> (usize, usize, f64) {
        let p = -0.5 + i as f64 * len as f64 / LSC_SECTORS as f64;
        let lo = p.floor();
        let f = p - lo;
        let lo_i = lo.max(0.0) as usize;
        (
            lo_i.min(len - 1),
            (lo_i + 1).min(len - 1),
            if p < 0.0 { 0.0 } else { f },
        )
    };
    for (j, row) in out.iter_mut().enumerate().take(LSC_SECTORS + 1) {
        let (y0, y1, fy) = at(h, j);
        for (i, cell) in row.iter_mut().enumerate().take(LSC_SECTORS + 1) {
            let (x0, x1, fx) = at(w, i);
            let v = |x: usize, y: usize| src[y * w + x];
            let top = v(x0, y0) * (1.0 - fx) + v(x1, y0) * fx;
            let bottom = v(x0, y1) * (1.0 - fx) + v(x1, y1) * fx;
            let gain = top * (1.0 - fy) + bottom * fy;
            *cell = (gain * LSC_ONE + 0.5).clamp(0.0, LSC_MAX) as u16;
        }
    }
    out
}

/// The eight sector sizes of a `len`-pixel axis, and their gradients (`32768 / size`, clamped
/// to the 12-bit field). The sizes sum to `len`.
fn sectors(len: u32) -> ([u16; LSC_SECTORS], [u16; LSC_SECTORS]) {
    let n = LSC_SECTORS as u32;
    let sizes: [u16; LSC_SECTORS] =
        core::array::from_fn(|i| ((len * (i as u32 + 1)) / n - (len * i as u32) / n) as u16);
    let grads = sizes.map(|s| {
        let s = u32::from(s.max(1));
        ((32768 + s / 2) / s).min(r::LSC_GRAD_MASK) as u16
    });
    (sizes, grads)
}
