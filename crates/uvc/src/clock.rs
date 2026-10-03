//! Timestamps: when each packet crossed the bus, and when the sensor captured each frame.
//!
//! **Bus time.** Isochronous URBs submitted ahead (`ISO_ASAP`) occupy consecutive
//! (micro)frames, so packet `p` of the stream ends exactly `T0 + (p + 1)·Δ` after the stream's
//! bus start `T0` (Δ = 125 µs at high speed). A URB is reaped some time after its last packet:
//! `reaped ≥ T0 + (p_last + 1)·Δ`, with scheduling latency on top. The smallest
//! `reaped − (p_last + 1)·Δ` over a sliding second is `T0` plus the smallest latency seen,
//! which tracks the drift between the host controller's clock and `CLOCK_MONOTONIC` and is
//! free of scheduling jitter. Every packet then has an arrival time, whenever it was reaped.
//!
//! **Device time.** The payload headers' SCR is the device clock (`dwClockFrequency`) sampled
//! at the start of a USB frame whose 11-bit number it carries; PTS is the device clock when
//! the sensor captured the frame. The packet that first carries a new SOF number lies at or
//! after that frame's start, so the bus position of USB frames is the largest
//! `m·sof − p` seen (`m` (micro)frames per USB frame). That puts each SCR sample at an exact
//! bus time; a least-squares line through the recent samples maps the device clock to host
//! time, and the frame's PTS through it gives the capture time — the timestamp `uvcvideo`
//! computes with `hwtimestamps=1`, here from the bus model rather than from interrupt times.

use std::collections::VecDeque;

use crate::payload::Scr;

/// Extends a wrapping counter of `bits` bits to 64 bits.
#[derive(Clone, Copy, Debug, Default)]
struct Unwrap {
    last: Option<(u64, u64)>,
}

impl Unwrap {
    fn next(&mut self, raw: u64, bits: u32) -> u64 {
        let modulus = 1u64 << bits;
        let ext = match self.last {
            None => raw,
            Some((last_raw, last_ext)) => {
                let d = raw.wrapping_sub(last_raw) & (modulus - 1);
                // Steps of more than half the range go backwards.
                if d > modulus / 2 {
                    last_ext.saturating_sub(modulus - d)
                } else {
                    last_ext + d
                }
            }
        };
        self.last = Some((raw, ext));
        ext
    }
}

/// Arrival times of packets from URB reap times.
#[derive(Debug)]
pub struct BusClock {
    interval_ns: u64,
    packets: u64,
    /// (packet count at the URB end, `reaped − end·Δ`), increasing candidates only.
    window: VecDeque<(u64, i128)>,
    window_packets: u64,
    t0: Option<i128>,
}

impl BusClock {
    /// Packets of `interval_ns` each (one per (micro)frame of the endpoint's interval).
    pub fn new(interval_ns: u64) -> BusClock {
        let interval_ns = interval_ns.max(1);
        BusClock {
            interval_ns,
            packets: 0,
            window: VecDeque::new(),
            window_packets: (1_000_000_000 / interval_ns).max(16),
            t0: None,
        }
    }

    /// A URB of `packets` packets was reaped at `reaped_ns`: returns the stream number of its
    /// first packet.
    pub fn urb(&mut self, reaped_ns: u64, packets: u64) -> u64 {
        let first = self.packets;
        self.packets += packets;
        let end = self.packets;
        let candidate = i128::from(reaped_ns) - i128::from(end * self.interval_ns);
        // Sliding minimum: drop larger candidates behind, expired ones in front.
        while self.window.back().is_some_and(|&(_, c)| c >= candidate) {
            self.window.pop_back();
        }
        self.window.push_back((end, candidate));
        while self
            .window
            .front()
            .is_some_and(|&(p, _)| p + self.window_packets < end)
        {
            self.window.pop_front();
        }
        self.t0 = self.window.front().map(|&(_, c)| c);
        first
    }

    /// When packet `p` arrived (the end of its (micro)frame), host monotonic ns.
    pub fn packet_time(&self, p: u64) -> u64 {
        let t0 = self.t0.unwrap_or(0);
        (t0 + i128::from((p + 1) * self.interval_ns)).max(0) as u64
    }

    /// When packet `p`'s (micro)frame started.
    pub fn packet_start(&self, p: u64) -> u64 {
        self.packet_time(p).saturating_sub(self.interval_ns)
    }

    /// The (micro)frame length.
    pub fn interval_ns(&self) -> u64 {
        self.interval_ns
    }
}

const MAX_SAMPLES: usize = 64;

/// Maps the device clock (PTS, SCR) to host time.
#[derive(Debug)]
pub struct DeviceClock {
    hz: f64,
    /// (Micro)frames per USB frame: 8 at high speed and above, 1 at full speed.
    per_sof: u64,
    sof: Unwrap,
    stc: Unwrap,
    last_sof_raw: Option<u16>,
    /// Bus position of USB frame 0 relative to packet 0: the largest `per_sof·sof − p`.
    offset: Option<i64>,
    /// (device clock, host ns) at USB frame starts.
    samples: VecDeque<(f64, f64)>,
    last_sample_sof: Option<u64>,
    last_stc: Option<(u32, u64)>,
}

impl DeviceClock {
    /// A device clock of `hz`; `per_sof` (micro)frames per USB frame.
    pub fn new(hz: u32, per_sof: u64) -> DeviceClock {
        DeviceClock {
            hz: f64::from(hz.max(1)),
            per_sof: per_sof.max(1),
            sof: Unwrap::default(),
            stc: Unwrap::default(),
            last_sof_raw: None,
            offset: None,
            samples: VecDeque::new(),
            last_sample_sof: None,
            last_stc: None,
        }
    }

