//! Memory a device reads or writes: frame buffers, statistics, ISP outputs.

use crate::error::ErrorKind;

/// How the CPU is about to use (or has used) a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The CPU reads what the device wrote.
    Read,
    /// The CPU writes what the device will read.
    Write,
    /// Both.
    ReadWrite,
}

/// Where a buffer must come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Whatever the device can reach.
    Any,
    /// Physically contiguous (CMA on Linux; any DMA-capable RAM on microcontrollers).
    Contiguous,
    /// On-chip SRAM: statistics, small frames.
    Fast,
    /// External SDRAM/PSRAM: full frames on microcontrollers.
    Large,
}

/// One buffer a device writes or reads.
pub trait DmaBuffer {
    /// A handle another process or device can import (Linux: the dma-buf descriptor).
    type Export<'a>: Copy
    where
        Self: 'a;

    /// Length in bytes.
    fn len(&self) -> usize;

    /// Whether the buffer is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The CPU view. Contents are defined only between [`Self::begin_cpu`] and
    /// [`Self::end_cpu`] (or always, on coherent memory).
    fn bytes(&self) -> &[u8];

    /// The CPU view, writable.
    fn bytes_mut(&mut self) -> &mut [u8];

    /// Cache maintenance before the CPU touches the buffer: `DMA_BUF_IOCTL_SYNC` start on
    /// Linux (cached dma-heap buffers), a D-cache invalidate by address on a Cortex-M7, nothing
    /// on coherent memory.
    fn begin_cpu(&self, access: Access);

    /// After the CPU is done: clean what it wrote. Implementations may skip it after reads
    /// (Linux: no `SYNC_END` after reads, 22 µs per 1.3 MB frame saved on the CM5).
    fn end_cpu(&self, access: Access);

    /// The address a DMA engine is programmed with (microcontrollers; `None` behind an IOMMU
    /// or a driver).
    fn device_address(&self) -> Option<u64> {
        None
    }

    /// A handle for other processes or devices, if the platform has one.
    fn export(&self) -> Option<Self::Export<'_>> {
        None
    }
}

/// Allocates [`DmaBuffer`]s.
pub trait DmaMemory {
    /// The buffers.
    type Buffer: DmaBuffer;
    /// The error.
    type Error: crate::HalError;
    /// A buffer of `len` bytes aligned to `align` (a power of two) from `region`.
    fn allocate(
        &mut self,
        len: usize,
        align: usize,
        region: Region,
    ) -> Result<Self::Buffer, Self::Error>;
}

/// Cache maintenance by address range, for [`StaticDma`] on cores with a data cache.
#[derive(Clone, Copy, Debug)]
pub struct CacheOps {
    /// Invalidate the lines covering `[address, address + len)` (before the CPU reads what a
    /// device wrote).
    pub invalidate: fn(address: usize, len: usize),
    /// Clean (write back) the lines covering the range (before a device reads what the CPU
    /// wrote).
    pub clean: fn(address: usize, len: usize),
}

impl CacheOps {
    /// Coherent memory (no data cache, or an uncached region): nothing to do.
    pub const COHERENT: CacheOps = CacheOps {
        invalidate: |_, _| {},
        clean: |_, _| {},
    };
}

/// [`DmaMemory`] carving buffers out of one `'static` region the linker script placed in
/// DMA-reachable RAM (AXI SRAM, SDRAM, PSRAM). Buffers are never freed (allocate at configure
/// time, as the runtime does).
#[derive(Debug)]
pub struct StaticDma {
    free: &'static mut [u8],
    region: Region,
    cache: CacheOps,
}

impl StaticDma {
    /// Buffers from `memory`, which serves `region` (requests for [`Region::Any`] or this
    /// region succeed, others fail with [`ErrorKind::Unsupported`]).
    pub fn new(memory: &'static mut [u8], region: Region, cache: CacheOps) -> Self {
        Self {
            free: memory,
            region,
            cache,
        }
    }

    /// Bytes left (before alignment padding).
    pub fn remaining(&self) -> usize {
        self.free.len()
    }
}

impl DmaMemory for StaticDma {
    type Buffer = StaticBuffer;
    type Error = ErrorKind;

    fn allocate(
        &mut self,
        len: usize,
        align: usize,
        region: Region,
    ) -> Result<StaticBuffer, ErrorKind> {
        if region != Region::Any && region != self.region {
            return Err(ErrorKind::Unsupported);
        }
        if !align.is_power_of_two() {
            return Err(ErrorKind::InvalidConfig);
        }
        let start = self.free.as_ptr() as usize;
        let pad = start.next_multiple_of(align) - start;
        if pad.checked_add(len).is_none_or(|n| n > self.free.len()) {
            return Err(ErrorKind::NoMemory);
        }
        let free = core::mem::take(&mut self.free);
        let (_, rest) = free.split_at_mut(pad);
        let (data, rest) = rest.split_at_mut(len);
        self.free = rest;
        Ok(StaticBuffer {
            data,
            cache: self.cache,
        })
    }
}

/// A buffer of [`StaticDma`].
#[derive(Debug)]
pub struct StaticBuffer {
    data: &'static mut [u8],
    cache: CacheOps,
}

impl DmaBuffer for StaticBuffer {
    type Export<'a> = ();

    fn len(&self) -> usize {
        self.data.len()
    }
    fn bytes(&self) -> &[u8] {
        self.data
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.data
    }
    fn begin_cpu(&self, access: Access) {
        if access != Access::Write {
            (self.cache.invalidate)(self.data.as_ptr() as usize, self.data.len());
        }
    }
    fn end_cpu(&self, access: Access) {
        if access != Access::Read {
            (self.cache.clean)(self.data.as_ptr() as usize, self.data.len());
        }
    }
    fn device_address(&self) -> Option<u64> {
        Some(self.data.as_ptr() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_dma_aligns_and_runs_out() {
        let memory: &'static mut [u8] = Box::leak(vec![0u8; 1000].into_boxed_slice());
        let mut dma = StaticDma::new(memory, Region::Fast, CacheOps::COHERENT);
        let a = dma.allocate(100, 32, Region::Any).unwrap();
        let b = dma.allocate(100, 32, Region::Fast).unwrap();
        assert_eq!(a.device_address().unwrap() % 32, 0);
        assert_eq!(b.device_address().unwrap() % 32, 0);
        assert!(b.device_address() >= a.device_address().map(|x| x + 100));
        assert_eq!(
            dma.allocate(10, 4, Region::Large).unwrap_err(),
            ErrorKind::Unsupported
        );
        assert_eq!(
            dma.allocate(2000, 4, Region::Any).unwrap_err(),
            ErrorKind::NoMemory
        );
        assert_eq!(
            dma.allocate(1, 3, Region::Any).unwrap_err(),
            ErrorKind::InvalidConfig
        );
    }
}
