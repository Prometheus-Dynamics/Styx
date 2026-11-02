//! [`IspSettings`] as the software ISP's parameters.

use alloc::vec::Vec;

#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

use styx_algo::LensShading;
use styx_softisp as soft;

use super::IspSettings;

impl IspSettings {
    /// Software ISP parameters for `bits`-bit samples, keeping `base`'s demosaic, YUV
    /// matrix and statistics settings.
    ///
    /// The tuning's denoise and sharpening apply only where `base` has them on (any value: the
    /// tuning's strengths replace it), so they cost nothing unless asked for.
    pub fn softisp(&self, bits: u8, base: &soft::IspParams) -> soft::IspParams {
        let mut out = soft::IspParams::default();
        self.softisp_into(bits, base, &mut out);
        out
    }

    /// [`Self::softisp`] into `out`, reusing its lens shading and tone curve buffers.
    pub fn softisp_into(&self, bits: u8, base: &soft::IspParams, out: &mut soft::IspParams) {
        let full = f64::from((1u32 << bits) - 1);
        let level = (self.black_level * f64::from(1u32 << bits))
            .round()
            .clamp(0.0, full - 1.0);
        let m = self.ccm.map(|v| v.clamp(-3.99, 3.99) as f32);
        // The output stages run only where `base` asks for them; their strengths are the tuning's.
        let (denoise, sharpen) = crate::soft::post_stages(&self.denoise, self.sharpen);
        let denoise = denoise.filter(|_| base.denoise.is_some());
        let sharpen = sharpen.filter(|_| base.sharpen.is_some());
        let (mut lens_shading, mut tone) = (out.lens_shading.take(), out.tone.take());
        match self.lens_shading.as_ref() {
            Some(ls) if soft_lens_shading_fits(ls) => {
                soft_lens_shading_into(ls, lens_shading.get_or_insert_with(Default::default))
            }
            _ => lens_shading = None,
        }
        match (&self.gamma, &mut tone) {
            (Some(p), Some(soft::ToneCurve::Points { points })) => {
                points.clear();
                points.extend(p.points().iter().map(|&(x, y)| [x as f32, y as f32]));
            }
            (Some(p), tone) => {
                *tone = Some(soft::ToneCurve::Points {
                    points: p
                        .points()
                        .iter()
                        .map(|&(x, y)| [x as f32, y as f32])
                        .collect(),
                })
            }
            (None, tone) => *tone = Some(soft::ToneCurve::Srgb),
        }
        *out = soft::IspParams {
            denoise,
            sharpen,
            black_level: Some(soft::BlackLevel::uniform(level as u16)),
            white_balance: Some(soft::WhiteBalance {
                r: self.wb[0] as f32,
                g: self.wb[1] as f32,
                b: self.wb[2] as f32,
            }),
            digital_gain: self.digital_gain as f32,
            lens_shading,
            ccm: Some(soft::ColorMatrix {
                m: [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
            }),
            tone,
            ..base.without_tables()
        };
    }
}

/// Whether the software ISP takes `ls` (complete grids of at least 2x2 nodes).
fn soft_lens_shading_fits(ls: &LensShading) -> bool {
    let n = (ls.width * ls.height) as usize;
    ls.width >= 2 && ls.height >= 2 && [&ls.r, &ls.g, &ls.b].iter().all(|t| t.len() == n)
}

/// `ls` as the software ISP's grid, written into `out` (its buffers reused).
fn soft_lens_shading_into(ls: &LensShading, out: &mut soft::LensShading) {
    let f = |to: &mut Vec<f32>, from: &[f64]| {
        to.clear();
        to.extend(from.iter().map(|&v| v as f32));
    };
    (out.width, out.height) = (ls.width, ls.height);
    f(&mut out.r, &ls.r);
    f(&mut out.g, &ls.g);
    f(&mut out.b, &ls.b);
}
