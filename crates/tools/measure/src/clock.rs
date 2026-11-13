//! The board's clocks and counters: `CLOCK_MONOTONIC` for frame times, the CPU time and memory of
//! this process and of another one (from `/proc`). Nothing here allocates per call except the
//! `/proc` reads, which run at start and end of a run only.

use std::fs;

/// `CLOCK_MONOTONIC` now, in nanoseconds. The clock frames are stamped with.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec` for the duration of the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    if rc != 0 {
        // CLOCK_MONOTONIC cannot fail on Linux; zero marks the impossible case.
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

/// This process's user plus system CPU time so far, in nanoseconds (`getrusage`).
pub fn own_cpu_ns() -> u64 {
    // SAFETY: an all-zero `rusage` is a valid value for `getrusage` to fill.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable `rusage` for the duration of the call.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    if rc != 0 {
        return 0;
    }
    timeval_ns(usage.ru_utime) + timeval_ns(usage.ru_stime)
}

fn timeval_ns(t: libc::timeval) -> u64 {
    (t.tv_sec as u64) * 1_000_000_000 + (t.tv_usec as u64) * 1_000
}

/// The resident and peak resident memory of a process, in bytes (`VmRSS`, `VmHWM`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    pub rss_bytes: u64,
    pub peak_bytes: u64,
}

/// Memory of process `pid` (`"self"` for this one) from `/proc/<pid>/status`, or `None` when the
/// process has no such file.
pub fn memory_of(pid: &str) -> Option<Memory> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut memory = Memory::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let kib = value
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok());
        match (key, kib) {
            ("VmRSS", Some(kib)) => memory.rss_bytes = kib * 1024,
            ("VmHWM", Some(kib)) => memory.peak_bytes = kib * 1024,
            _ => {}
        }
    }
    (memory.rss_bytes > 0).then_some(memory)
}

/// User plus system CPU time of process `pid` from `/proc/<pid>/stat` (fields 14 and 15), in
/// nanoseconds. The kernel counts in clock ticks, so the resolution is one tick (10 ms at 100 Hz).
pub fn cpu_of(pid: u32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces: fields start after the last ')'.
    let after = text.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_ascii_whitespace().collect();
    // After the ')': field 3 (state) is index 0, so utime (14) is 11 and stime (15) is 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let hz = clock_ticks_per_second()?;
    Some((utime + stime) * 1_000_000_000 / hz)
}

fn clock_ticks_per_second() -> Option<u64> {
    // SAFETY: `sysconf` takes a plain integer and reads no memory of ours.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(hz).ok().filter(|&hz| hz > 0)
}
