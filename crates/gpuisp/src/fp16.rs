//! The fp16 pictures: an option for devices with `shaderFloat16` (`shaders/full16.comp`,
//! `half16.comp`). Not bit-exact with the integer arithmetic; bounded against `styx-softisp`'s
//! `Arithmetic::Half` instead.

use ash::vk;
use styx_softisp::{Arithmetic, Demosaic, Scale};

use crate::isp::GpuIsp;

impl GpuIsp {
    /// Runs the pictures in fp16 (default: off), as `styx-softisp`'s `Arithmetic::Half`: the
    /// front end, bilinear demosaic, colour matrix and tone curve in float16 (the statistics
    /// stay on the integer arithmetic). Used only where the device has `shaderFloat16`, the
    /// frames are at most 10 bits and the demosaic is bilinear (`Half`'s conditions); else the
    /// integer pictures run.
    pub fn set_half_precision(&mut self, on: bool) {
        self.half = on;
    }

    /// Whether [`Self::set_half_precision`] is on (the pictures may still run in integers, see
    /// [`Self::arithmetic`]).
    pub fn half_precision(&self) -> bool {
        self.half
    }

    /// Whether the next pictures run in fp16.
    pub(crate) fn fp16_active(&self) -> bool {
        self.half
            && self.k.full16 != vk::Pipeline::null()
            && self.format().packing.bit_depth() <= 10
            && self.params().demosaic == Demosaic::Bilinear
    }

    /// The pipeline of a picture at `scale`.
    pub(crate) fn picture_pipeline(&self, scale: Scale) -> vk::Pipeline {
        match (scale, self.fp16_active()) {
            (Scale::Full, false) => self.k.full,
            (Scale::Half, false) => self.k.half,
            (Scale::Full, true) => self.k.full16,
            (Scale::Half, true) => self.k.half16,
        }
    }

    /// The arithmetic the pictures run in: [`Arithmetic::Half`] when fp16 is on and applies,
    /// [`Arithmetic::IntPolyTone`] with the tone curve as quadratics that fit it, else
    /// [`Arithmetic::Int`].
    pub fn arithmetic(&self) -> Arithmetic {
        if self.fp16_active() {
            Arithmetic::Half
        } else if self.tables.block.tone_lut == 2 {
            Arithmetic::IntPolyTone
        } else {
            Arithmetic::Int
        }
    }
}
