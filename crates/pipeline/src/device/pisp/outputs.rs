//! The back end's outputs: how their buffers read on the CPU, and their crops.

use styx_pisp::device::{BackEndStream, BeJob};
use styx_pisp::uapi::BeCropConfig;

use super::PispPipeline;
use crate::error::{PipelineError, Result};
use crate::pisp_passes::check_crop;

impl PispPipeline {
    /// Whether output `i`'s buffers are cached for the CPU (a cached dma-heap) rather than the
    /// driver's uncached ones.
    pub fn output_cached(&self, i: usize) -> bool {
        self.be_dev.as_ref().is_some_and(|b| b.output_cached(i))
    }

    /// The bytes of output `i`'s buffer `index` (a frame's job or one of its passes).
    pub fn output_buffer(&self, i: usize, index: u32) -> Option<&[u8]> {
        self.be_dev.as_ref()?.output_data(i, index)
    }

    /// The dma-buf of output `i`'s buffer `index`.
    pub fn output_buffer_dmabuf(
        &self,
        i: usize,
        index: u32,
    ) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.be_dev.as_ref()?.output_dmabuf(i, index)
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

impl PispPipeline {
    /// Output `i`'s bytes for a frame's job.
    pub fn output(&self, i: usize, job: &BeJob) -> Option<&[u8]> {
        self.be_dev.as_ref()?.output_data(i, job.outputs[i]?)
    }

    /// Brackets CPU reads of output `i` of a frame's job: call with `start` before reading
    /// [`Self::output`] and without after (see [`BackEndStream::sync_output`]).
    pub fn sync_output(&self, i: usize, job: &BeJob, start: bool) -> Result<()> {
        let (Some(b), Some(index)) = (self.be_dev.as_ref(), job.outputs.get(i).copied().flatten())
        else {
            return Ok(());
        };
        Ok(b.sync_output(i, index, start)?)
    }

    /// Output `i`'s dma-buf for a frame's job.
    pub fn output_dmabuf(&self, i: usize, job: &BeJob) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.be_dev.as_ref()?.output_dmabuf(i, job.outputs[i]?)
    }

    /// Adopts the back end's spare buffers (`PispOptions::be_buffers_initial`) and asks for the
    /// next ones: call between frames. Never allocates on the calling thread.
    pub fn top_up_outputs(&mut self) {
        if let Some(b) = self.be_dev.as_mut() {
            b.top_up();
        }
    }

    /// Frames dropped per output because no back end buffer was free or ready.
    pub fn spare_misses(&self) -> [u64; 2] {
        self.be_dev
            .as_ref()
            .map_or([0; 2], BackEndStream::spare_misses)
    }

    /// Spare buffers adopted per output without a prepare (see [`BackEndStream::spare_unprepared`]).
    pub fn spare_unprepared(&self) -> [u64; 2] {
        self.be_dev
            .as_ref()
            .map_or([0; 2], BackEndStream::spare_unprepared)
    }

    /// Gives output `i`'s buffer `index` back (one output of a job, e.g. when its two outputs
    /// go to different consumers).
    pub fn release_output(&mut self, i: usize, index: u32) {
        if let Some(b) = self.be_dev.as_mut() {
            b.release_output(i, index);
        }
    }

    /// Gives a frame's output buffers back.
    pub fn release(&mut self, job: &BeJob) {
        if let Some(b) = self.be_dev.as_mut() {
            b.release(job);
        }
    }
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
