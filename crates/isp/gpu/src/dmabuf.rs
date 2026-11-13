//! [`GpuIsp`]'s zero-copy side: capture dma-bufs registered and read in place, and the export
//! ring whose buffers go out as dma-bufs (`VK_EXT_external_memory_dma_buf`).

use std::os::fd::{BorrowedFd, OwnedFd};

use styx_soft::Scale;

use crate::error::GpuError;
use crate::input::Input;
use crate::isp::{ExportedFrame, GpuIsp, ImportId, Imported, Region, Target};
use crate::layout::OutputKind;
use crate::memory::Buffer;

impl GpuIsp {
    /// Process one frame into the next buffer of the export ring and leave it there:
    /// [`Self::export_dmabuf`] hands it to other devices or processes, [`Self::exported`]
    /// reads it on the CPU.
    pub fn process_to_export(
        &mut self,
        input: Input<'_>,
        stride: usize,
        scale: Scale,
        kind: OutputKind,
    ) -> Result<ExportedFrame, GpuError> {
        if self.ctx.external_fd.is_none() {
            return Err(GpuError::Unsupported("dma-buf export".into()));
        }
        let layout = self.layout(kind, scale)?;
        let slot = self.ring_next;
        self.ensure_slot(Target::Ring(slot), layout.size)?;
        let stats = self.run(
            input,
            stride,
            scale,
            &layout,
            Target::Ring(slot),
            Region::Frame,
        )?;
        self.ring_next = (slot + 1) % self.ring_len;
        Ok(ExportedFrame {
            slot,
            layout,
            stats,
        })
    }

    /// A dma-buf of export ring slot `slot` (a new file each call).
    pub fn export_dmabuf(&self, slot: usize) -> Result<OwnedFd, GpuError> {
        let s = self
            .ring
            .get(slot)
            .ok_or_else(|| GpuError::Unsupported(format!("no export slot {slot}")))?;
        s.buffer.export_dmabuf()
    }

    /// The bytes of export ring slot `slot`.
    pub fn exported(&self, slot: usize) -> Result<&[u8], GpuError> {
        let s = self
            .ring
            .get(slot)
            .ok_or_else(|| GpuError::Unsupported(format!("no export slot {slot}")))?;
        s.buffer.invalidate()?;
        Ok(s.buffer.bytes())
    }

    /// Register a dma-buf holding raw frames (`size` bytes; for example a capture buffer)
    /// so that frames in it are read in place ([`Input::DmaBuf`]). The file is duplicated;
    /// the caller keeps its own.
    pub fn import_dmabuf(&mut self, fd: BorrowedFd<'_>, size: usize) -> Result<ImportId, GpuError> {
        let buffer = Buffer::import_dmabuf(&self.ctx, fd, size as u64)?;
        let set = if self.separate {
            None
        } else {
            Some(self.k.set(1, &[buffer.buffer])?)
        };
        let entry = Some(Imported { buffer, set });
        let id = match self.imports.iter().position(Option::is_none) {
            Some(i) => {
                self.imports[i] = entry;
                i
            }
            None => {
                self.imports.push(entry);
                self.imports.len() - 1
            }
        };
        Ok(ImportId(id))
    }

    /// Forget an imported dma-buf.
    pub fn release_import(&mut self, id: ImportId) {
        if let Some(Some(i)) = self.imports.get_mut(id.0).map(Option::take)
            && let Some(set) = i.set
        {
            self.k.free_set(set);
        }
    }
}
