//! The front end configuration: the black levels the raw output keeps and the statistics
//! weights that follow the white balance, from [`IspSettings`], as the library's front end
//! builder takes them.

use styx_lemnos::pisp::fe::FrontEnd;
use styx_lemnos::pisp::uapi::{BayerOrder, BlaConfig, FeConfig, FeRgbyConfig};
use styx_settings::IspSettings;

use crate::PispError;
use crate::back_end::level16;

/// The RGB-to-Y weights of the statistics: the luma row of the library's full-range BT.601
/// encoding (`be::defaults`), on the front end's 4.10 scale.
fn y_coeffs() -> [f64; 3] {
    let c = styx_lemnos::pisp::be::defaults::Encoding::Jpeg
        .forward()
        .coeffs;
    [f64::from(c[0]), f64::from(c[1]), f64::from(c[2])]
}

/// The front end for one raw geometry.
#[derive(Debug)]
pub struct FrontEndBuilder {
    fe: FrontEnd,
}

impl FrontEndBuilder {
    /// A front end for a `width` x `height` 16-bit Bayer input in `bayer` order.
    pub fn new(width: u16, height: u16, bayer: BayerOrder) -> Self {
        Self {
            fe: FrontEnd::new(width, height, bayer),
        }
    }

    /// Applies `s` and prepares the configuration.
    pub fn update(&mut self, s: &IspSettings) -> Result<FeConfig, PispError> {
        let level = level16(s.black_level);
        let bla = BlaConfig {
            black_level_r: level,
            black_level_gr: level,
            black_level_gb: level,
            black_level_b: level,
            output_black_level: level,
            pad: [0; 2],
        };
        self.fe.set_bla(bla);
        self.fe.set_blc(BlaConfig {
            output_black_level: 0,
            ..bla
        });
        let extra = 1.0 / s.wb.iter().copied().fold(1.0, f64::min).max(0.1);
        let y = |coeff: f64, gain: f64| (coeff * gain * extra).round().clamp(0.0, 16383.0) as u16;
        let [r, g, b] = y_coeffs();
        let rgby = FeRgbyConfig {
            gain_r: y(r, s.wb[0]),
            gain_g: y(g, s.wb[1]),
            gain_b: y(b, s.wb[2]),
            ..self.fe.config().rgby
        };
        self.fe.set_rgby(rgby);
        self.fe
            .prepare()
            .map_err(|e| PispError::Config(format!("front end: {e}")))
    }
}
