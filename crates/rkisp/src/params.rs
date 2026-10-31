//! A parameter buffer for one frame ([`Params`]), for ISP21 (RK3566 / RK3568) or ISP30
//! (RK3588 / RK3588S).

use alloc::boxed::Box;

// Float methods for builds without std (with std the inherent ones are used).
#[allow(unused_imports)]
use styx_math::Float as _;

use crate::uapi::{isp3x, isp21, module};

/// The ISP generation a parameter buffer or a statistics buffer is for. The two share the
/// modules and most of the layouts; the lens shading and the output gamma differ (see
/// [`ISP21_LSC_SECTORS`] and [`ISP21_GAMMA_SAMPLES`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    /// ISP21: RK3566 / RK3568 (`rk-isp21-config.h`).
    Isp21,
    /// ISP30: RK3588 / RK3588S (`rk-isp3-config.h`).
    Isp30,
}

/// White balance gain 1.0 (the `awb_gain_cfg` fields are unsigned with 8 fractional bits, as
/// Rockchip's 3A writes them; the struct is shared by both generations).
pub const GAIN_ONE: f32 = 256.0;
/// The smallest gain written, in the same units: the driver refuses a parameter buffer with a
/// zero gain (`isp_awbgain_config`, "awb gain is zero!") and keeps the previous gains.
const GAIN_MIN: u16 = 1;
/// Colour matrix coefficient 1.0 (signed, 7 fractional bits, as Rockchip's 3A writes them).
pub const CCM_ONE: f32 = 128.0;
/// Full scale of the output gamma curve's samples (12 bits).
pub const GAMMA_OUT_MAX: f32 = 4095.0;
/// Lens shading gain 1.0 (10 fractional bits in a 12-bit sample, as Rockchip's 3A writes
/// them).
pub const LSC_ONE: f64 = 1024.0;

/// Sectors per axis of the ISP21 lens shading: its size and gradient tables have 8 entries
/// (`isp2x_lsc_cfg`, `ISP2X_LSC_SIZE_TBL_SIZE`) and its driver has no 16x16 flag
/// (`isp_lsc_config` in `isp_params_v2x.c`). Read as 8x8 sectors, 9x9 vertices.
pub const ISP21_LSC_SECTORS: usize = 8;
/// Sectors per axis of the ISP30 lens shading, 16x16 (`sector_16x16` set, the 16-entry
/// tables of `isp3x_lsc_cfg`).
pub const ISP30_LSC_SECTORS: usize = 16;
/// Row stride of the lens shading data tables: the driver writes 17 x 17 entries in both
/// generations (`CIF_ISP_LSC_SECTORS_MAX` in `isp_params_v2x.c`); ISP21's 9x9 vertices are
/// its top-left corner. Which 9 of the 17 entries the ISP21 hardware reads is not stated by
/// the driver: to be confirmed on a board.
const LSC_STRIDE: usize = 17;
/// Entries in each lens shading data table (`ISP2X_LSC_DATA_TBL_SIZE`).
const LSC_DATA: usize = 290;

/// Output gamma samples of ISP21 (`ISP2X_GAMMA_OUT_MAX_SAMPLES`); ISP30 has 49.
pub const ISP21_GAMMA_SAMPLES: usize = 45;

/// The big raw AE block's window grid: 15x15 zones (`wnd_num` 2).
const AE_GRID_15X15: u8 = 2;
/// The big raw histogram's mode for luma from the weighted channels.
const HIST_MODE_Y: u8 = 5;
/// The luma weights of the histogram (Rec.601 in 1/256: 77, 150, 29).
const HIST_RGB_WEIGHTS: (u8, u8, u8) = (77, 150, 29);

/// The configuration of either generation.
enum Config {
    Isp21(Box<isp21::isp21_isp_params_cfg>),
    Isp30(Box<isp3x::isp3x_isp_params_cfg>),
}

