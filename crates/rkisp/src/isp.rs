//! A Rockchip ISP's ([`Isp`], Lemnos's) parameter buffer
//! ([`AnyParams`]) and statistics whatever the generation: ISP21 (RK3566 / RK3568), ISP30
//! (RK3588 / RK3588S), ISP32 (RV1106 / RV1103) and ISP32-lite (RK3562). Written, untested on
//! board: none of these has run on its chip yet.

use styx_algo::Statistics;

use crate::{Generation, Isp, ParamBuffer, Params, isp32};

/// What Styx builds and reads for an [`Isp`]: its parameter buffer and its statistics, whatever
/// the generation. [`Isp`] itself (the generation, from the SoC) is Lemnos's.
pub trait IspBuffers {
    /// A parameter buffer for this ISP, every module off.
    fn params(self) -> AnyParams;

    /// This ISP's statistics buffer `buf` as [`Statistics`] (see [`crate::statistics`] and
    /// [`isp32::statistics`]), written into `out`; `None` when `buf` is short.
    fn statistics(self, buf: &[u8], pixels_per_zone: u32, out: &mut Statistics) -> Option<()>;
}

impl IspBuffers for Isp {
    fn params(self) -> AnyParams {
        match self {
            Self::Isp21 => AnyParams::Rk(Params::new(Generation::Isp21)),
            Self::Isp30 => AnyParams::Rk(Params::new(Generation::Isp30)),
            Self::Isp32 => AnyParams::Isp32(isp32::Params::new(isp32::Variant::Isp32)),
            Self::Isp32Lite => AnyParams::Isp32(isp32::Params::new(isp32::Variant::Isp32Lite)),
        }
    }

    fn statistics(self, buf: &[u8], pixels_per_zone: u32, out: &mut Statistics) -> Option<()> {
        match self {
            Self::Isp21 => crate::statistics(Generation::Isp21, buf, pixels_per_zone, out),
            Self::Isp30 => crate::statistics(Generation::Isp30, buf, pixels_per_zone, out),
            Self::Isp32 => isp32::statistics(isp32::Variant::Isp32, buf, pixels_per_zone, out),
            Self::Isp32Lite => {
                isp32::statistics(isp32::Variant::Isp32Lite, buf, pixels_per_zone, out)
            }
        }
    }
}

/// A parameter buffer of any generation ([`IspBuffers::params`]), built the same way.
pub enum AnyParams {
    /// ISP21 or ISP30.
    Rk(Params),
    /// ISP32 or ISP32-lite.
    Isp32(isp32::Params),
}

impl core::fmt::Debug for AnyParams {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AnyParams")
            .field("isp", &self.isp())
            .finish_non_exhaustive()
    }
}

macro_rules! forward {
    ($($(#[$doc:meta])* fn $name:ident($($arg:ident: $ty:ty),*);)*) => {$(
        $(#[$doc])*
        pub fn $name(self, $($arg: $ty),*) -> Self {
            match self {
                Self::Rk(p) => Self::Rk(p.$name($($arg),*)),
                Self::Isp32(p) => Self::Isp32(p.$name($($arg),*)),
            }
        }
    )*};
}

impl AnyParams {
    forward! {
        /// See [`Params::frame`].
        fn frame(id: u32);
        /// See [`Params::measure`].
        fn measure(width: u16, height: u16);
        /// See [`Params::black_level`].
        fn black_level(levels: [u16; 4]);
        /// See [`Params::white_balance`].
        fn white_balance(gains: [f32; 3]);
        /// See [`Params::colour_matrix`].
        fn colour_matrix(m: [[f32; 3]; 3], offsets: [f32; 3]);
        /// See [`Params::lens_shading`].
        fn lens_shading(width: u16, height: u16, grid: (usize, usize), planes: [&[f64]; 3]);
    }

    /// See [`Params::reset`].
    pub fn reset(self) -> Self {
        match self {
            Self::Rk(p) => Self::Rk(p.reset()),
            Self::Isp32(p) => Self::Isp32(p.reset()),
        }
    }

    /// See [`Params::gamma`].
    pub fn gamma(self, curve: impl Fn(f32) -> f32) -> Self {
        match self {
            Self::Rk(p) => Self::Rk(p.gamma(curve)),
            Self::Isp32(p) => Self::Isp32(p.gamma(curve)),
        }
    }

    /// The ISP these parameters are for.
    pub fn isp(&self) -> Isp {
        match self {
            Self::Rk(p) => match p.generation() {
                Generation::Isp21 => Isp::Isp21,
                Generation::Isp30 => Isp::Isp30,
            },
            Self::Isp32(p) => match p.variant() {
                isp32::Variant::Isp32 => Isp::Isp32,
                isp32::Variant::Isp32Lite => Isp::Isp32Lite,
            },
        }
    }

    /// The buffer the driver takes.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Rk(p) => p.as_bytes(),
            Self::Isp32(p) => p.as_bytes(),
        }
    }
}

impl ParamBuffer for Params {
    fn param_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl ParamBuffer for isp32::Params {
    fn param_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl ParamBuffer for AnyParams {
    fn param_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_isp_builds_its_own_buffer() {
        for isp in [Isp::Isp21, Isp::Isp30, Isp::Isp32, Isp::Isp32Lite] {
            let p = isp
                .params()
                .frame(3)
                .measure(1280, 800)
                .black_level([256; 4])
                .white_balance([2.0, 1.0, 1.5])
                .colour_matrix(
                    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    [0.0; 3],
                )
                .gamma(|x| x);
            assert_eq!(p.isp(), isp);
            let want = match isp {
                Isp::Isp21 => Params::new(Generation::Isp21).as_bytes().len(),
                Isp::Isp30 => Params::new(Generation::Isp30).as_bytes().len(),
                Isp::Isp32 | Isp::Isp32Lite => 11145,
            };
            assert_eq!(p.as_bytes().len(), want);
            let p = p.reset();
            assert!(p.as_bytes().iter().all(|&b| b == 0), "{isp:?} reset");
        }
    }

    #[test]
    fn short_statistics_buffers_are_refused() {
        let mut out = Statistics::default();
        for isp in [Isp::Isp21, Isp::Isp30, Isp::Isp32, Isp::Isp32Lite] {
            assert!(isp.statistics(&[0; 16], 100, &mut out).is_none());
        }
    }
}
