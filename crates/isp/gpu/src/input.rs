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
    /// A buffer of the ISP's own device ([`ExternalBuffer::new`]): read in place (zero-copy).
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
