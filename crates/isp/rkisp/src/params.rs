//! A parameter buffer for one frame: [`Params`] holds the bytes the ISP's parameter buffer takes,
//! in the library's layout for its generation, and [`Params::apply`] writes a neutral
//! [`IspSettings`] into it.
//!
//! The mapping is the one dev's Rockchip path made (`crates/rkisp/src/params.rs`, dev branch):
//! the same blocks, the same units, the same white balance (gains times the digital gain) and
//! the same lens shading and gamma tables. The modules it enables are the library's bits.

use core::mem::offset_of;

use styx_aaa::{LensShading, Pwl};
use styx_lemnos::rkisp::uapi::{isp3x, isp21, module};
use styx_settings::IspSettings;

use crate::bytes::{put_i16, put_u8, put_u16, put_u32, put_u64};
use crate::{Error, Generation};

/// White balance gain 1.0 in the ISP's gain fields (8 fractional bits), and the smallest gain
/// written: the driver refuses a zero gain and keeps the previous ones.
const GAIN_ONE: f32 = 256.0;
const GAIN_MIN: u16 = 1;
/// Colour matrix coefficient 1.0 (signed, 7 fractional bits).
const CCM_ONE: f32 = 128.0;
/// Lens shading gain 1.0 (10 fractional bits in a 12-bit sample).
const LSC_ONE: f64 = 1024.0;
/// Full scale of the output gamma samples (12 bits).
const GAMMA_OUT_MAX: f32 = 4095.0;
/// The 12-bit scale the black levels are on (the ISP's own).
const LEVEL12: f64 = 4096.0;
/// Data entries per lens shading table (`ISP2X_LSC_DATA_TBL_SIZE`).
const LSC_DATA: usize = 290;
/// Row stride of the lens shading data tables (17 x 17 entries, `CIF_ISP_LSC_SECTORS_MAX`).
const LSC_STRIDE: usize = 17;
/// Most sectors per axis any generation has (ISP30's 16x16).
const MAX_SECTORS: usize = 16;
/// The big raw AE block's window grid: 15x15 zones (`wnd_num` 2).
const AE_GRID_15X15: u8 = 2;
/// The big raw histogram's mode for luma from the weighted channels.
const HIST_MODE_Y: u8 = 5;
/// The luma weights of the histogram (Rec.601 in 1/256).
const HIST_RGB_WEIGHTS: (u8, u8, u8) = (77, 150, 29);

/// The parameter buffer of one ISP generation, in the library's layout for it. Built once per
/// ISP and reused frame to frame: [`Params::apply`] clears it and writes the next frame's values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    generation: Generation,
    bytes: Vec<u8>,
}

impl Params {
    /// Every module off, for `generation`: the buffer the library's layout names, zeroed.
    #[must_use]
    pub fn new(generation: Generation) -> Self {
        Self {
            generation,
            bytes: vec![0; generation.config_layout().bytes as usize],
        }
    }

    /// The generation these parameters are for.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The bytes the ISP's parameter buffer takes (the library's layout size).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Clears the buffer and writes `settings` for frame `frame`, measuring the statistics over
    /// a `width` x `height` raw frame. No allocation: the buffer keeps its size.
    ///
    /// # Errors
    ///
    /// [`Error::Unmapped`] for ISP35 (its module bits are not in the library; see the crate
    /// documentation). The buffer is left cleared in that case.
    pub fn apply(
        &mut self,
        settings: &IspSettings,
        frame: u32,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        self.bytes.fill(0);
        match self.generation {
            Generation::Isp21 => apply::<Isp21>(&mut self.bytes, settings, frame, width, height),
            Generation::Isp30 => apply::<Isp30>(&mut self.bytes, settings, frame, width, height),
            Generation::Isp35
            | Generation::Isp32
            | Generation::Isp32Lite
            | Generation::Isp39 => Err(Error::Unmapped(self.generation)),
        }
    }
}

/// The blocks Styx writes, for one generation's structures.
trait Layout {
    /// Sectors per axis of the lens shading.
    const SECTORS: usize;
    /// Samples of the output gamma table.
    const GAMMA: usize;
    fn frame(b: &mut [u8], id: u32);
    fn measure(b: &mut [u8], width: u16, height: u16);
    fn black_level(b: &mut [u8], levels: [u16; 4]);
    fn white_balance(b: &mut [u8], gains: [f32; 3]);
    fn colour_matrix(b: &mut [u8], m: [[f32; 3]; 3], offsets: [f32; 3]);
    /// Writes the tables and turns the block on; `sh.sectors` is [`Self::SECTORS`].
    fn lens_shading(b: &mut [u8], sh: &Shading);
    fn gamma(b: &mut [u8], curve: &dyn Fn(f32) -> f32);
}

