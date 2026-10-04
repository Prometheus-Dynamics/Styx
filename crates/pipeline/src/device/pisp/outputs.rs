//! The back end's outputs: how their buffers read on the CPU, and their crops.

use styx_pisp::uapi::{BE_MIN_TILE_HEIGHT, BE_MIN_TILE_WIDTH, BeCropConfig};

use super::PispPipeline;
use crate::error::{PipelineError, Result};

impl PispPipeline {
    /// Whether output `i`'s buffers are cached for the CPU (a cached dma-heap) rather than the
    /// driver's uncached ones.
    pub fn output_cached(&self, i: usize) -> bool {
        self.be_dev.as_ref().is_some_and(|b| b.output_cached(i))
    }

    /// Output `i`'s crop of the sensor frame (see [`super::PispOptions::crop`]).
    pub fn output_crop(&self, i: usize) -> Option<BeCropConfig> {
        self.options.crop.get(i).copied().flatten()
    }

    /// Crops output `i` to `crop` of the sensor frame (`None`: all of it) from the next frame
    /// on. The crop must lie inside the frame, with even offsets and sizes of at least 16×16;
    /// an output without a size of its own then delivers the crop at full resolution. Fails,
    /// changing nothing, otherwise.
    pub fn set_output_crop(&mut self, i: usize, crop: Option<BeCropConfig>) -> Result<()> {
        if i >= self.options.crop.len() {
            return Err(PipelineError::Config(format!("no output {i}")));
        }
        if self.options.crop[i] == crop {
            return Ok(());
        }
        if let Some(c) = crop {
            check_crop(c, (self.info.width, self.info.height))?;
        }
        self.be.set_output_crop(i, crop)?;
        self.options.crop[i] = crop;
        Ok(())
    }
}

/// Whether the back end can crop a `frame`-sized input to `c`.
pub(crate) fn check_crop(c: BeCropConfig, frame: (u32, u32)) -> Result<()> {
    let (x, y, w, h) = (
        u32::from(c.offset_x),
        u32::from(c.offset_y),
        u32::from(c.width),
        u32::from(c.height),
    );
    let fail = |why: &str| {
        Err(PipelineError::Config(format!(
            "crop {w}x{h} at ({x}, {y}) of {}x{}: {why}",
            frame.0, frame.1
        )))
    };
    if w < BE_MIN_TILE_WIDTH || h < BE_MIN_TILE_HEIGHT {
        return fail("smaller than 16x16");
    }
    if (x | y | w | h) & 1 != 0 {
        return fail("offsets and sizes must be even");
    }
    if x + w > frame.0 || y + h > frame.1 {
        return fail("outside the frame");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(x: u16, y: u16, w: u16, h: u16) -> BeCropConfig {
        BeCropConfig {
            offset_x: x,
            offset_y: y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn crops_must_fit_the_frame_on_even_pixels() {
        let frame = (1280, 800);
        assert!(check_crop(crop(0, 0, 1280, 800), frame).is_ok());
        assert!(check_crop(crop(640, 400, 320, 200), frame).is_ok());
        assert!(check_crop(crop(1, 0, 64, 64), frame).is_err());
        assert!(check_crop(crop(0, 0, 63, 64), frame).is_err());
        assert!(check_crop(crop(1200, 0, 100, 64), frame).is_err());
        assert!(check_crop(crop(0, 0, 8, 64), frame).is_err());
    }
}
