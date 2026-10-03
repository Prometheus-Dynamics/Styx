//! Asynchronous transfers: a ring of URBs (`USBDEVFS_SUBMITURB` / `USBDEVFS_REAPURBNDELAY` /
//! `USBDEVFS_DISCARDURB`) with their buffers.

use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::UsbDevice;
use crate::Mapping;
use crate::ioctl::{self, io, ior, iow};
use crate::{Error, Result};

/// The most packets one isochronous URB may carry (usbfs's limit).
pub const MAX_ISO_PACKETS: usize = 128;

const URB_TYPE_ISO: u8 = 0;
const URB_TYPE_BULK: u8 = 3;
const URB_ISO_ASAP: u32 = 0x02;

/// `struct usbdevfs_urb` (without its flexible packet array).
#[repr(C)]
struct RawUrb {
    kind: u8,
    endpoint: u8,
    status: libc::c_int,
    flags: libc::c_uint,
    buffer: *mut libc::c_void,
    buffer_length: libc::c_int,
    actual_length: libc::c_int,
    start_frame: libc::c_int,
    number_of_packets: libc::c_int,
    error_count: libc::c_int,
    signr: libc::c_uint,
    usercontext: *mut libc::c_void,
}

/// `struct usbdevfs_iso_packet_desc`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RawIsoDesc {
    length: libc::c_uint,
    actual_length: libc::c_uint,
    status: libc::c_uint,
}

/// A URB followed by room for its packet descriptors, as the kernel reads and writes it.
#[repr(C)]
struct UrbStorage {
    urb: RawUrb,
    iso: [RawIsoDesc; MAX_ISO_PACKETS],
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<RawUrb>() == 56);
    assert!(std::mem::offset_of!(UrbStorage, iso) == 56);
    assert!(size_of::<RawIsoDesc>() == 12);
};

ioctl::ioctls! {
    USBDEVFS_SUBMITURB = ior::<RawUrb>(b'U', 10);
    USBDEVFS_DISCARDURB = io(b'U', 11);
    USBDEVFS_REAPURBNDELAY = iow::<*mut libc::c_void>(b'U', 13);
}

/// What one URB moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    /// Isochronous: `packets` (1..=128) packets of up to `packet_size` bytes each, one per
    /// (micro)frame of the endpoint's interval.
    Iso { packets: usize, packet_size: usize },
    /// Bulk: one transfer of up to `size` bytes (a short packet ends it early).
    Bulk { size: usize },
}

/// A ring of URBs on one IN endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrbRingConfig {
    /// The endpoint address (bit 7 set: IN).
    pub endpoint: u8,
    pub kind: TransferKind,
    /// URBs kept in flight.
    pub count: usize,
    /// Put the buffers in memory mapped from usbfs (`mmap` on the device, Linux 4.6+): the
    /// host controller writes them directly and reaping copies nothing (the kernel otherwise
    /// allocates, zeroes and copies a buffer per URB). Where DMA is not cache-coherent the
    /// mapping is uncached, so reading it costs more; falls back to ordinary buffers when the
    /// mapping fails.
    pub mmap: bool,
}

/// How a URB ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrbStatus {
    Ok,
    /// Discarded before it completed.
    Cancelled,
    /// The device went away (`ESHUTDOWN`, `ENODEV`).
    Disconnected,
    /// Another error (`EPROTO`, `EOVERFLOW`, `EXDEV` for an isochronous URB with failed
    /// packets, ...): the errno.
    Error(i32),
}

impl UrbStatus {
    fn from_raw(status: i32) -> UrbStatus {
        match -status {
            0 => UrbStatus::Ok,
            libc::ENOENT | libc::ECONNRESET => UrbStatus::Cancelled,
            libc::ESHUTDOWN | libc::ENODEV => UrbStatus::Disconnected,
            e => UrbStatus::Error(e),
        }
    }
}

/// One isochronous packet of a completed URB.
#[derive(Clone, Copy, Debug)]
pub struct IsoPacket<'a> {
    /// 0, or the (positive) errno the host controller reported for this packet.
    pub status: i32,
    /// The bytes received.
    pub data: &'a [u8],
}