    /// An SCR seen in packet `p`.
    pub fn scr(&mut self, scr: Scr, p: u64, bus: &BusClock) {
        if self.last_sof_raw == Some(scr.sof) {
            // Only the first packet with a new USB frame number places it.
            return;
        }
        self.last_sof_raw = Some(scr.sof);
        let sof = self.sof.next(u64::from(scr.sof), 11);
        let stc = self.stc.next(u64::from(scr.stc), 32);
        self.last_stc = Some((scr.stc, stc));
        let candidate = (self.per_sof * sof) as i64 - p as i64;
        let offset = self.offset.map_or(candidate, |o| o.max(candidate));
        self.offset = Some(offset);
        // One sample per ~16 USB frames is plenty for a line over a second.
        if self.last_sample_sof.is_some_and(|s| sof < s + 16) {
            return;
        }
        self.last_sample_sof = Some(sof);
        let packet_of_sof = (self.per_sof * sof) as i64 - offset;
        if packet_of_sof < 0 {
            return;
        }
        let host = bus.packet_start(packet_of_sof as u64) as f64;
        self.samples.push_back((stc as f64, host));
        if self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// Host time of device clock value `pts` (the latest SCRs' 32-bit range), once two SCR
    /// samples exist.
    pub fn to_host(&self, pts: u32) -> Option<u64> {
        let (last_raw, last_ext) = self.last_stc?;
        if self.samples.len() < 2 {
            return None;
        }
        let behind = i64::from(last_raw.wrapping_sub(pts) as i32);
        let x = last_ext as f64 - behind as f64;
        let n = self.samples.len() as f64;
        let (mx, my) = self
            .samples
            .iter()
            .fold((0.0, 0.0), |(a, b), &(x, y)| (a + x / n, b + y / n));
        let (sxy, sxx) = self.samples.iter().fold((0.0, 0.0), |(a, b), &(x, y)| {
            (a + (x - mx) * (y - my), b + (x - mx) * (x - mx))
        });
        let nominal = 1e9 / self.hz;
        let slope = if sxx > 0.0 {
            let s = sxy / sxx;
            // A device clock off by more than 1% from its declared frequency is broken.
            if (s / nominal - 1.0).abs() < 0.01 {
                s
            } else {
                nominal
            }
        } else {
            nominal
        };
        let t = my + slope * (x - mx);
        (t > 0.0).then_some(t as u64)
    }

    /// Samples in the fit.
    pub fn samples(&self) -> usize {
        self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_counters() {
        let mut u = Unwrap::default();
        assert_eq!(u.next(2040, 11), 2040);
        assert_eq!(u.next(2047, 11), 2047);
        assert_eq!(u.next(3, 11), 2051);
        assert_eq!(u.next(1, 11), 2049);
        let mut u = Unwrap::default();
        assert_eq!(u.next(u64::from(u32::MAX), 32), u64::from(u32::MAX));
        assert_eq!(u.next(5, 32), u64::from(u32::MAX) + 6);
    }

    /// URBs of 32 high-speed packets reaped 50-800 µs after their last packet: packet times
    /// come out exact once one URB was reaped quickly.
    #[test]
    fn bus_time_ignores_reap_latency() {
        let t0 = 5_000_000_000u64;
        let mut bus = BusClock::new(125_000);
        let lat = [300_000u64, 50_000, 800_000, 120_000, 50_000, 640_000];
        for (i, l) in lat.iter().cycle().take(100).enumerate() {
            let end = (i as u64 + 1) * 32;
            let first = bus.urb(t0 + end * 125_000 + l, 32);
            assert_eq!(first, i as u64 * 32);
        }
        assert_eq!(bus.packet_time(0), t0 + 125_000 + 50_000);
        assert_eq!(bus.packet_time(99), t0 + 100 * 125_000 + 50_000);
    }

    /// A 48 MHz device clock, 0.01% fast, an SCR in every packet: PTS maps to host time
    /// within a microsecond.
    #[test]
    fn device_clock_maps_pts_to_host_time() {
        let hz = 48_000_000u32;
        let t0 = 7_000_000_000u64;
        let mut bus = BusClock::new(125_000);
        let mut dev = DeviceClock::new(hz, 8);
        // Packet p is in microframe p + 3 of the bus; USB frame s starts at microframe 8s.
        let k = 3u64;
        let stc_at = |t_ns: f64| ((t_ns - t0 as f64) * 1e-9 * f64::from(hz) * 1.0001) as u64;
        let mut p = 0u64;
        for _urb in 0..400 {
            bus.urb(t0 + (p + 32) * 125_000 + 30_000, 32);
            for q in p..p + 32 {
                let micro = q + k;
                let sof = micro / 8;
                let sof_start_ns = t0 as f64 + ((8 * sof) as f64 - k as f64) * 125_000.0;
                let stc = stc_at(sof_start_ns) as u32;
                dev.scr(
                    Scr {
                        stc,
                        sof: (sof % 2048) as u16,
                    },
                    q,
                    &bus,
                );
            }
            p += 32;
        }
        assert!(dev.samples() >= 32);
        // A frame captured 1.2 s into the stream.
        let capture_ns = t0 as f64 + 1.2e9;
        let pts = stc_at(capture_ns) as u32;
        let host = dev.to_host(pts).unwrap() as f64;
        // The bus model is 30 µs late (the smallest reap latency).
        let err = host - capture_ns - 30_000.0;
        assert!(err.abs() < 1_000.0, "error {err} ns");
    }
}
