//! Payload headers (UVC 1.5 §2.4.3.3) and frame assembly from a stream of payloads.
//!
//! Every isochronous packet (or bulk payload) starts with a header: its length, the frame id
//! bit (FID, toggles each frame), end of frame (EOF), presentation time stamp (PTS, the device
//! clock when the sensor captured the frame), source clock reference (SCR: the device clock
//! and the USB frame number it was sampled at), still image and error bits.
//!
//! [`Assembler`] turns payloads into frames the way `uvcvideo` does, with its rules made
//! explicit: a frame ends at EOF or when FID toggles; the stream is joined at the first frame
//! boundary (a partial first frame is dropped, never delivered); after EOF the next frame must
//! toggle FID (payloads that do not are dropped as out of sync); lost or failed packets, the
//! ERR bit, a short uncompressed frame or an overflowing one mark the frame as damaged.

use crate::pool::{BufferPool, PooledBuffer};

const FID: u8 = 0x01;
const EOF: u8 = 0x02;
const PTS: u8 = 0x04;
const SCR: u8 = 0x08;
const STI: u8 = 0x20;
const ERR: u8 = 0x40;

/// A source clock reference: the device clock (`stc`, at `dwClockFrequency`) sampled at the
/// start of USB frame `sof` (11 bits, 1 ms frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scr {
    pub stc: u32,
    pub sof: u16,
}

/// A parsed payload header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadHeader {
    /// `bHeaderLength`: the payload data starts here.
    pub len: usize,
    pub fid: bool,
    pub eof: bool,
    pub pts: Option<u32>,
    pub scr: Option<Scr>,
    pub still: bool,
    pub error: bool,
}

impl PayloadHeader {
    /// Parses the header at the start of `payload`; `None` when it is not a valid header
    /// (too short, a length beyond the payload, or too short for the fields it announces).
    pub fn parse(payload: &[u8]) -> Option<PayloadHeader> {
        let len = usize::from(*payload.first()?);
        let flags = *payload.get(1)?;
        if len < 2 || len > payload.len() {
            return None;
        }
        let mut at = 2;
        let pts = if flags & PTS != 0 {
            let b = payload.get(at..at + 4).filter(|_| at + 4 <= len)?;
            at += 4;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        } else {
            None
        };
        let scr = if flags & SCR != 0 {
            let b = payload.get(at..at + 6).filter(|_| at + 6 <= len)?;
            Some(Scr {
                stc: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                sof: u16::from_le_bytes([b[4], b[5]]) & 0x7ff,
            })
        } else {
            None
        };
        Some(PayloadHeader {
            len,
            fid: flags & FID != 0,
            eof: flags & EOF != 0,
            pts,
            scr,
            still: flags & STI != 0,
            error: flags & ERR != 0,
        })
    }
}

/// One payload as it came off the bus.
#[derive(Clone, Copy, Debug)]
pub struct Payload<'a> {
    pub data: &'a [u8],
    /// The host controller reported an error for it, or it was lost.
    pub failed: bool,
    /// Its packet number in the stream (counted from 0 at stream start).
    pub packet: u64,
    /// When it arrived (host `CLOCK_MONOTONIC`, ns): the end of its (micro)frame.
    pub time_ns: u64,
}

/// Why a frame is damaged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameFlags {
    /// A packet was lost or failed, or a payload had the ERR bit.
    pub error: bool,
    /// An uncompressed frame shorter than its size.
    pub short: bool,
    /// More data than the frame's buffer holds (truncated).
    pub overflow: bool,
    /// Ended by a FID toggle instead of EOF.
    pub no_eof: bool,
    /// A still image (`STI`).
    pub still: bool,
}

impl FrameFlags {
    /// Whether the frame's data is incomplete or corrupted.
    pub fn damaged(&self) -> bool {
        self.error || self.short || self.overflow
    }
}

/// A frame put together from its payloads.
#[derive(Debug)]
pub struct AssembledFrame {
    pub data: PooledBuffer,
    pub fid: bool,
    /// The frame's presentation time stamp (device clock), if the payloads carry one.
    pub pts: Option<u32>,
    /// The first and the last SCR seen in the frame.
    pub scr_first: Option<Scr>,
    pub scr_last: Option<Scr>,
    pub flags: FrameFlags,
    /// Packet numbers of its first and last payloads.
    pub first_packet: u64,
    pub last_packet: u64,
    /// Arrival of its first and last payloads (host monotonic ns).
    pub first_time_ns: u64,
    pub last_time_ns: u64,
    pub payloads: u32,
}

/// Counters of an assembler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames: u64,
    pub damaged: u64,
    /// Payloads dropped before joining the stream or as out of sync.
    pub dropped_payloads: u64,
    /// Payloads without a valid header.
    pub invalid_headers: u64,
    pub failed_packets: u64,
}

struct Building {
    frame: AssembledFrame,
}