/// A completed URB, borrowed from its ring until the next ring call.
#[derive(Debug)]
pub struct Completion<'a> {
    /// Which URB of the ring (resubmit it with [`UrbRing::submit`]).
    pub slot: usize,
    pub status: UrbStatus,
    /// Bytes received (isochronous: all packets together).
    pub actual_length: usize,
    /// Isochronous packets that failed.
    pub error_count: u32,
    /// When it was reaped.
    pub reaped: Instant,
    kind: TransferKind,
    buffer: &'a [u8],
    iso: &'a [RawIsoDesc],
}

impl<'a> Completion<'a> {
    /// A bulk URB's bytes.
    pub fn data(&self) -> &'a [u8] {
        &self.buffer[..self.actual_length.min(self.buffer.len())]
    }

    /// An isochronous URB's packets, in bus order (empty for bulk).
    pub fn packets(&self) -> impl Iterator<Item = IsoPacket<'a>> + 'a {
        let size = match self.kind {
            TransferKind::Iso { packet_size, .. } => packet_size,
            TransferKind::Bulk { .. } => 0,
        };
        let buffer = self.buffer;
        self.iso.iter().enumerate().map(move |(i, d)| {
            let start = (i * size).min(buffer.len());
            let end = (start + d.actual_length as usize).min(buffer.len());
            IsoPacket {
                status: -(d.status as i32),
                data: &buffer[start..end],
            }
        })
    }

    /// Packets in this URB (isochronous; 0 for bulk).
    pub fn packet_count(&self) -> usize {
        self.iso.len()
    }
}

struct Slot {
    storage: NonNull<UrbStorage>,
    buffer: NonNull<[u8]>,
    in_flight: bool,
}

/// URBs and their buffers on one endpoint of a [`UsbDevice`]. At most one ring exists per
/// device descriptor (reaping returns whichever transfer completed).
///
/// Memory the kernel may still write (a submitted URB and its buffer) is only freed once that
/// URB was reaped; [`UrbRing::cancel`] (also run on drop) discards and reaps everything, and
/// leaks what the kernel does not give back within a few seconds rather than free it.
pub struct UrbRing {
    dev: Arc<UsbDevice>,
    config: UrbRingConfig,
    slots: Vec<Slot>,
    /// The usbfs mapping the buffers live in (`config.mmap`).
    mapping: Option<Mapping>,
}

// SAFETY: the raw pointers are owned allocations only this ring touches (the kernel writes
// them only during the ring's own reap calls); nothing is shared between threads.
unsafe impl Send for UrbRing {}

impl UrbRing {
    /// Allocates `config.count` URBs for `config.endpoint` (nothing is submitted yet).
    pub fn new(dev: Arc<UsbDevice>, config: UrbRingConfig) -> Result<UrbRing> {
        let (kind, len, packets) = match config.kind {
            TransferKind::Iso {
                packets,
                packet_size,
            } => {
                if !(1..=MAX_ISO_PACKETS).contains(&packets) || packet_size == 0 {
                    return Err(Error::Invalid(format!(
                        "isochronous URB of {packets} packets of {packet_size} bytes"
                    )));
                }
                (URB_TYPE_ISO, packets * packet_size, packets)
            }
            TransferKind::Bulk { size } => (URB_TYPE_BULK, size, 0),
        };
        if config.count == 0 || len == 0 || len > i32::MAX as usize {
            return Err(Error::Invalid("empty URB ring".into()));
        }
        dev.take_ring()?;
        // Kernels before 4.6, or usbfs memory exhausted: ordinary buffers.
        let mut mapping = config
            .mmap
            .then(|| {
                Mapping::new(
                    dev.as_fd(),
                    (len * config.count).div_ceil(4096) * 4096,
                    0,
                    true,
                )
            })
            .and_then(|m| m.ok());
        let mut slots = Vec::with_capacity(config.count);
        for i in 0..config.count {
            let buffer = match mapping.as_mut() {
                Some(m) => NonNull::from(&mut m.as_mut_slice()[i * len..(i + 1) * len]),
                None => NonNull::from(Box::leak(vec![0u8; len].into_boxed_slice())),
            };
            let mut storage = Box::new(UrbStorage {
                urb: RawUrb {
                    kind,
                    endpoint: config.endpoint,
                    status: 0,
                    flags: if kind == URB_TYPE_ISO {
                        URB_ISO_ASAP
                    } else {
                        0
                    },
                    buffer: buffer.as_ptr().cast(),
                    buffer_length: len as libc::c_int,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets: packets as libc::c_int,
                    error_count: 0,
                    signr: 0,
                    usercontext: std::ptr::null_mut(),
                },
                iso: [RawIsoDesc::default(); MAX_ISO_PACKETS],
            });
            if let TransferKind::Iso { packet_size, .. } = config.kind {
                for d in &mut storage.iso[..packets] {
                    d.length = packet_size as libc::c_uint;
                }
            }
            slots.push(Slot {
                storage: NonNull::from(Box::leak(storage)),
                buffer,
                in_flight: false,
            });
        }
        Ok(UrbRing {
            dev,
            config,
            slots,
            mapping,
        })
    }