struct Isp21;
struct Isp30;

/// Turns `bits` on in the module masks of the buffer: enabled, enable update and config update.
fn enable(b: &mut [u8], ens_at: usize, upd_at: usize, cfg_at: usize, bits: u64) {
    let or = |b: &mut [u8], at: usize| {
        let v = u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
        put_u64(b, at, v | bits);
    };
    or(b, ens_at);
    or(b, upd_at);
    or(b, cfg_at);
}

/// Writes `values` as consecutive u16 entries from `at`.
fn put_u16s(b: &mut [u8], at: usize, values: &[u16]) {
    for (i, &v) in values.iter().enumerate() {
        put_u16(b, at + 2 * i, v);
    }
}

/// A `width` x `height` raw frame's lens shading over `sectors` x `sectors` sectors: the sector
/// sizes and gradients, and the R, Gr (= Gb) and B data tables, at the ISP's vertices.
pub(crate) struct Shading {
    sectors: usize,
    x_size: [u16; MAX_SECTORS],
    y_size: [u16; MAX_SECTORS],
    x_grad: [u16; MAX_SECTORS],
    y_grad: [u16; MAX_SECTORS],
    data: [[u16; LSC_DATA]; 3],
}

impl Shading {
    /// The tables from `grid` cells (gains at the cell centres, row by row) for red, green and
    /// blue, resampled bilinearly to the vertices of `sectors` x `sectors` sectors. The sectors
    /// cover the frame's Bayer quads: half its size, split as evenly as it goes.
    fn new(
        width: u16,
        height: u16,
        sectors: usize,
        grid: (usize, usize),
        planes: [&[f64]; 3],
    ) -> Result<Self, Error> {
        if sectors == 0 || sectors > MAX_SECTORS {
            return Err(Error::Shading);
        }
        let sizes = |len: u16| -> [u16; MAX_SECTORS] {
            let half = usize::from(len / 2);
            core::array::from_fn(|i| {
                if i < sectors {
                    ((half * (i + 1)) / sectors - (half * i) / sectors) as u16
                } else {
                    0
                }
            })
        };
        let grad = |s: &[u16; MAX_SECTORS]| {
            s.map(|v| ((32768 + u32::from(v) / 2) / u32::from(v.max(1))) as u16)
        };
        let (x_size, y_size) = (sizes(width), sizes(height));
        Ok(Self {
            sectors,
            x_size,
            y_size,
            x_grad: grad(&x_size),
            y_grad: grad(&y_size),
            data: [
                vertices(planes[0], grid, sectors),
                vertices(planes[1], grid, sectors),
                vertices(planes[2], grid, sectors),
            ],
        })
    }
}