/// Runs `$body` on the configuration, bound to `$c`, of whichever generation it is: fields
/// the generations share are written once; where they differ, `$body` matches on the
/// generation itself.
macro_rules! on_cfg {
    ($this:ident, |$c:ident| $body:block) => {
        match &mut $this.cfg {
            Config::Isp21($c) => $body,
            Config::Isp30($c) => $body,
        }
    };
}

/// A parameter buffer for one frame, all modules off until set. [`Params::as_bytes`] is what
/// the driver's parameter node takes.
pub struct Params {
    cfg: Config,
}

impl Params {
    /// Every module off and unchanged, for `generation`.
    pub fn new(generation: Generation) -> Self {
        let cfg = match generation {
            Generation::Isp21 => Config::Isp21(bytemuck::zeroed_box()),
            Generation::Isp30 => Config::Isp30(bytemuck::zeroed_box()),
        };
        Self { cfg }
    }

    /// The generation these parameters are for.
    pub fn generation(&self) -> Generation {
        match self.cfg {
            Config::Isp21(_) => Generation::Isp21,
            Config::Isp30(_) => Generation::Isp30,
        }
    }

    /// The frame these parameters are for (the driver applies them to it).
    pub fn frame(mut self, id: u32) -> Self {
        on_cfg!(self, |c| {
            c.frame_id = id;
        });
        self
    }

    /// Turns `bits` on with their configuration updated.
    fn enable(&mut self, bits: u64) {
        on_cfg!(self, |c| {
            let (ens, en_update, cfg_update) =
                (c.module_ens, c.module_en_update, c.module_cfg_update);
            c.module_ens = ens | bits;
            c.module_en_update = en_update | bits;
            c.module_cfg_update = cfg_update | bits;
        });
    }

    /// The statistics Styx's 3A reads over a `width` x `height` raw frame: the big raw AE
    /// block's 15x15 zones of channel means (colour zones) and the big raw histogram of luma.
    pub fn measure(mut self, width: u16, height: u16) -> Self {
        on_cfg!(self, |c| {
            let ae = &mut c.meas.rawae3;
            ae.rawae_sel = 0;
            ae.wnd_num = AE_GRID_15X15;
            (ae.win.h_offs, ae.win.v_offs, ae.win.h_size, ae.win.v_size) = (0, 0, width, height);
            let hist = &mut c.meas.rawhist1;
            hist.wnd_num = AE_GRID_15X15;
            hist.data_sel = 0;
            hist.mode = HIST_MODE_Y;
            hist.stepsize = 0;
            (hist.rcc, hist.gcc, hist.bcc) = HIST_RGB_WEIGHTS;
            (
                hist.win.h_offs,
                hist.win.v_offs,
                hist.win.h_size,
                hist.win.v_size,
            ) = (0, 0, width, height);
            hist.weight = [1; 225];
        });
        self.enable(module::RAWAE3 | module::RAWHIST1);
        self
    }

    /// Black level to subtract, per channel (R, Gr, Gb, B) in 12-bit sensor units.
    pub fn black_level(mut self, levels: [u16; 4]) -> Self {
        let v = |l: u16| l.min(4095) as i16;
        on_cfg!(self, |c| {
            let bls = &mut c.others.bls_cfg;
            bls.enable_auto = 0;
            bls.fixed_val.r = v(levels[0]);
            bls.fixed_val.gr = v(levels[1]);
            bls.fixed_val.gb = v(levels[2]);
            bls.fixed_val.b = v(levels[3]);
        });
        self.enable(module::BLS);
        self
    }

    /// White balance gains (red, green, blue; 1.0 is none), up to 255.99.
    pub fn white_balance(mut self, gains: [f32; 3]) -> Self {
        let q = |g: f32| ((g.clamp(0.0, 65535.0 / GAIN_ONE) * GAIN_ONE + 0.5) as u16).max(GAIN_MIN);
        let (r, g, b) = (q(gains[0]), q(gains[1]), q(gains[2]));
        on_cfg!(self, |c| {
            let w = &mut c.others.awb_gain_cfg;
            (w.gain0_red, w.gain0_green_r, w.gain0_green_b, w.gain0_blue) = (r, g, g, b);
            (w.gain1_red, w.gain1_green_r, w.gain1_green_b, w.gain1_blue) = (r, g, g, b);
            (w.gain2_red, w.gain2_green_r, w.gain2_green_b, w.gain2_blue) = (r, g, g, b);
        });
        self.enable(module::AWB_GAIN);
        self
    }

