//! Buffers: device-local, mapped for the CPU, imported from a dma-buf, exportable as one.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;

use crate::context::Inner;
use crate::error::{GpuError, VkContext};

/// Where a buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// Device-local, not mapped (separate-memory devices only).
    Device,
    /// Mapped; the CPU writes it (write-combined is fine).
    Upload,
    /// Mapped; the CPU reads it (cached when the device offers it).
    Readback,
}

pub(crate) struct Buffer {
    ctx: Arc<Inner>,
    pub buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pub size: u64,
    ptr: *mut u8,
    coherent: bool,
}

// SAFETY: the mapping is owned by the buffer; access goes through `&self`/`&mut self`.
unsafe impl Send for Buffer {}

impl Drop for Buffer {
    fn drop(&mut self) {
        let d = &self.ctx.device;
        // SAFETY: the owner waits for the GPU before dropping buffers.
        unsafe {
            d.destroy_buffer(self.buffer, None);
            d.free_memory(self.memory, None);
        }
    }
}

const USAGE: vk::BufferUsageFlags = vk::BufferUsageFlags::from_raw(
    vk::BufferUsageFlags::STORAGE_BUFFER.as_raw()
        | vk::BufferUsageFlags::TRANSFER_SRC.as_raw()
        | vk::BufferUsageFlags::TRANSFER_DST.as_raw(),
);

fn memory_type(
    ctx: &Inner,
    bits: u32,
    want: vk::MemoryPropertyFlags,
    prefer: vk::MemoryPropertyFlags,
    avoid: vk::MemoryPropertyFlags,
) -> Option<(u32, vk::MemoryPropertyFlags)> {
    let types = &ctx.memory.memory_types[..ctx.memory.memory_type_count as usize];
    let ok =
        |(i, t): &(usize, &vk::MemoryType)| bits & (1 << i) != 0 && t.property_flags.contains(want);
    let score = |t: &vk::MemoryType| {
        u32::from(t.property_flags.contains(prefer)) * 2
            + u32::from(!t.property_flags.intersects(avoid))
    };
    types
        .iter()
        .enumerate()
        .filter(ok)
        .max_by_key(|(i, t)| (score(t), u32::MAX - *i as u32))
        .map(|(i, t)| (i as u32, t.property_flags))
}

impl Buffer {
    /// A buffer of `size` bytes in `place`; with `export`, its memory can be exported as a
    /// dma-buf ([`Self::export_dmabuf`]).
    pub fn new(ctx: &Arc<Inner>, size: u64, place: Place, export: bool) -> Result<Self, GpuError> {
        let d = &ctx.device;
        let size = size.max(4);
        let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if export {
            info = info.push_next(&mut ext);
        }
        // SAFETY: valid create info.
        let buffer = unsafe { d.create_buffer(&info, None) }.ctx("vkCreateBuffer")?;
        // SAFETY: a buffer of this device.
        let req = unsafe { d.get_buffer_memory_requirements(buffer) };
        use vk::MemoryPropertyFlags as F;
        let (want, prefer, avoid) = match place {
            Place::Device => (F::DEVICE_LOCAL, F::empty(), F::HOST_VISIBLE),
            Place::Upload => (F::HOST_VISIBLE | F::HOST_COHERENT, F::empty(), F::empty()),
            Place::Readback => (F::HOST_VISIBLE, F::HOST_CACHED, F::empty()),
        };
        let found = memory_type(ctx, req.memory_type_bits, want, prefer, avoid)
            .or_else(|| memory_type(ctx, req.memory_type_bits, F::HOST_VISIBLE, prefer, avoid));
        let Some((index, flags)) = found else {
            // SAFETY: not in use.
            unsafe { d.destroy_buffer(buffer, None) };
            return Err(GpuError::Unsupported(format!(
                "no memory type for {place:?}"
            )));
        };
        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let mut alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        if export {
            alloc = alloc.push_next(&mut export_info).push_next(&mut dedicated);
        }
        Self::bind(ctx, buffer, &alloc, size, flags)
    }

    fn bind(
        ctx: &Arc<Inner>,
        buffer: vk::Buffer,
        alloc: &vk::MemoryAllocateInfo,
        size: u64,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<Self, GpuError> {
        let d = &ctx.device;
        // SAFETY: valid allocation info; on failure the buffer is destroyed.
        let memory = match unsafe { d.allocate_memory(alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: not in use.
                unsafe { d.destroy_buffer(buffer, None) };
                return Err(GpuError::Vulkan {
                    call: "vkAllocateMemory",
                    result: e,
                });
            }
        };
        let mut b = Self {
            ctx: Arc::clone(ctx),
            buffer,
            memory,
            size,
            ptr: std::ptr::null_mut(),
            coherent: flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT),
        };
        // SAFETY: memory and buffer of this device; `b` frees both on error.
        unsafe { d.bind_buffer_memory(buffer, memory, 0) }.ctx("vkBindBufferMemory")?;
        if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            // SAFETY: host-visible memory, mapped once for the buffer's life.
            let p = unsafe { d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                .ctx("vkMapMemory")?;
            b.ptr = p.cast();
        }
        Ok(b)
    }

