//! A running stream: URBs on the video endpoint, frames assembled from their payloads, and
//! timestamps from the bus and the device clock. Driven by its owner (no thread): reap,
//! assemble, resubmit, whenever the usbfs descriptor reports completed transfers.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use styx_graph::rt::{AsyncFd, Ready};
use styx_kernel::usbfs::{Speed, TransferKind, UrbRing, UrbRingConfig, UrbStatus, UsbDevice};

use crate::clock::{BusClock, DeviceClock};
use crate::descriptors::TransferType;
use crate::device::{UvcDevice, frame_capacity};
use crate::payload::{AssembledFrame, Assembler, FrameFlags, Payload, PayloadHeader, Scr};
use crate::pool::{BufferPool, PooledBuffer};
use crate::probe::StreamingParams;
use crate::{Result, UvcError};

/// Total URB memory kept under usbfs's default limit (16 MiB per system).
const URB_MEMORY: usize = 12 << 20;

/// What to stream and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub interface: u8,
    pub format_index: u8,
    pub frame_index: u8,
    /// Frame interval, 100 ns units.
    pub interval: u32,
    /// URBs in flight.
    pub urbs: usize,
    /// Isochronous packets per URB (1..=128): 32 = 4 ms at high speed.
    pub packets_per_urb: usize,
    /// Frame buffers kept for reuse.
    pub buffers: usize,
    /// Deliver damaged frames (flagged) instead of dropping them.
    pub deliver_damaged: bool,
    /// URB buffers mapped from usbfs: no per-URB kernel allocation, zeroing and copy, but
    /// uncached memory where DMA is not cache-coherent (`UrbRingConfig::mmap`).
    pub mapped_urbs: bool,
}

impl StreamConfig {
    /// Defaults: 5 URBs of 32 packets (as `uvcvideo`), 4 buffers, damaged frames dropped.
    pub fn new(interface: u8, format_index: u8, frame_index: u8, interval: u32) -> StreamConfig {
        StreamConfig {
            interface,
            format_index,
            frame_index,
            interval,
            urbs: 5,
            packets_per_urb: 32,
            buffers: 4,
            deliver_damaged: false,
            mapped_urbs: false,
        }
    }
}

/// One frame.
#[derive(Debug)]
pub struct UvcFrame {
    /// The frame's bytes, assembled in place (no further copy on the way out).
    pub data: PooledBuffer,
    /// Frames assembled since the start, damaged ones included (gaps: frames dropped).
    pub sequence: u32,
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// `CLOCK_MONOTONIC`: the capture time from PTS when the camera sends PTS and SCR,
    /// otherwise when its first payload arrived.
    pub timestamp: Duration,
    /// Whether `timestamp` is the camera's capture time (PTS through SCR).
    pub timestamp_from_pts: bool,
    /// When its first and last payloads arrived (bus time, `CLOCK_MONOTONIC`).
    pub first_payload: Duration,
    pub last_payload: Duration,
    /// Raw PTS and the frame's last SCR (device clock).
    pub pts: Option<u32>,
    pub scr: Option<Scr>,
    pub flags: FrameFlags,
    /// When the stream handed it out.
    pub dequeued: Instant,
}

/// Counters of a stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub frames: u64,
    /// Damaged frames dropped.
    pub dropped: u64,
    pub damaged: u64,
    pub urbs: u64,
    pub failed_packets: u64,
    pub invalid_headers: u64,
}

/// The negotiated stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    pub params: StreamingParams,
    /// Alternate setting (0 for bulk) and its bytes per (micro)frame.
    pub alt: u8,
    pub bytes_per_interval: usize,
    pub transfer: TransferType,
}

