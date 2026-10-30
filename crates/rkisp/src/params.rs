//! A parameter buffer for one frame ([`Params`]).

use alloc::boxed::Box;

// Float methods for builds without std (with std the inherent ones are used).
#[allow(unused_imports)]
use styx_math::Float as _;

use crate::uapi::{isp2x_window, isp3x_isp_params_cfg, module};

/// White balance gain 1.0 (the `awb_gain_cfg` fields are unsigned with 8 fractional bits, as
/// Rockchip's 3A writes them).
pub const GAIN_ONE: f32 = 256.0;
/// Colour matrix coefficient 1.0 (signed, 7 fractional bits, as Rockchip's 3A writes them).
pub const CCM_ONE: f32 = 128.0;
/// Full scale of the output gamma curve's samples (12 bits).
pub const GAMMA_OUT_MAX: f32 = 4095.0;
/// Lens shading gain 1.0 (10 fractional bits in a 12-bit sample, as Rockchip's 3A writes
/// them).
pub const LSC_ONE: f64 = 1024.0;

/// The lens shading grid: 16x16 sectors, 17x17 vertices (`sector_16x16`).
const LSC_SECTORS: usize = 16;
const LSC_VERTICES: usize = LSC_SECTORS + 1;

/// The big raw AE block's window grid: 15x15 zones (`wnd_num` 2).
const AE_GRID_15X15: u8 = 2;
/// The big raw histogram's mode for luma from the weighted channels.
const HIST_MODE_Y: u8 = 5;
/// The luma weights of the histogram (Rec.601 in 1/256: 77, 150, 29).
const HIST_RGB_WEIGHTS: (u8, u8, u8) = (77, 150, 29);

/// A parameter buffer for one frame, all modules off until set. [`Params::as_bytes`] is what
/// the driver's parameter node takes.
pub struct Params {
    cfg: Box<isp3x_isp_params_cfg>,
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

impl Params {
    /// Every module off and unchanged.
    pub fn new() -> Self {
        Self {
            cfg: bytemuck::zeroed_box(),
        }
    }

    /// The frame these parameters are for (the driver applies them to it).
    pub fn frame(mut self, id: u32) -> Self {
        self.cfg.frame_id = id;
        self
    }

    /// Turns `bits` on with their configuration updated.
    fn enable(&mut self, bits: u64) {
        let (ens, en_update, cfg_update) = (
            self.cfg.module_ens,
            self.cfg.module_en_update,
            self.cfg.module_cfg_update,
        );
        self.cfg.module_ens = ens | bits;
        self.cfg.module_en_update = en_update | bits;
        self.cfg.module_cfg_update = cfg_update | bits;
    }

    /// The statistics Styx's 3A reads over a `width` x `height` raw frame: the big raw AE
    /// block's 15x15 zones of channel means (colour zones) and the big raw histogram of luma.
    pub fn measure(mut self, width: u16, height: u16) -> Self {
        let win = isp2x_window {
            h_offs: 0,
            v_offs: 0,
            h_size: width,
            v_size: height,
        };
        let ae = &mut self.cfg.meas.rawae3;
        ae.rawae_sel = 0;
        ae.wnd_num = AE_GRID_15X15;
        ae.win = win;
        let hist = &mut self.cfg.meas.rawhist1;
        hist.wnd_num = AE_GRID_15X15;
        hist.data_sel = 0;
        hist.mode = HIST_MODE_Y;
        hist.stepsize = 0;
        (hist.rcc, hist.gcc, hist.bcc) = HIST_RGB_WEIGHTS;
        hist.win = win;
        hist.weight = [1; 225];
        self.enable(module::RAWAE3 | module::RAWHIST1);
        self
    }

    /// Black level to subtract, per channel (R, Gr, Gb, B) in 12-bit sensor units.
    pub fn black_level(mut self, levels: [u16; 4]) -> Self {
        let v = |l: u16| l.min(4095) as i16;
        let bls = &mut self.cfg.others.bls_cfg;
        bls.enable_auto = 0;
        bls.fixed_val.r = v(levels[0]);
        bls.fixed_val.gr = v(levels[1]);
        bls.fixed_val.gb = v(levels[2]);
        bls.fixed_val.b = v(levels[3]);
        self.enable(module::BLS);
        self
    }

    /// White balance gains (red, green, blue; 1.0 is none), up to 255.99.
    pub fn white_balance(mut self, gains: [f32; 3]) -> Self {
        let q = |g: f32| (g.clamp(0.0, 65535.0 / GAIN_ONE) * GAIN_ONE + 0.5) as u16;
        let (r, g, b) = (q(gains[0]), q(gains[1]), q(gains[2]));
        let w = &mut self.cfg.others.awb_gain_cfg;
        (w.gain0_red, w.gain0_green_r, w.gain0_green_b, w.gain0_blue) = (r, g, g, b);
        (w.gain1_red, w.gain1_green_r, w.gain1_green_b, w.gain1_blue) = (r, g, g, b);
        (w.gain2_red, w.gain2_green_r, w.gain2_green_b, w.gain2_blue) = (r, g, g, b);
        self.enable(module::AWB_GAIN);
        self
    }