    /// A buffer over the dma-buf `fd` (`size` bytes; the file stays the caller's).
    pub fn import_dmabuf(
        ctx: &Arc<Inner>,
        fd: BorrowedFd<'_>,
        size: u64,
    ) -> Result<Self, GpuError> {
        let Some(ext) = &ctx.external_fd else {
            return Err(GpuError::Unsupported(
                "dma-buf import (VK_EXT_external_memory_dma_buf)".into(),
            ));
        };
        let d = &ctx.device;
        let handle = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        let mut props = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: a valid fd; the call only reads it.
        unsafe { ext.get_memory_fd_properties(handle, fd.as_raw_fd(), &mut props) }
            .ctx("vkGetMemoryFdPropertiesKHR")?;
        let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle);
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info);
        // SAFETY: valid create info.
        let buffer = unsafe { d.create_buffer(&info, None) }.ctx("vkCreateBuffer")?;
        // SAFETY: a buffer of this device.
        let req = unsafe { d.get_buffer_memory_requirements(buffer) };
        let bits = req.memory_type_bits & props.memory_type_bits;
        let found = memory_type(
            ctx,
            bits,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::empty(),
        );
        let Some((index, _)) = found else {
            // SAFETY: not in use.
            unsafe { d.destroy_buffer(buffer, None) };
            return Err(GpuError::Unsupported(
                "no memory type for the dma-buf".into(),
            ));
        };
        // Vulkan takes ownership of the fd it imports: give it a duplicate.
        let dup = match fd.try_clone_to_owned() {
            Ok(f) => f,
            Err(e) => {
                // SAFETY: not in use.
                unsafe { d.destroy_buffer(buffer, None) };
                return Err(GpuError::Unsupported(format!("dup: {e}")));
            }
        };
        let raw = dup.as_raw_fd();
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(handle)
            .fd(raw);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size.max(size))
            .memory_type_index(index)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        // Imported memory is not mapped: the CPU side of a dma-buf is the dma-buf's own mmap.
        match Self::bind(ctx, buffer, &alloc, size, vk::MemoryPropertyFlags::empty()) {
            Ok(b) => {
                // The import succeeded: the fd is Vulkan's now.
                std::mem::forget(dup);
                Ok(b)
            }
            Err(e) => Err(e),
        }
    }

    /// A new dma-buf file for this buffer's memory (made with `export`).
    pub fn export_dmabuf(&self) -> Result<OwnedFd, GpuError> {
        let Some(ext) = &self.ctx.external_fd else {
            return Err(GpuError::Unsupported("dma-buf export".into()));
        };
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(self.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: memory allocated exportable; the new fd is ours.
        let fd = unsafe { ext.get_memory_fd(&info) }.ctx("vkGetMemoryFdKHR")?;
        // SAFETY: a fresh fd owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn mapped(&self) -> bool {
        !self.ptr.is_null()
    }

    /// Copy `data` to byte `offset` (a mapped buffer).
    pub fn write(&mut self, offset: usize, data: &[u8]) {
        assert!(self.mapped() && offset + data.len() <= self.size as usize);
        // SAFETY: inside the mapping; the GPU is not using the buffer (the owner waits).
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data.len()) };
    }

    /// Copy `words` (little-endian, as the device reads them) to the start (a mapped buffer).
    pub fn write_words(&mut self, words: &[u32]) {
        assert!(self.mapped() && 4 * words.len() <= self.size as usize);
        let dst = self.ptr.cast::<u32>();
        if cfg!(target_endian = "little") && dst.is_aligned() {
            // SAFETY: inside the mapping (aligned: mappings are page-aligned); the GPU is not
            // using the buffer.
            unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), dst, words.len()) };
        } else {
            for (i, w) in words.iter().enumerate() {
                self.write(4 * i, &w.to_le_bytes());
            }
        }
    }

    /// Make CPU writes visible to the device (non-coherent memory).
    pub fn flush(&self) -> Result<(), GpuError> {
        if self.coherent || !self.mapped() {
            return Ok(());
        }
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .size(vk::WHOLE_SIZE);
        // SAFETY: a mapped range of this buffer's memory.
        unsafe { self.ctx.device.flush_mapped_memory_ranges(&[range]) }
            .ctx("vkFlushMappedMemoryRanges")
    }

    /// Make device writes visible to the CPU (non-coherent memory).
    pub fn invalidate(&self) -> Result<(), GpuError> {
        if self.coherent || !self.mapped() {
            return Ok(());
        }
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .size(vk::WHOLE_SIZE);
        // SAFETY: as above.
        unsafe { self.ctx.device.invalidate_mapped_memory_ranges(&[range]) }
            .ctx("vkInvalidateMappedMemoryRanges")
    }

    /// The mapped contents.
    pub fn bytes(&self) -> &[u8] {
        assert!(self.mapped());
        // SAFETY: the whole mapping; the GPU is idle on it (the owner waits on its fence).
        unsafe { std::slice::from_raw_parts(self.ptr, self.size as usize) }
    }
}