    /// The ring's configuration.
    pub fn config(&self) -> UrbRingConfig {
        self.config
    }

    /// Whether the buffers are mapped from usbfs (no copy at reap).
    pub fn is_mapped(&self) -> bool {
        self.mapping.is_some()
    }

    /// The device.
    pub fn device(&self) -> &Arc<UsbDevice> {
        &self.dev
    }

    /// URBs submitted and not yet reaped.
    pub fn in_flight(&self) -> usize {
        self.slots.iter().filter(|s| s.in_flight).count()
    }

    /// Submits URB `slot` (idle) again.
    pub fn submit(&mut self, slot: usize) -> Result<()> {
        let s = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| Error::Invalid(format!("no URB {slot}")))?;
        if s.in_flight {
            return Ok(());
        }
        let urb = s.storage.as_ptr();
        // SAFETY: `urb` is this slot's live allocation; the URB is not in flight, so the
        // kernel holds no reference to it and these writes race with nothing.
        unsafe {
            (*urb).urb.status = 0;
            (*urb).urb.actual_length = 0;
            (*urb).urb.error_count = 0;
            (*urb).urb.start_frame = 0;
            (*urb).urb.usercontext = urb.cast();
        }
        // SAFETY: `urb` points to a `usbdevfs_urb` followed by `number_of_packets` packet
        // descriptors, and its `buffer` to `buffer_length` bytes, all owned by this ring and
        // kept alive (not moved, not accessed through references) until the URB is reaped.
        unsafe { ioctl::ioctl_raw(self.dev.as_fd(), USBDEVFS_SUBMITURB, urb.cast())? };
        s.in_flight = true;
        Ok(())
    }

    /// Submits every idle URB.
    pub fn submit_all(&mut self) -> Result<()> {
        for i in 0..self.slots.len() {
            self.submit(i)?;
        }
        Ok(())
    }

    /// Takes one completed URB if there is one (never blocks; wait on the device descriptor
    /// for `POLLOUT`, [`UsbDevice::wait`]). The URB stays idle until [`UrbRing::submit`].
    /// `Err` with `ENODEV` when the device went away and nothing completed is left.
    pub fn reap(&mut self) -> Result<Option<Completion<'_>>> {
        let mut ptr: *mut libc::c_void = std::ptr::null_mut();
        // SAFETY: the kernel writes the completed URB's data, status and packet lengths into
        // the memory given at submit (owned by this ring, alive) and its address into `ptr`.
        let r = unsafe {
            ioctl::ioctl_raw(
                self.dev.as_fd(),
                USBDEVFS_REAPURBNDELAY,
                (&mut ptr as *mut *mut libc::c_void).cast(),
            )
        };
        match r {
            Ok(_) => {}
            Err(e) if e.is_would_block() => return Ok(None),
            Err(e) => return Err(e),
        }
        let reaped = Instant::now();
        let slot = self
            .slots
            .iter()
            .position(|s| s.storage.as_ptr().cast() == ptr)
            .ok_or_else(|| Error::Invalid("reaped a URB this ring did not submit".into()))?;
        let s = &mut self.slots[slot];
        s.in_flight = false;
        let storage = s.storage.as_ptr();
        // SAFETY: the URB was reaped: the kernel no longer references the slot's memory, so
        // shared borrows of it are sound until the ring is used mutably again (the borrow of
        // `self` the completion holds).
        let (urb, iso, buffer) = unsafe {
            let urb = &(*storage).urb;
            let n = (urb.number_of_packets.max(0) as usize).min(MAX_ISO_PACKETS);
            let iso = if urb.kind == URB_TYPE_ISO {
                &(&(*storage).iso)[..n]
            } else {
                &[][..]
            };
            (urb, iso, &*s.buffer.as_ptr())
        };
        Ok(Some(Completion {
            slot,
            status: UrbStatus::from_raw(urb.status),
            actual_length: urb.actual_length.max(0) as usize,
            error_count: urb.error_count.max(0) as u32,
            reaped,
            kind: self.config.kind,
            buffer,
            iso,
        }))
    }

    /// Discards every URB in flight and reaps them all, waiting up to `timeout`. Returns how
    /// many could not be reaped (their memory is then leaked when the ring drops).
    pub fn cancel(&mut self, timeout: Duration) -> usize {
        for s in self.slots.iter().filter(|s| s.in_flight) {
            // SAFETY: DISCARDURB takes the submitted URB's address as its argument and only
            // looks it up among the descriptor's pending URBs.
            let _ = unsafe {
                ioctl::ioctl_raw(
                    self.dev.as_fd(),
                    USBDEVFS_DISCARDURB,
                    s.storage.as_ptr().cast(),
                )
            };
        }
        let deadline = Instant::now() + timeout;
        while self.in_flight() > 0 {
            match self.reap() {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                // Gone: the kernel has nothing left to give back.
                Err(_) => break,
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let _ = self
                .dev
                .wait(Some((deadline - now).min(Duration::from_millis(50))));
        }
        self.in_flight()
    }
}