/// A running stream. Dropping it stops the camera (alternate setting 0) and frees the bus
/// bandwidth.
pub struct UvcStream {
    dev: UvcDevice,
    config: StreamConfig,
    format: StreamFormat,
    ring: Option<UrbRing>,
    afd: Option<AsyncFd<Arc<UsbDevice>>>,
    asm: Assembler,
    bus: BusClock,
    clock: DeviceClock,
    ready: VecDeque<UvcFrame>,
    scratch: Vec<AssembledFrame>,
    packets: u64,
    sequence: u32,
    stats: StreamStats,
    started: Instant,
    ended: bool,
}

impl std::fmt::Debug for UvcStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UvcStream")
            .field("format", &self.format)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn mono_ns() -> u64 {
    styx_kernel::monotonic_now().as_nanos() as u64
}

impl UvcStream {
    pub(crate) fn start(dev: UvcDevice, config: StreamConfig) -> Result<UvcStream> {
        let vs = dev.streaming(Some(config.interface))?.clone();
        let format = vs
            .format(config.format_index)
            .ok_or_else(|| UvcError::Invalid(format!("no format {}", config.format_index)))?
            .clone();
        let frame = format
            .frame(config.frame_index)
            .ok_or_else(|| UvcError::Invalid(format!("no frame {}", config.frame_index)))?
            .clone();
        let fourcc = format
            .fourcc()
            .ok_or_else(|| UvcError::Unsupported("format without a FourCC".into()))?;
        // Streaming off while negotiating (a previous owner may have left it on).
        let transfer = vs.transfer();
        if transfer == TransferType::Isochronous {
            dev.usb()
                .set_interface(vs.number, 0)
                .map_err(UvcError::from_kernel)?;
        }
        let params = dev.negotiate(
            vs.number,
            config.format_index,
            config.frame_index,
            config.interval,
        )?;
        let capacity = frame_capacity(&format, &frame, &params);
        let info = dev.info();
        let speed = info.speed;
        let usb = dev.usb().clone();
        let (alt, bytes_per_interval, ring_config, interval_ns) = match transfer {
            TransferType::Isochronous => {
                let payload = params.max_payload_transfer_size as usize;
                let (alt, ep) = vs.alt_for_bandwidth(payload).ok_or_else(|| {
                    UvcError::Unsupported("no isochronous alternate setting".into())
                })?;
                let bpi = ep.bytes_per_interval();
                let packets = config.packets_per_urb.clamp(1, 128);
                let count = config.urbs.clamp(2, (URB_MEMORY / (packets * bpi)).max(2));
                let exp = u32::from(ep.interval.clamp(1, 16)) - 1;
                let interval = speed.bus_interval() * (1u32 << exp);
                (
                    alt,
                    bpi,
                    UrbRingConfig {
                        endpoint: ep.address,
                        kind: TransferKind::Iso {
                            packets,
                            packet_size: bpi,
                        },
                        count,
                        mmap: config.mapped_urbs,
                    },
                    interval.as_nanos() as u64,
                )
            }
            _ => {
                let size = (params.max_payload_transfer_size as usize)
                    .max(capacity.min(16 << 10))
                    .max(512);
                if size * 2 > URB_MEMORY {
                    return Err(UvcError::Unsupported(format!(
                        "bulk payloads of {size} bytes"
                    )));
                }
                let count = config.urbs.clamp(2, URB_MEMORY / size);
                (
                    0,
                    size,
                    UrbRingConfig {
                        endpoint: vs.endpoint,
                        kind: TransferKind::Bulk { size },
                        count,
                        mmap: config.mapped_urbs,
                    },
                    speed.bus_interval().as_nanos() as u64,
                )
            }
        };
        if transfer == TransferType::Isochronous {
            usb.set_interface(vs.number, alt)
                .map_err(UvcError::from_kernel)?;
        }
        let mut ring = UrbRing::new(usb.clone(), ring_config).map_err(UvcError::from_kernel)?;
        let started = Instant::now();
        if let Err(e) = ring.submit_all() {
            drop(ring);
            if transfer == TransferType::Isochronous {
                let _ = usb.set_interface(vs.number, 0);
            }
            return Err(UvcError::from_kernel(e));
        }
        let clock_hz = if params.clock_frequency != 0 {
            params.clock_frequency
        } else {
            dev.function().control.clock_frequency
        };
        let per_sof = if matches!(speed, Speed::Low | Speed::Full) {
            1
        } else {
            8
        };
        let expected = format.frame_bytes(&frame);
        tracing::debug!(
            fourcc = %String::from_utf8_lossy(&fourcc),
            width = frame.width,
            height = frame.height,
            interval = params.frame_interval,
            payload = params.max_payload_transfer_size,
            alt,
            bytes_per_interval,
            "uvc: streaming"
        );
        Ok(UvcStream {
            format: StreamFormat {
                fourcc,
                width: frame.width.into(),
                height: frame.height.into(),
                params,
                alt,
                bytes_per_interval,
                transfer,
            },
            config,
            ring: Some(ring),
            afd: None,
            asm: Assembler::new(BufferPool::new(capacity, config.buffers.max(1)), expected),
            bus: BusClock::new(interval_ns),
            clock: DeviceClock::new(clock_hz, per_sof),
            ready: VecDeque::new(),
            scratch: Vec::new(),
            packets: 0,
            sequence: 0,
            stats: StreamStats::default(),
            started,
            ended: false,
            dev,
        })
    }

