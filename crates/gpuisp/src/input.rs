//! Where a raw frame comes from: memory, a registered dma-buf, or a buffer of the same device.

use std::marker::PhantomData;

use ash::vk;

use crate::isp::ImportId;

/// A raw frame for [`GpuIsp::process_input`](crate::GpuIsp::process_input).
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// Bytes in memory: copied into a buffer the GPU reads (one `memcpy`).
    Bytes(&'a [u8]),
    /// A dma-buf registered with [`GpuIsp::import_dmabuf`](crate::GpuIsp::import_dmabuf), the
    /// frame starting `offset` bytes in: read by the GPU in place (zero-copy).
    DmaBuf { id: ImportId, offset: usize },
    /// A buffer of the ISP's own device (Daedalus's imported frames,
    /// [`ExternalBuffer::from_frame`]): read in place (zero-copy).
    Buffer(ExternalBuffer<'a>),
}

/// A `VkBuffer` of the ISP's device holding a raw frame, for [`Input::Buffer`].
#[derive(Clone, Copy, Debug)]
pub struct ExternalBuffer<'a> {
    pub(crate) device: vk::Device,
    pub(crate) buffer: vk::Buffer,
    /// Byte offset of the frame in the buffer.
    pub(crate) offset: usize,
    /// Bytes of the buffer from the frame's start on.
    pub(crate) len: usize,
    _owner: PhantomData<&'a ()>,
}

impl ExternalBuffer<'_> {
    /// The frame `offset` bytes into `buffer` (`len` bytes from there) on `device`.
    ///
    /// # Safety
    /// `buffer` is a live buffer of `device` with storage-buffer and transfer-source usage,
    /// at least `offset + len` bytes, that nothing writes until the ISP call it is passed to
    /// returns.
    pub unsafe fn new(device: vk::Device, buffer: vk::Buffer, offset: usize, len: usize) -> Self {
        Self {
            device,
            buffer,
            offset,
            len,
            _owner: PhantomData,
        }
    }
}

#[cfg(feature = "daedalus")]
impl<'a> ExternalBuffer<'a> {
    /// The first plane of a frame Daedalus imported (`GpuFrameImports::import`) on `gpu`,
    /// for a [`GpuIsp`](crate::GpuIsp) made with [`GpuContext::from_daedalus`](crate::GpuContext::from_daedalus)
    /// on the same context. `None` off Vulkan.
    pub fn from_frame(
        gpu: &daedalus_gpu::GpuContextHandle,
        frame: &'a daedalus_gpu::GpuFrameInput,
    ) -> Option<Self> {
        // SAFETY: the handles are only read; the frame (and its storage) outlives `'a`, and
        // the producer cannot reuse the dma-buf while the frame input lives.
        let (raw, buf) = unsafe { (gpu.raw_vulkan()?, frame.storage.raw_vk_buffer()?) };
        let offset = frame.planes().first().map_or(0, |p| p.0);
        Some(Self {
            device: raw.device.handle(),
            buffer: buf.buffer,
            offset: (buf.offset + offset) as usize,
            len: buf.size.saturating_sub(offset) as usize,
            _owner: PhantomData,
        })
    }
}