impl Drop for UrbRing {
    fn drop(&mut self) {
        self.cancel(Duration::from_secs(3));
        let mapped = self.mapping.is_some();
        let mut leaked = false;
        for s in &self.slots {
            if s.in_flight {
                // The kernel may still write these at a reap: never free them.
                leaked = true;
                continue;
            }
            // SAFETY: the storage (and, without a mapping, the buffer) was leaked from a box in
            // `new`, and this URB is not in flight, so nothing else references them.
            unsafe {
                drop(Box::from_raw(s.storage.as_ptr()));
                if !mapped {
                    drop(Box::from_raw(s.buffer.as_ptr()));
                }
            }
        }
        if leaked && let Some(m) = self.mapping.take() {
            std::mem::forget(m);
        }
        self.dev.give_back_ring();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(USBDEVFS_SUBMITURB.nr, 0x8038_550a);
        assert_eq!(USBDEVFS_DISCARDURB.nr, 0x550b);
        assert_eq!(USBDEVFS_REAPURBNDELAY.nr, 0x4008_550d);
    }

    #[test]
    fn statuses() {
        assert_eq!(UrbStatus::from_raw(0), UrbStatus::Ok);
        assert_eq!(UrbStatus::from_raw(-libc::ENOENT), UrbStatus::Cancelled);
        assert_eq!(
            UrbStatus::from_raw(-libc::ESHUTDOWN),
            UrbStatus::Disconnected
        );
        assert_eq!(
            UrbStatus::from_raw(-libc::EPROTO),
            UrbStatus::Error(libc::EPROTO)
        );
    }

    #[test]
    fn packets_are_laid_out_at_fixed_strides() {
        let buffer = [1u8, 2, 3, 0, 4, 5, 0, 0, 6, 0, 0, 0];
        let iso = [
            RawIsoDesc {
                length: 4,
                actual_length: 3,
                status: 0,
            },
            RawIsoDesc {
                length: 4,
                actual_length: 2,
                status: (-libc::EPROTO) as u32,
            },
            RawIsoDesc {
                length: 4,
                actual_length: 1,
                status: 0,
            },
        ];
        let c = Completion {
            slot: 0,
            status: UrbStatus::Ok,
            actual_length: 6,
            error_count: 1,
            reaped: Instant::now(),
            kind: TransferKind::Iso {
                packets: 3,
                packet_size: 4,
            },
            buffer: &buffer,
            iso: &iso,
        };
        let p: Vec<_> = c.packets().collect();
        assert_eq!(p[0].data, &[1, 2, 3]);
        assert_eq!((p[1].data, p[1].status), (&[4u8, 5][..], libc::EPROTO));
        assert_eq!(p[2].data, &[6]);
    }
}