/// A table of `grid` cells at the `sectors + 1` vertices per axis, bilinearly, clamped at the
/// edges; samples of [`LSC_ONE`] in the table's [`LSC_STRIDE`]-wide layout.
fn vertices(src: &[f64], grid: (usize, usize), sectors: usize) -> [u16; LSC_DATA] {
    let (w, h) = grid;
    let at = |len: usize, i: usize| -> (usize, usize, f64) {
        let p = -0.5 + i as f64 * len as f64 / sectors as f64;
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
    for j in 0..=sectors {
        let (y0, y1, fy) = at(h, j);
        for i in 0..=sectors {
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

/// Clamps a value to the `u16` range of a dimension field.
fn dim(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// The sRGB transfer function on `[0, 1]`: the tone curve when the settings have none.
fn srgb(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// The settings' tone curve as a function on `[0, 1]`: the curve's points, or sRGB.
fn tone(curve: &Option<Pwl>) -> impl Fn(f32) -> f32 + '_ {
    move |x: f32| match curve {
        Some(p) if p.points().len() >= 2 => p.eval_clamped(f64::from(x)) as f32,
        _ => srgb(x),
    }
}

/// The whole of one frame's parameters, in the order dev wrote them. The buffer is cleared
/// already.
fn apply<L: Layout>(
    b: &mut [u8],
    s: &IspSettings,
    frame: u32,
    width: u32,
    height: u32,
) -> Result<(), Error> {
    if let Some(ls) = &s.lens_shading {
        let grid = lens_grid(ls);
        if let Some((gw, gh)) = grid {
            let sh = Shading::new(
                dim(width),
                dim(height),
                L::SECTORS,
                (gw, gh),
                [&ls.r, &ls.g, &ls.b].map(Vec::as_slice),
            )?;
            L::lens_shading(b, &sh);
        }
    }
    L::frame(b, frame);
    L::measure(b, dim(width), dim(height));
    let level = |v: f64| (v * LEVEL12).round().clamp(0.0, LEVEL12 - 1.0) as u16;
    L::black_level(b, s.black_cfa().map(level));
    let gain = s.digital_gain;
    L::white_balance(b, s.wb.map(|g| (g * gain) as f32));
    let m = s.ccm.map(|c| c as f32);
    L::colour_matrix(
        b,
        [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
        [0.0; 3],
    );
    let curve = tone(&s.gamma);
    L::gamma(b, &curve);
    Ok(())
}

/// The lens shading's cell grid, if its tables are the size it names (else none: the block stays
/// off, as dev's did).
fn lens_grid(ls: &LensShading) -> Option<(usize, usize)> {
    let (gw, gh) = (ls.width as usize, ls.height as usize);
    let n = gw.checked_mul(gh)?;
    (gw > 0 && gh > 0 && [ls.r.len(), ls.g.len(), ls.b.len()].iter().all(|&l| l == n))
        .then_some((gw, gh))
}

/// The modules' enable bits and the blocks' layouts for one generation, from the library's
/// structures. `$cfg` is the generation's parameter structure.
macro_rules! layout {
    ($name:ident, $cfg:ty, $sectors:literal, $gamma:literal, $sector:path) => {
        impl Layout for $name {
            const SECTORS: usize = $sectors;
            const GAMMA: usize = $gamma;

            fn frame(b: &mut [u8], id: u32) {
                put_u32(b, offset_of!($cfg, frame_id), id);
            }

            fn measure(b: &mut [u8], width: u16, height: u16) {
                // Raw AE, unit 3: the 15x15 zones over the whole frame.
                put_u8(b, offset_of!($cfg, meas.rawae3.rawae_sel), 0);
                put_u8(b, offset_of!($cfg, meas.rawae3.wnd_num), AE_GRID_15X15);
                put_u16(b, offset_of!($cfg, meas.rawae3.win.h_offs), 0);
                put_u16(b, offset_of!($cfg, meas.rawae3.win.v_offs), 0);
                put_u16(b, offset_of!($cfg, meas.rawae3.win.h_size), width);
                put_u16(b, offset_of!($cfg, meas.rawae3.win.v_size), height);
                // Raw histogram, unit 1: luma from the weighted channels, uniform zone weights.
                put_u8(b, offset_of!($cfg, meas.rawhist1.wnd_num), AE_GRID_15X15);
                put_u8(b, offset_of!($cfg, meas.rawhist1.data_sel), 0);
                put_u8(b, offset_of!($cfg, meas.rawhist1.mode), HIST_MODE_Y);
                put_u8(b, offset_of!($cfg, meas.rawhist1.stepsize), 0);
                put_u8(b, offset_of!($cfg, meas.rawhist1.rcc), HIST_RGB_WEIGHTS.0);
                put_u8(b, offset_of!($cfg, meas.rawhist1.gcc), HIST_RGB_WEIGHTS.1);
                put_u8(b, offset_of!($cfg, meas.rawhist1.bcc), HIST_RGB_WEIGHTS.2);
                put_u16(b, offset_of!($cfg, meas.rawhist1.win.h_offs), 0);
                put_u16(b, offset_of!($cfg, meas.rawhist1.win.v_offs), 0);
                put_u16(b, offset_of!($cfg, meas.rawhist1.win.h_size), width);
                put_u16(b, offset_of!($cfg, meas.rawhist1.win.v_size), height);
                let weight = offset_of!($cfg, meas.rawhist1.weight);
                b[weight..weight + 225].fill(1);
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::RAWAE3 | module::RAWHIST1,
                );
            }

            fn black_level(b: &mut [u8], levels: [u16; 4]) {
                let v = |l: u16| l.min(4095) as i16;
                put_u8(b, offset_of!($cfg, others.bls_cfg.enable_auto), 0);
                put_i16(
                    b,
                    offset_of!($cfg, others.bls_cfg.fixed_val.r),
                    v(levels[0]),
                );
                put_i16(
                    b,
                    offset_of!($cfg, others.bls_cfg.fixed_val.gr),
                    v(levels[1]),
                );
                put_i16(
                    b,
                    offset_of!($cfg, others.bls_cfg.fixed_val.gb),
                    v(levels[2]),
                );
                put_i16(
                    b,
                    offset_of!($cfg, others.bls_cfg.fixed_val.b),
                    v(levels[3]),
                );
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::BLS,
                );
            }

            fn white_balance(b: &mut [u8], gains: [f32; 3]) {
                let q = |g: f32| {
                    ((g.clamp(0.0, 65535.0 / GAIN_ONE) * GAIN_ONE + 0.5) as u16).max(GAIN_MIN)
                };
                let (r, g, bl) = (q(gains[0]), q(gains[1]), q(gains[2]));
                // The three gain sets (the ISP's three AWB gain entries) take the same gains.
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain0_red), r);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain0_green_r), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain0_green_b), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain0_blue), bl);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain1_red), r);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain1_green_r), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain1_green_b), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain1_blue), bl);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain2_red), r);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain2_green_r), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain2_green_b), g);
                put_u16(b, offset_of!($cfg, others.awb_gain_cfg.gain2_blue), bl);
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::AWB_GAIN,
                );
            }

            fn colour_matrix(b: &mut [u8], m: [[f32; 3]; 3], offsets: [f32; 3]) {
                let q = |c: f32| (c * CCM_ONE).round().clamp(-32768.0, 32767.0) as i16;
                let o = |c: f32| c.round().clamp(-4095.0, 4095.0) as i16;
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff0_r), q(m[0][0]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff1_r), q(m[0][1]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff2_r), q(m[0][2]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.offset_r), o(offsets[0]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff0_g), q(m[1][0]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff1_g), q(m[1][1]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff2_g), q(m[1][2]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.offset_g), o(offsets[1]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff0_b), q(m[2][0]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff1_b), q(m[2][1]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.coeff2_b), q(m[2][2]));
                put_i16(b, offset_of!($cfg, others.ccm_cfg.offset_b), o(offsets[2]));
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::CCM,
                );
            }

            fn lens_shading(b: &mut [u8], sh: &Shading) {
                let n = sh.sectors;
                let base = offset_of!($cfg, others.lsc_cfg.r_data_tbl);
                put_u16s(b, base, &sh.data[0]);
                put_u16s(b, offset_of!($cfg, others.lsc_cfg.gr_data_tbl), &sh.data[1]);
                put_u16s(b, offset_of!($cfg, others.lsc_cfg.gb_data_tbl), &sh.data[1]);
                put_u16s(b, offset_of!($cfg, others.lsc_cfg.b_data_tbl), &sh.data[2]);
                put_u16s(
                    b,
                    offset_of!($cfg, others.lsc_cfg.x_grad_tbl),
                    &sh.x_grad[..n],
                );
                put_u16s(
                    b,
                    offset_of!($cfg, others.lsc_cfg.y_grad_tbl),
                    &sh.y_grad[..n],
                );
                put_u16s(
                    b,
                    offset_of!($cfg, others.lsc_cfg.x_size_tbl),
                    &sh.x_size[..n],
                );
                put_u16s(
                    b,
                    offset_of!($cfg, others.lsc_cfg.y_size_tbl),
                    &sh.y_size[..n],
                );
                $sector(b);
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::LSC,
                );
            }

            fn gamma(b: &mut [u8], curve: &dyn Fn(f32) -> f32) {
                let at = offset_of!($cfg, others.gammaout_cfg.gamma_y);
                put_u8(b, offset_of!($cfg, others.gammaout_cfg.equ_segm), 1);
                let n = Self::GAMMA;
                for i in 0..n {
                    let x = i as f32 / (n - 1) as f32;
                    let s = (curve(x).clamp(0.0, 1.0) * GAMMA_OUT_MAX + 0.5) as u16;
                    put_u16(b, at + 2 * i, s);
                }
                enable(
                    b,
                    offset_of!($cfg, module_ens),
                    offset_of!($cfg, module_en_update),
                    offset_of!($cfg, module_cfg_update),
                    module::GOC,
                );
            }
        }
    };
}

/// ISP21's lens shading has no 16x16 flag.
fn no_sector(_b: &mut [u8]) {}

/// ISP30's lens shading is 16x16 sectors: the flag the driver reads.
fn sector_16x16(b: &mut [u8]) {
    put_u8(
        b,
        offset_of!(isp3x::isp3x_isp_params_cfg, others.lsc_cfg.sector_16x16),
        1,
    );
}

layout!(Isp21, isp21::isp21_isp_params_cfg, 8, 45, no_sector);
layout!(Isp30, isp3x::isp3x_isp_params_cfg, 16, 49, sector_16x16);
