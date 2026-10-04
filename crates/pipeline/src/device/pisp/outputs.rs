//! How the back end's output buffers read on the CPU.

use super::PispPipeline;

impl PispPipeline {
    /// Whether output `i`'s buffers are cached for the CPU (a cached dma-heap) rather than the
    /// driver's uncached ones.
    pub fn output_cached(&self, i: usize) -> bool {
        self.be_dev.as_ref().is_some_and(|b| b.output_cached(i))
    }
}
