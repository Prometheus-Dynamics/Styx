//! One frame's 3A settings as the register writes Lemnos's `k230isp` library takes: [`Params`].
//!
//! The register addresses, field masks and the bits of each register are Lemnos's
//! ([`regs`](crate::regs)); what this module decides is the values, which are Styx's: the neutral
//! settings' full scale of 1.0 mapped to the chip's units (`100h` is a gain of 1.0 for the AWB
//! gains, `80h` a colour coefficient of 1.0, `400h` a fixed black level of 1.0 in 12-bit units).
//!
//! The builder does not read the ISP. It produces an ordered list of [`RegWrite`]s, each value
//! clamped to its field, and [`Params::encode`] lays the list out as a configuration of Lemnos's
//! `camera.isp/1` layout ([`config`](crate::config)). Lemnos checks the whole list before any
//! write reaches the block, and the driver strobes the shadow update after it.
//!
//! Not written here: lens shading. Lemnos's library has the lens shading registers' addresses and
//! field masks, but no layout for the sample table or the sector sizes and gradients, so Styx
//! does not pack them (see the crate's docs for the gap).

use styx_lemnos::k230isp::config;
use styx_lemnos::k230isp::regs::{self as r, CC_COEFF_MASK, CT_OFFSET_MASK};

/// Why a configuration cannot be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// More writes than Lemnos's layout takes in one configuration.
    TooManyWrites(usize),
    /// The buffer given is shorter than the configuration (its length in bytes).
    TooSmall(usize),
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyWrites(n) => write!(f, "{n} register writes, past the layout's limit"),
            Self::TooSmall(n) => write!(f, "a configuration of {n} bytes does not fit"),
        }
    }
}

impl std::error::Error for ConfigError {}

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

/// One register write: an offset in the register window and the value written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegWrite {
    /// Register offset within the window.
    pub addr: u32,
    /// Value written.
    pub value: u32,
}

/// One frame's register writes.
///
/// Each builder method appends its writes; a later write to the same register overrides the
/// earlier one in the device, as Lemnos applies them in order. The gamma enable is kept apart
/// as `isp_ctrl` bits and written last by [`Params::encode`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    writes: Vec<RegWrite>,
    ctrl: u32,
}

impl Params {
    /// No writes yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same value with no writes, its list's capacity kept: one value reused frame to frame
    /// builds each frame's writes without allocating once the list has grown.
    #[must_use]
    pub fn reset(mut self) -> Self {
        self.writes.clear();
        self.ctrl = 0;
        self
    }

    /// The writes, in the order they are applied (before the `isp_ctrl` bits).
    #[must_use]
    pub fn writes(&self) -> &[RegWrite] {
        &self.writes
    }

    /// The `isp_ctrl` bits [`Params::encode`] writes last; zero when none are needed.
    #[must_use]
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
    #[must_use]
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

    /// Black levels to subtract, per Bayer position (top-left, top-right, bottom-left,
    /// bottom-right), in 12-bit sensor units. Clamped to the 13-bit signed field.
    #[must_use]
    pub fn black_level(mut self, levels: [u16; 4]) -> Self {
        let field = |l: u16| (i32::from(l).clamp(BLS_MIN, BLS_MAX) as u32) & r::BLS_FIXED_MASK;
        self.put(r::BLS_CTRL, r::BLS_ENABLE);
        self.put(r::BLS_A_FIXED, field(levels[0]));
        self.put(r::BLS_B_FIXED, field(levels[1]));
        self.put(r::BLS_C_FIXED, field(levels[2]));
        self.put(r::BLS_D_FIXED, field(levels[3]));
        self
    }

    /// White balance gains (red, green, blue; 1.0 is none), clamped to 1/256..=[`GAIN_MAX`]. A
    /// zero gain would blank the channel, so it is written as the smallest step. The green gain
    /// goes to both green positions.
    #[must_use]
    pub fn white_balance(mut self, gains: [f32; 3]) -> Self {
        let q =
            |g: f32| ((g.clamp(0.0, GAIN_MAX) * GAIN_ONE + 0.5) as u32).clamp(1, r::AWB_GAIN_MASK);
        let (red, green, blue) = (q(gains[0]), q(gains[1]), q(gains[2]));
        self.put(r::ISP_AWB_GAIN_G, (green << r::AWB_GAIN_HI_SHIFT) | green);
        self.put(r::ISP_AWB_GAIN_RB, (red << r::AWB_GAIN_HI_SHIFT) | blue);
        self
    }

    /// The colour matrix (rows: output R, G, B; columns: input R, G, B) and its offsets, in the
    /// registers' units. Coefficients are clamped to the 9-bit field (-2.0 to +1.99), the offsets
    /// to the 12-bit field.
    #[must_use]
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

    /// The output gamma: `curve` from 0..=1 to 0..=1, sampled at the 17 points of the equal
    /// segments (`x = i / 16`) and quantised to 10 bits. Each step is kept inside the register's
    /// signed 10-bit delta. Turns the gamma-out on when the list is encoded.
    #[must_use]
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

    /// Lays the writes out as a configuration of Lemnos's `camera.isp/1` layout into `out`, and
    /// returns its length. The `isp_ctrl` bits, when there are any, are the last write: its mask
    /// keeps the ISP's other bits as they are. Lemnos's [`config::check`] accepts the result.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooManyWrites`] past Lemnos's limit, [`ConfigError::TooSmall`] when `out`
    /// cannot hold the configuration.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, ConfigError> {
        let count = self.count()?;
        let len = 4 + 8 * count;
        let out = out.get_mut(..len).ok_or(ConfigError::TooSmall(len))?;
        out[..4].copy_from_slice(&(count as u32).to_le_bytes());
        let ctrl = self.ctrl_write();
        let all = self.writes.iter().chain(ctrl.iter());
        for (chunk, w) in out[4..].as_chunks_mut::<8>().0.iter_mut().zip(all) {
            chunk[..4].copy_from_slice(&w.addr.to_le_bytes());
            chunk[4..].copy_from_slice(&w.value.to_le_bytes());
        }
        Ok(len)
    }

    /// The length in bytes [`Params::encode`] gives the configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooManyWrites`] past Lemnos's limit.
    pub fn encoded_len(&self) -> Result<usize, ConfigError> {
        Ok(4 + 8 * self.count()?)
    }

    fn ctrl_write(&self) -> Option<RegWrite> {
        (self.ctrl != 0).then_some(RegWrite {
            addr: r::ISP_CTRL,
            value: self.ctrl,
        })
    }

    fn count(&self) -> Result<usize, ConfigError> {
        let count = self.writes.len() + usize::from(self.ctrl != 0);
        if count > config::MAX_WRITES {
            return Err(ConfigError::TooManyWrites(count));
        }
        Ok(count)
    }
}