/// Assembles frames from payloads; see the module documentation for the rules.
pub struct Assembler {
    pool: BufferPool,
    /// Bytes of a complete frame (uncompressed formats).
    expected: Option<usize>,
    building: Option<Building>,
    /// FID of the last payload with a valid header.
    prev_fid: Option<bool>,
    /// FID of the last completed frame: the next one must differ.
    done_fid: Option<bool>,
    synced: bool,
    /// A failed packet since the last frame started: the next frame is damaged.
    pending_error: bool,
    stats: AssemblerStats,
}

impl Assembler {
    /// Frames come from `pool` (buffers of at least the largest frame); `expected` is the size
    /// of a complete uncompressed frame.
    pub fn new(pool: BufferPool, expected: Option<usize>) -> Assembler {
        Assembler {
            pool,
            expected,
            building: None,
            prev_fid: None,
            done_fid: None,
            synced: false,
            pending_error: false,
            stats: AssemblerStats::default(),
        }
    }

    /// Counters so far.
    pub fn stats(&self) -> AssemblerStats {
        self.stats
    }

    /// Feeds one payload; completed frames are pushed to `out`.
    pub fn push(&mut self, p: Payload<'_>, out: &mut Vec<AssembledFrame>) {
        if p.failed {
            self.stats.failed_packets += 1;
            match self.building.as_mut() {
                Some(b) => b.frame.flags.error = true,
                None => self.pending_error = self.synced,
            }
            return;
        }
        if p.data.is_empty() {
            // Zero-length isochronous packets carry nothing, not even a header.
            return;
        }
        let Some(h) = PayloadHeader::parse(p.data) else {
            self.stats.invalid_headers += 1;
            if let Some(b) = self.building.as_mut() {
                b.frame.flags.error = true;
            }
            return;
        };
        let body = &p.data[h.len..];
        if let Some(b) = &self.building
            && b.frame.fid != h.fid
        {
            // A new frame began without EOF on the last one.
            let mut b = self.building.take().expect("checked");
            b.frame.flags.no_eof = true;
            self.finish(b, out);
        }
        if self.building.is_none() {
            let starts = if self.synced {
                self.done_fid != Some(h.fid)
            } else if self.prev_fid.is_some_and(|f| f != h.fid) {
                // Joined at a FID toggle.
                self.synced = true;
                true
            } else {
                if h.eof {
                    // Joined after a frame's end: the next frame (toggled FID) starts clean.
                    self.synced = true;
                    self.done_fid = Some(h.fid);
                }
                false
            };
            if !starts {
                if !body.is_empty() || !self.synced {
                    self.stats.dropped_payloads += 1;
                }
                self.prev_fid = Some(h.fid);
                return;
            }
            let mut data = self.pool.take();
            data.clear();
            self.building = Some(Building {
                frame: AssembledFrame {
                    data,
                    fid: h.fid,
                    pts: None,
                    scr_first: None,
                    scr_last: None,
                    flags: FrameFlags {
                        error: std::mem::take(&mut self.pending_error),
                        ..FrameFlags::default()
                    },
                    first_packet: p.packet,
                    last_packet: p.packet,
                    first_time_ns: p.time_ns,
                    last_time_ns: p.time_ns,
                    payloads: 0,
                },
            });
        }
        self.prev_fid = Some(h.fid);
        let b = self.building.as_mut().expect("building");
        let f = &mut b.frame;
        f.payloads += 1;
        f.last_packet = p.packet;
        f.last_time_ns = p.time_ns;
        if f.pts.is_none() {
            f.pts = h.pts;
        }
        if h.scr.is_some() {
            f.scr_first = f.scr_first.or(h.scr);
            f.scr_last = h.scr;
        }
        f.flags.error |= h.error;
        f.flags.still |= h.still;
        let room = f.data.capacity().saturating_sub(f.data.len());
        let limit = self
            .expected
            .map_or(room, |e| room.min(e.saturating_sub(f.data.len())));
        if body.len() > limit {
            f.flags.overflow = true;
        }
        f.data.extend_from_slice(&body[..body.len().min(limit)]);
        if h.eof {
            let b = self.building.take().expect("building");
            self.done_fid = Some(h.fid);
            self.finish(b, out);
        }
    }

    /// Drops the frame being built (stream stopped or restarted) and rejoins at the next
    /// frame boundary.
    pub fn reset(&mut self) {
        self.building = None;
        self.prev_fid = None;
        self.done_fid = None;
        self.synced = false;
        self.pending_error = false;
    }

    fn finish(&mut self, b: Building, out: &mut Vec<AssembledFrame>) {
        let mut f = b.frame;
        if f.data.is_empty() {
            // Header-only payloads with a fresh FID: nothing to deliver.
            return;
        }
        if let Some(e) = self.expected
            && f.data.len() < e
        {
            f.flags.short = true;
        }
        self.done_fid = Some(f.fid);
        self.stats.frames += 1;
        if f.flags.damaged() {
            self.stats.damaged += 1;
        }
        out.push(f);
    }
}

#[cfg(test)]
#[path = "payload_tests.rs"]
mod tests;
