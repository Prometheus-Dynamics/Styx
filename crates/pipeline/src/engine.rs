//! What processes the software path's frames: `styx-softisp` on the CPU, or (feature `gpu`)
//! `styx-gpuisp`, the same pipeline as Vulkan compute shaders, which takes the same
//! parameters and gives the same pictures and statistics.

use alloc::string::String;
use alloc::vec::Vec;

use styx_softisp::{
    Arithmetic, IspParams, IspStats, OutputBuffers, RawFormat, Scale, SoftIsp, Window,
};

use crate::error::Result;

/// Which ISP a [`SoftLoop`](crate::SoftLoop) runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IspEngine {
    /// `styx-softisp` on this many threads (0: one per CPU).
    Cpu { threads: usize },
    /// `styx-gpuisp` on this Vulkan device.
    Gpu { device: String },
}

/// A raw frame: bytes in memory, or (GPU ISP) a dma-buf it reads in place.
#[derive(Clone, Copy, Debug)]
pub enum RawFrame<'a> {
    Bytes(&'a [u8]),
    /// A capture buffer's dma-buf, `len` bytes, the frame at its start. Each buffer (told
    /// apart by its inode) is imported once and kept.
    #[cfg(feature = "gpu")]
    DmaBuf {
        fd: std::os::fd::BorrowedFd<'a>,
        len: usize,
    },
}

