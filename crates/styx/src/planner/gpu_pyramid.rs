//! Pyramid levels on the GPU (`PyramidSource::Gpu`, feature `gpu-wgpu`): styx-wgpu's box
//! filter, bit for bit the CPU's, attached as the CPU's levels are.

use std::sync::OnceLock;

use parking_lot::Mutex;
use styx_core::prelude::*;
use styx_wgpu::{BoxPyramid, Gpu, GpuError, Level, Luma};

/// The process's GPU, made on first use; `None` (said once) where there is none.
fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| match Gpu::new() {
        Ok(gpu) => {
            crate::trace::info!(adapter = gpu.name(), "GPU pyramid levels");
            Some(gpu)
        }
        Err(e) => {
            crate::trace::warn!(error = %e, "no GPU: pyramid levels on the CPU");
            None
        }
    })
    .as_ref()
}

/// The kernel behind a trait object: proving wgpu's types `Send` and `Sync` inside every
/// type that holds a preparer overflows the compiler's recursion limit.
trait Kernel: Send {
    fn run(
        &mut self,
        src: Luma<'_>,
        levels: u8,
        out: &mut dyn FnMut(Level<'_>),
    ) -> Result<(), GpuError>;
}

impl Kernel for BoxPyramid {
    fn run(
        &mut self,
        src: Luma<'_>,
        levels: u8,
        out: &mut dyn FnMut(Level<'_>),
    ) -> Result<(), GpuError> {
        BoxPyramid::run(self, src, levels, out)
    }
}

pub(super) struct GpuPyramid {
    pyramid: Mutex<Box<dyn Kernel>>,
}

impl GpuPyramid {
    /// `None` where there is no GPU (the CPU makes the levels).
    pub(super) fn new() -> Option<Self> {
        gpu().map(|gpu| Self {
            pyramid: Mutex::new(Box::new(BoxPyramid::new(gpu))),
        })
    }

    /// Levels 1 to `levels` of `frame` from the GPU, finest first; none when it has level 1
    /// already (an ISP's: the CPU fills the rest).
    pub(super) fn levels(
        &self,
        frame: &FrameLease,
        levels: u8,
        alignment: usize,
        pool: &BufferPool,
    ) -> Result<Vec<(u8, FrameLease)>, String> {
        if frame.pyramid_level(1).is_some() {
            return Ok(Vec::new());
        }
        let mut made = Vec::with_capacity(usize::from(levels));
        {
            let rows = frame.luma_rows().map_err(|e| e.to_string())?;
            let src = Luma {
                data: rows.as_bytes(),
                width: rows.row_bytes() as u32,
                height: rows.len() as u32,
                stride: rows.stride(),
            };
            let mut failed = None;
            self.pyramid
                .lock()
                .run(src, levels, &mut |l| {
                    let width = l.width as usize;
                    let level = luma_level_in(
                        frame,
                        l.level,
                        width,
                        l.height as usize,
                        alignment,
                        pool,
                        |y, out| out.copy_from_slice(&l.data[y * l.stride..y * l.stride + width]),
                    );
                    match level {
                        Ok(f) => made.push((l.level, f)),
                        Err(e) => failed = Some(e.to_string()),
                    }
                })
                .map_err(|e| e.to_string())?;
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(made)
    }
}