    /// The colour matrix (rows: output R, G, B) and its offsets (12-bit units).
    pub fn colour_matrix(mut self, m: [[f32; 3]; 3], offsets: [f32; 3]) -> Self {
        let q = |c: f32| (c * CCM_ONE).round().clamp(-32768.0, 32767.0) as i16;
        let o = |c: f32| c.round().clamp(-4095.0, 4095.0) as i16;
        on_cfg!(self, |cfg| {
            let c = &mut cfg.others.ccm_cfg;
            (c.coeff0_r, c.coeff1_r, c.coeff2_r, c.offset_r) =
                (q(m[0][0]), q(m[0][1]), q(m[0][2]), o(offsets[0]));
            (c.coeff0_g, c.coeff1_g, c.coeff2_g, c.offset_g) =
                (q(m[1][0]), q(m[1][1]), q(m[1][2]), o(offsets[1]));
            (c.coeff0_b, c.coeff1_b, c.coeff2_b, c.offset_b) =
                (q(m[2][0]), q(m[2][1]), q(m[2][2]), o(offsets[2]));
        });
        self.enable(module::CCM);
        self
    }

    /// Lens shading gains over a `width` x `height` raw frame, from tables of `grid.0` x
    /// `grid.1` cells (sampled at the cell centres, row by row) for red, green and blue:
    /// resampled bilinearly to the vertices of the generation's sectors (ISP21: 8x8, ISP30:
    /// 16x16).
    pub fn lens_shading(
        mut self,
        width: u16,
        height: u16,
        grid: (usize, usize),
        [r, g, b]: [&[f64]; 3],
    ) -> Self {
        let (gw, gh) = grid;
        if gw == 0 || gh == 0 || [r, g, b].iter().any(|t| t.len() != gw * gh) {
            return self;
        }
        match &mut self.cfg {
            Config::Isp21(c) => {
                let s = shading::<ISP21_LSC_SECTORS>(width, height, grid, [r, g, b]);
                let l = &mut c.others.lsc_cfg;
                (l.x_size_tbl, l.y_size_tbl) = (s.x_size, s.y_size);
                (l.x_grad_tbl, l.y_grad_tbl) = (s.x_grad, s.y_grad);
                (l.r_data_tbl, l.gr_data_tbl, l.gb_data_tbl, l.b_data_tbl) =
                    (s.data[0], s.data[1], s.data[1], s.data[2]);
            }
            Config::Isp30(c) => {
                let s = shading::<ISP30_LSC_SECTORS>(width, height, grid, [r, g, b]);
                let l = &mut c.others.lsc_cfg;
                l.sector_16x16 = 1;
                (l.x_size_tbl, l.y_size_tbl) = (s.x_size, s.y_size);
                (l.x_grad_tbl, l.y_grad_tbl) = (s.x_grad, s.y_grad);
                (l.r_data_tbl, l.gr_data_tbl, l.gb_data_tbl, l.b_data_tbl) =
                    (s.data[0], s.data[1], s.data[1], s.data[2]);
            }
        }
        self.enable(module::LSC);
        self
    }

    /// The output gamma: `curve` from 0..=1 to 0..=1, sampled at the equally spaced points of
    /// the 12-bit input (`equ_segm`: 49 points on ISP30, [`ISP21_GAMMA_SAMPLES`] on ISP21).
    pub fn gamma(mut self, curve: impl Fn(f32) -> f32) -> Self {
        on_cfg!(self, |c| {
            let g = &mut c.others.gammaout_cfg;
            g.equ_segm = 1;
            let mut y = g.gamma_y;
            gamma_samples(&mut y, &curve);
            g.gamma_y = y;
        });
        self.enable(module::GOC);
        self
    }