    /// The colour matrix (rows: output R, G, B) and its offsets (12-bit units).
    pub fn colour_matrix(mut self, m: [[f32; 3]; 3], offsets: [f32; 3]) -> Self {
        let q = |c: f32| (c * CCM_ONE).round().clamp(-32768.0, 32767.0) as i16;
        let o = |c: f32| c.round().clamp(-4095.0, 4095.0) as i16;
        let c = &mut self.cfg.others.ccm_cfg;
        (c.coeff0_r, c.coeff1_r, c.coeff2_r, c.offset_r) =
            (q(m[0][0]), q(m[0][1]), q(m[0][2]), o(offsets[0]));
        (c.coeff0_g, c.coeff1_g, c.coeff2_g, c.offset_g) =
            (q(m[1][0]), q(m[1][1]), q(m[1][2]), o(offsets[1]));
        (c.coeff0_b, c.coeff1_b, c.coeff2_b, c.offset_b) =
            (q(m[2][0]), q(m[2][1]), q(m[2][2]), o(offsets[2]));
        self.enable(module::CCM);
        self
    }

    /// Lens shading gains over a `width` x `height` raw frame, from tables of `grid.0` x
    /// `grid.1` cells (sampled at the cell centres, row by row) for red, green and blue:
    /// resampled bilinearly to the 17x17 vertices of 16x16 equal sectors.
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
        let lsc = &mut self.cfg.others.lsc_cfg;
        lsc.sector_16x16 = 1;
        // The sectors cover the frame's Bayer quads: half its size, split as evenly as it goes.
        let sizes = |len: u16| -> [u16; LSC_SECTORS] {
            let half = usize::from(len / 2);
            core::array::from_fn(|i| {
                ((half * (i + 1)) / LSC_SECTORS - (half * i) / LSC_SECTORS) as u16
            })
        };
        let grad = |s: [u16; LSC_SECTORS]| {
            s.map(|v| ((32768 + u32::from(v) / 2) / u32::from(v.max(1))) as u16)
        };
        let (xs, ys) = (sizes(width), sizes(height));
        lsc.x_size_tbl = xs;
        lsc.y_size_tbl = ys;
        lsc.x_grad_tbl = grad(xs);
        lsc.y_grad_tbl = grad(ys);
        let (rt, gt, bt) = (
            vertices(r, gw, gh),
            vertices(g, gw, gh),
            vertices(b, gw, gh),
        );
        lsc.r_data_tbl = rt;
        lsc.gr_data_tbl = gt;
        lsc.gb_data_tbl = gt;
        lsc.b_data_tbl = bt;
        self.enable(module::LSC);
        self
    }

    /// The output gamma: `curve` from 0..=1 to 0..=1, sampled at the 49 equally spaced points
    /// of the 12-bit input (`equ_segm`).
    pub fn gamma(mut self, curve: impl Fn(f32) -> f32) -> Self {
        let g = &mut self.cfg.others.gammaout_cfg;
        g.equ_segm = 1;
        g.finalx4_dense_en = 0;
        g.offset = 0;
        let mut samples = [0u16; crate::uapi::ISP3X_GAMMA_OUT_MAX_SAMPLES as usize];
        let n = samples.len();
        for (i, s) in samples.iter_mut().enumerate() {
            let x = i as f32 / (n - 1) as f32;
            *s = (curve(x).clamp(0.0, 1.0) * GAMMA_OUT_MAX + 0.5) as u16;
        }
        g.gamma_y = samples;
        self.enable(module::GOC);
        self
    }

    /// The buffer for the driver's parameter node.
    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::bytes_of(&*self.cfg)
    }

    /// The configuration as built.
    pub fn config(&self) -> &isp3x_isp_params_cfg {
        &self.cfg
    }
}

/// A table of `w` x `h` cells (gains at the cell centres) at the 17x17 grid vertices (the
/// frame's corners and sector edges), bilinearly, clamped at the edges; as samples of
/// [`LSC_ONE`], padded to the uAPI's 290.
fn vertices(src: &[f64], w: usize, h: usize) -> [u16; 290] {
    let at = |len: usize, i: usize| -> (usize, usize, f64) {
        let p = -0.5 + i as f64 * len as f64 / LSC_SECTORS as f64;
        let lo = p.floor();
        let f = p - lo;
        let lo = lo.max(0.0) as usize;
        (
            lo.min(len - 1),
            (lo + 1).min(len - 1),
            if p < 0.0 { 0.0 } else { f },
        )
    };
    let mut out = [LSC_ONE as u16; 290];
    for j in 0..LSC_VERTICES {
        let (y0, y1, fy) = at(h, j);
        for i in 0..LSC_VERTICES {
            let (x0, x1, fx) = at(w, i);
            let v = |x: usize, y: usize| src[y * w + x];
            let top = v(x0, y0) * (1.0 - fx) + v(x1, y0) * fx;
            let bottom = v(x0, y1) * (1.0 - fx) + v(x1, y1) * fx;
            let gain = top * (1.0 - fy) + bottom * fy;
            out[j * LSC_VERTICES + i] = (gain * LSC_ONE).round().clamp(0.0, 4095.0) as u16;
        }
    }
    out
}