/// What the software ISP makes of a frame.
#[derive(Debug)]
pub enum SoftTarget<'a> {
    /// The whole frame at a scale (with its statistics in the same pass).
    Frame(Scale, OutputBuffers<'a>),
    /// Parts of it: the frame's statistics are gathered on their own (or by the overview's
    /// pass), so a small region costs a small part of a frame. CPU ISP only.
    Parts(SoftParts<'a>),
}

/// Regions of a frame and an overview of all of it ([`SoftTarget::Parts`]).
#[derive(Debug, Default)]
pub struct SoftParts<'a> {
    /// Regions at full resolution, each into buffers of its size: the same pixels as that
    /// region of the whole frame's picture (`SoftIsp::process_window`). The whole frame as a
    /// region is the whole frame's picture.
    pub regions: Vec<(Window, OutputBuffers<'a>)>,
    /// The whole frame binned by an even factor, into buffers of
    /// `SoftIsp::binned_size(factor)`; its pass also gathers the statistics
    /// (`SoftIsp::process_binned`).
    pub overview: Option<(u32, OutputBuffers<'a>)>,
}

/// `isp` on `threads` row bands (the helper thread pool needs `std`: one band without it).
pub(crate) fn with_threads(isp: SoftIsp, threads: usize) -> SoftIsp {
    #[cfg(feature = "std")]
    return isp.with_threads(threads);
    #[cfg(not(feature = "std"))]
    {
        let _ = threads;
        isp
    }
}

/// The row bands `isp` runs (1 without `std`).
pub(crate) fn threads(isp: &SoftIsp) -> usize {
    #[cfg(feature = "std")]
    return isp.threads();
    #[cfg(not(feature = "std"))]
    {
        let _ = isp;
        1
    }
}

// One per loop, never moved per frame: the size difference costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Engine {
    Cpu(SoftIsp),
    #[cfg(feature = "gpu")]
    Gpu(Box<gpu::Gpu>),
}

impl Engine {
    pub fn kind(&self) -> IspEngine {
        match self {
            Self::Cpu(i) => IspEngine::Cpu {
                threads: threads(i),
            },
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => IspEngine::Gpu {
                device: g.isp.context().info().name.clone(),
            },
        }
    }

    pub fn set_params(&mut self, params: IspParams) -> Result<()> {
        match self {
            Self::Cpu(i) => i.set_params(params)?,
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.set_params(params)?,
        }
        Ok(())
    }

    pub fn set_statistics(&mut self, on: bool) {
        match self {
            Self::Cpu(i) => i.set_statistics(on),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.set_statistics(on),
        }
    }

    pub fn set_copy_input(&mut self, copy: bool) {
        match self {
            Self::Cpu(i) => i.set_copy_input(copy),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.copy_input = copy,
        }
    }

    /// Whether raw input is copied before processing: rows staged (CPU), or uploaded into the
    /// GPU's buffer (GPU, which also copies its output back).
    pub fn copies_input(&self) -> bool {
        match self {
            Self::Cpu(i) => i.copies_input(),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.copy_input,
        }
    }

    pub fn arithmetic(&self) -> Arithmetic {
        match self {
            Self::Cpu(i) => i.arithmetic(),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.arithmetic(),
        }
    }

    pub fn format(&self) -> RawFormat {
        match self {
            Self::Cpu(i) => i.format(),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.format(),
        }
    }

    #[cfg(feature = "gpu")]
    pub fn params(&self) -> &IspParams {
        match self {
            Self::Cpu(i) => i.params(),
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.params(),
        }
    }

    /// GPU time of the last frame (GPU ISP with timestamp queries).
    pub fn gpu_time(&self) -> Option<core::time::Duration> {
        match self {
            Self::Cpu(_) => None,
            #[cfg(feature = "gpu")]
            Self::Gpu(g) => g.isp.gpu_time(),
        }
    }

    /// Forget imported capture buffers (a new stream has new ones).
    pub fn forget_imports(&mut self) {
        #[cfg(feature = "gpu")]
        if let Self::Gpu(g) = self {
            g.forget_imports();
        }
    }

    /// Makes `target` of `raw`; the statistics when the parameters ask for them.
    pub fn process_target(
        &mut self,
        raw: RawFrame<'_>,
        stride: usize,
        target: SoftTarget<'_>,
    ) -> Result<Option<IspStats>> {
        let parts = match target {
            SoftTarget::Frame(scale, out) => return self.process(raw, stride, scale, out),
            SoftTarget::Parts(parts) => parts,
        };
        let (isp, b) = match (self, raw) {
            (Self::Cpu(isp), RawFrame::Bytes(b)) => (isp, b),
            #[cfg(feature = "gpu")]
            _ => {
                return Err(crate::PipelineError::Config(
                    "regions of frames need the CPU ISP and frames in memory".into(),
                ));
            }
        };
        let stats = match parts.overview {
            Some((factor, out)) => isp.process_binned(b, stride, factor, out)?,
            None => isp.statistics(b, stride)?,
        };
        for (window, out) in parts.regions {
            isp.process_window(b, stride, window, Scale::Full, out)?;
        }
        Ok(stats)
    }

    pub fn process(
        &mut self,
        raw: RawFrame<'_>,
        stride: usize,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<Option<IspStats>> {
        match (self, raw) {
            (Self::Cpu(i), RawFrame::Bytes(b)) => Ok(i.process(b, stride, scale, out)?),
            #[cfg(feature = "gpu")]
            (Self::Cpu(_), RawFrame::DmaBuf { .. }) => Err(crate::PipelineError::Config(
                "dma-buf frames need the GPU ISP".into(),
            )),
            #[cfg(feature = "gpu")]
            (Self::Gpu(g), raw) => g.process(raw, stride, scale, out),
        }
    }
}

#[cfg(feature = "gpu")]
pub(crate) mod gpu {
    use std::collections::HashMap;

    use styx_gpuisp::{GpuIsp, ImportId, Input};
    use styx_softisp::{IspStats, OutputBuffers, Scale};

    use super::RawFrame;
    use crate::error::Result;

    /// Imported capture buffers kept at most.
    const MAX_IMPORTS: usize = 32;

    /// The GPU ISP with the capture buffers it imported.
    pub(crate) struct Gpu {
        pub isp: GpuIsp,
        /// By the dma-buf's inode: the buffer and its size.
        imports: HashMap<u64, (ImportId, usize)>,
        /// Kept for switching back to the CPU (the GPU reads frames however they are mapped).
        pub copy_input: bool,
    }

    impl Gpu {
        pub fn new(isp: GpuIsp, copy_input: bool) -> Self {
            Self {
                isp,
                imports: HashMap::new(),
                copy_input,
            }
        }

        pub fn forget_imports(&mut self) {
            for (_, (id, _)) in self.imports.drain() {
                self.isp.release_import(id);
            }
        }

        pub fn process(
            &mut self,
            raw: RawFrame<'_>,
            stride: usize,
            scale: Scale,
            out: OutputBuffers<'_>,
        ) -> Result<Option<IspStats>> {
            let input = match raw {
                RawFrame::Bytes(b) => Input::Bytes(b),
                RawFrame::DmaBuf { fd, len } => {
                    use std::os::unix::fs::MetadataExt;
                    let ino = std::fs::File::from(fd.try_clone_to_owned()?)
                        .metadata()?
                        .ino();
                    let id = match self.imports.get(&ino) {
                        Some(&(id, l)) if l == len => id,
                        _ => {
                            if let Some((old, _)) = self.imports.remove(&ino) {
                                self.isp.release_import(old);
                            }
                            // More buffers than any capture queue has: some are an earlier
                            // stream's, gone but for this reference.
                            if self.imports.len() >= MAX_IMPORTS {
                                self.forget_imports();
                            }
                            let id = self.isp.import_dmabuf(fd, len)?;
                            self.imports.insert(ino, (id, len));
                            id
                        }
                    };
                    Input::DmaBuf { id, offset: 0 }
                }
            };
            Ok(self.isp.process_input(input, stride, scale, out)?)
        }
    }
}