    /// What was negotiated.
    pub fn format(&self) -> &StreamFormat {
        &self.format
    }

    /// Counters so far.
    pub fn stats(&self) -> StreamStats {
        let a = self.asm.stats();
        StreamStats {
            failed_packets: a.failed_packets,
            invalid_headers: a.invalid_headers,
            damaged: a.damaged,
            ..self.stats
        }
    }

    /// When the stream started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// The camera.
    pub fn device(&self) -> &UvcDevice {
        &self.dev
    }

    /// Reaps every completed URB, assembles, resubmits. Never blocks.
    fn pump(&mut self) -> Result<()> {
        if self.ended {
            return Err(UvcError::Disconnected);
        }
        let Some(ring) = self.ring.as_mut() else {
            return Err(UvcError::Disconnected);
        };
        loop {
            let c = match ring.reap() {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    self.ended = true;
                    return Err(UvcError::from_kernel(e));
                }
            };
            self.stats.urbs += 1;
            let slot = c.slot;
            match c.status {
                UrbStatus::Disconnected => {
                    self.ended = true;
                    return Err(UvcError::Disconnected);
                }
                UrbStatus::Cancelled => continue,
                UrbStatus::Ok | UrbStatus::Error(_) => {}
            }
            let reaped_ns = mono_ns().saturating_sub(c.reaped.elapsed().as_nanos() as u64);
            if self.format.transfer == TransferType::Isochronous {
                let first = self.bus.urb(reaped_ns, c.packet_count() as u64);
                for (k, pkt) in c.packets().enumerate() {
                    let p = first + k as u64;
                    if pkt.status == 0
                        && let Some(scr) = PayloadHeader::parse(pkt.data).and_then(|h| h.scr)
                    {
                        self.clock.scr(scr, p, &self.bus);
                    }
                    self.asm.push(
                        Payload {
                            data: pkt.data,
                            failed: pkt.status != 0,
                            packet: p,
                            time_ns: self.bus.packet_time(p),
                        },
                        &mut self.scratch,
                    );
                }
                self.packets = first + c.packet_count() as u64;
            } else {
                let failed = !matches!(c.status, UrbStatus::Ok);
                let stalled = c.status == UrbStatus::Error(libc::EPIPE);
                self.asm.push(
                    Payload {
                        data: c.data(),
                        failed,
                        packet: self.packets,
                        time_ns: reaped_ns,
                    },
                    &mut self.scratch,
                );
                self.packets += 1;
                if stalled {
                    let _ = ring.device().clear_halt(ring.config().endpoint);
                }
            }
            if let Err(e) = ring.submit(slot) {
                self.ended = true;
                return Err(UvcError::from_kernel(e));
            }
        }
        let frames = std::mem::take(&mut self.scratch);
        for f in frames {
            self.deliver(f);
        }
        Ok(())
    }

    fn deliver(&mut self, f: AssembledFrame) {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        if f.flags.damaged() && !self.config.deliver_damaged {
            self.stats.dropped += 1;
            return;
        }
        let pts_time = f.pts.and_then(|p| self.clock.to_host(p));
        let first = Duration::from_nanos(f.first_time_ns);
        self.stats.frames += 1;
        self.ready.push_back(UvcFrame {
            sequence,
            fourcc: self.format.fourcc,
            width: self.format.width,
            height: self.format.height,
            timestamp: pts_time.map_or(first, Duration::from_nanos),
            timestamp_from_pts: pts_time.is_some(),
            first_payload: first,
            last_payload: Duration::from_nanos(f.last_time_ns),
            pts: f.pts,
            scr: f.scr_last,
            flags: f.flags,
            dequeued: Instant::now(),
            data: f.data,
        });
    }

    /// The next frame if one is ready (never blocks).
    pub fn try_next(&mut self) -> Result<Option<UvcFrame>> {
        if self.ready.is_empty() {
            self.pump()?;
        }
        Ok(self.ready.pop_front().map(|mut f| {
            f.dequeued = Instant::now();
            f
        }))
    }

    /// Waits up to `timeout` for the next frame ([`UvcError::Timeout`] if none came).
    pub fn next_blocking(&mut self, timeout: Duration) -> Result<UvcFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(f) = self.try_next()? {
                return Ok(f);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(UvcError::Timeout);
            }
            let ready = self
                .dev
                .usb()
                .wait(Some(deadline - now))
                .map_err(UvcError::from_kernel)?;
            if ready.hangup || ready.error {
                // Take what completed first; the reap then reports the disconnect.
                self.pump()?;
                if self.ready.is_empty() {
                    self.ended = true;
                    return Err(UvcError::Disconnected);
                }
            }
        }
    }

    /// The next frame, waiting on any executor (styx-graph's reactor wakes it when the
    /// usbfs descriptor reports completed transfers).
    pub async fn next(&mut self) -> Result<UvcFrame> {
        loop {
            if let Some(f) = self.try_next()? {
                return Ok(f);
            }
            if self.afd.is_none() {
                let afd = AsyncFd::new(self.dev.usb().clone())
                    .map_err(|e| UvcError::Invalid(format!("reactor: {e}")))?;
                self.afd = Some(afd);
            }
            let afd = self.afd.as_ref().expect("set above");
            let ready = afd
                .writable()
                .await
                .map_err(|e| UvcError::Invalid(format!("reactor: {e}")))?;
            if ready.contains(Ready::HANGUP) || ready.contains(Ready::ERROR) {
                self.pump()?;
                if self.ready.is_empty() {
                    self.ended = true;
                    return Err(UvcError::Disconnected);
                }
            }
        }
    }

    /// Stops streaming (also on drop).
    pub fn stop(&mut self) {
        self.afd = None;
        if let Some(ring) = self.ring.take() {
            drop(ring);
            if self.format.transfer == TransferType::Isochronous && !self.ended {
                let _ = self.dev.usb().set_interface(self.config.interface, 0);
            }
        }
        self.ended = true;
        self.dev.inner.streaming.store(false, Ordering::Release);
    }
}

impl AsFd for UvcStream {
    /// `POLLOUT` when transfers completed (call [`UvcStream::try_next`]), `POLLHUP` when the
    /// camera went away.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dev.usb().as_fd()
    }
}

impl Drop for UvcStream {
    fn drop(&mut self) {
        self.stop();
    }
}