    /// The buffer for the driver's parameter node.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.cfg {
            Config::Isp21(c) => bytemuck::bytes_of(&**c),
            Config::Isp30(c) => bytemuck::bytes_of(&**c),
        }
    }

    /// The ISP21 configuration as built; `None` for an ISP30 buffer.
    pub fn isp21(&self) -> Option<&isp21::isp21_isp_params_cfg> {
        match &self.cfg {
            Config::Isp21(c) => Some(c),
            Config::Isp30(_) => None,
        }
    }

    /// The ISP30 configuration as built; `None` for an ISP21 buffer.
    pub fn isp30(&self) -> Option<&isp3x::isp3x_isp_params_cfg> {
        match &self.cfg {
            Config::Isp30(c) => Some(c),
            Config::Isp21(_) => None,
        }
    }
}

/// A lens shading table over `S` sectors per axis: the sector sizes and gradients (`S`
/// entries each) and the R, Gr (= Gb) and B data tables.
struct Shading<const S: usize> {
    x_size: [u16; S],
    y_size: [u16; S],
    x_grad: [u16; S],
    y_grad: [u16; S],
    data: [[u16; LSC_DATA]; 3],
}

/// The lens shading tables of a `width` x `height` raw frame for `S` x `S` sectors. The
/// sectors cover the frame's Bayer quads: half its size, split as evenly as it goes.
fn shading<const S: usize>(
    width: u16,
    height: u16,
    grid: (usize, usize),
    [r, g, b]: [&[f64]; 3],
) -> Shading<S> {
    let sizes = |len: u16| -> [u16; S] {
        let half = usize::from(len / 2);
        core::array::from_fn(|i| ((half * (i + 1)) / S - (half * i) / S) as u16)
    };
    let grad = |s: [u16; S]| s.map(|v| ((32768 + u32::from(v) / 2) / u32::from(v.max(1))) as u16);
    let (x_size, y_size) = (sizes(width), sizes(height));
    Shading {
        x_size,
        y_size,
        x_grad: grad(x_size),
        y_grad: grad(y_size),
        data: [
            vertices::<S>(r, grid),
            vertices::<S>(g, grid),
            vertices::<S>(b, grid),
        ],
    }
}

/// A table of `grid` cells (gains at the cell centres) at the `S` + 1 vertices per axis (the
/// frame's corners and sector edges), bilinearly, clamped at the edges; as samples of
/// [`LSC_ONE`] in the table's [`LSC_STRIDE`]-wide layout.
fn vertices<const S: usize>(src: &[f64], grid: (usize, usize)) -> [u16; LSC_DATA] {
    let (w, h) = grid;
    let at = |len: usize, i: usize| -> (usize, usize, f64) {
        let p = -0.5 + i as f64 * len as f64 / S as f64;
        let lo = p.floor();
        let f = p - lo;
        let lo = lo.max(0.0) as usize;
        (
            lo.min(len - 1),
            (lo + 1).min(len - 1),
            if p < 0.0 { 0.0 } else { f },
        )
    };
    let mut out = [LSC_ONE as u16; LSC_DATA];
    for j in 0..=S {
        let (y0, y1, fy) = at(h, j);
        for i in 0..=S {
            let (x0, x1, fx) = at(w, i);
            let v = |x: usize, y: usize| src[y * w + x];
            let top = v(x0, y0) * (1.0 - fx) + v(x1, y0) * fx;
            let bottom = v(x0, y1) * (1.0 - fx) + v(x1, y1) * fx;
            let gain = top * (1.0 - fy) + bottom * fy;
            out[j * LSC_STRIDE + i] = (gain * LSC_ONE).round().clamp(0.0, 4095.0) as u16;
        }
    }
    out
}

/// `out` filled with `curve` sampled at its equally spaced inputs from 0 to 1.
fn gamma_samples(out: &mut [u16], curve: &impl Fn(f32) -> f32) {
    let n = out.len();
    for (i, s) in out.iter_mut().enumerate() {
        let x = i as f32 / (n - 1) as f32;
        *s = (curve(x).clamp(0.0, 1.0) * GAMMA_OUT_MAX + 0.5) as u16;
    }
}
