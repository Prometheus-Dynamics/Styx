//! Allocation call sites for `hop_breakdown`, opt-in: `STYX_ALLOC_TRACE=1` records the call
//! stack of every heap allocation the counting allocator sees during a window of frames, and
//! prints the stacks grouped and sorted by count, as raw return addresses in the executable
//! (`exe+0x...`, relative to its load base) for `scripts/symbolize-alloc-trace.sh`.
//!
//! The window: `STYX_ALLOC_TRACE_FRAMES` frames (default 60) starting when the measurement does
//! (after the warm-up, in the steady state), in whatever the mode counts as a frame. Every
//! thread's allocations are recorded, as the counts are. `STYX_ALLOC_TRACE_TOP` stacks are
//! printed (default 20).
//!
//! Recording does not allocate: the stack comes from `_Unwind_Backtrace` (the unwinder reads
//! `.eh_frame`, so no frame pointers and no RUSTFLAGS are needed, glibc or musl, any
//! architecture the unwinder covers) into a fixed table of atomics. A thread-local re-entrancy
//! flag stops the unwinder's own allocations (if it makes any) from recording. With the env var
//! unset the counting hook costs one relaxed atomic load.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Return addresses kept per stack, innermost first.
const DEPTH: usize = 32;
/// Stacks kept per window; allocations beyond are counted and reported, not recorded.
const RECORDS: usize = 16_384;
/// Frames at the top of each stack that are the tracer's own: `record_slow` and the counting
/// hook (`GlobalAlloc::alloc*`, whatever it inlines into).
const SKIP: usize = 2;

/// The env var is set: the unwinder is loaded and a window may open.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The window has opened (it opens once).
static ARMED: AtomicBool = AtomicBool::new(false);
/// The window is open: the one relaxed load every counted allocation makes.
static ON: AtomicBool = AtomicBool::new(false);
static WINDOW_FRAMES: AtomicU64 = AtomicU64::new(60);
static TOP: AtomicUsize = AtomicUsize::new(20);
/// The frame count when the window opened, and the count at which it closes.
static START: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);
/// Frames the window covered, set when it closes.
static FRAMES_SEEN: AtomicU64 = AtomicU64::new(0);
/// Allocations recorded in the window (may exceed `RECORDS`: those are dropped).
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Whether slot `i` of `TABLE` holds a complete stack (set after its addresses).
static READY: [AtomicBool; RECORDS] = [const { AtomicBool::new(false) }; RECORDS];
/// `RECORDS` stacks of `DEPTH` return addresses (zero after the last).
static TABLE: [[AtomicUsize; DEPTH]; RECORDS] =
    [const { [const { AtomicUsize::new(0) }; DEPTH] }; RECORDS];

thread_local! {
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

#[repr(C)]
pub struct UnwindContext {
    _opaque: [u8; 0],
}

type Trace = unsafe extern "C" fn(*mut UnwindContext, *mut c_void) -> c_int;

unsafe extern "C" {
    fn _Unwind_Backtrace(trace: Trace, arg: *mut c_void) -> c_int;
    fn _Unwind_GetIP(ctx: *mut UnwindContext) -> usize;
}

struct Walk {
    skip: usize,
    len: usize,
    ips: [usize; DEPTH],
}

/// Called by the unwinder per frame, innermost first: collects the return addresses.
unsafe extern "C" fn visit(ctx: *mut UnwindContext, arg: *mut c_void) -> c_int {
    // SAFETY: `arg` is the `Walk` that `walk` passed to `_Unwind_Backtrace`, alive for the call.
    let walk = unsafe { &mut *arg.cast::<Walk>() };
    if walk.skip > 0 {
        walk.skip -= 1;
        return 0;
    }
    // SAFETY: `ctx` is the context the unwinder passed for the frame it is visiting.
    let ip = unsafe { _Unwind_GetIP(ctx) };
    if ip == 0 || walk.len == DEPTH {
        return 5; // _URC_END_OF_STACK: stop
    }
    walk.ips[walk.len] = ip;
    walk.len += 1;
    0
}

fn walk(skip: usize) -> Walk {
    let mut walk = Walk {
        skip,
        len: 0,
        ips: [0; DEPTH],
    };
    // SAFETY: `visit` only uses the `Walk` it is given, for this call's duration.
    unsafe { _Unwind_Backtrace(visit, (&raw mut walk).cast()) };
    walk
}

/// Reads the env vars and loads the unwinder (it may allocate: here, not in a window).
pub fn init() {
    if !std::env::var_os("STYX_ALLOC_TRACE").is_some_and(|v| v != "0") {
        return;
    }
    let number = |var: &str| std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok());
    if let Some(n) = number("STYX_ALLOC_TRACE_FRAMES") {
        WINDOW_FRAMES.store(n, Ordering::SeqCst);
    }
    if let Some(n) = number("STYX_ALLOC_TRACE_TOP") {
        TOP.store(n as usize, Ordering::SeqCst);
    }
    walk(0);
    ENABLED.store(true, Ordering::SeqCst);
}

/// Opens the window at `frames` frames seen so far (once, when enabled).
pub fn arm(frames: u64) {
    if !ENABLED.load(Ordering::Relaxed) || ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    START.store(frames, Ordering::SeqCst);
    END.store(
        frames + WINDOW_FRAMES.load(Ordering::SeqCst),
        Ordering::SeqCst,
    );
    ON.store(true, Ordering::SeqCst);
}

/// Closes the window once `frames` frames have passed in it.
pub fn frames(seen: u64) {
    if ON.load(Ordering::Relaxed) && seen >= END.load(Ordering::Relaxed) {
        close(seen);
    }
}

fn close(seen: u64) {
    ON.store(false, Ordering::SeqCst);
    FRAMES_SEEN.store(
        seen.saturating_sub(START.load(Ordering::SeqCst)),
        Ordering::SeqCst,
    );
}

/// Called by the counting hook for every counted allocation: one relaxed load unless the
/// window is open.
#[inline(always)]
pub fn record() {
    if ON.load(Ordering::Relaxed) {
        record_slow();
    }
}

#[inline(never)]
fn record_slow() {
    if INSIDE.try_with(|c| c.replace(true)).unwrap_or(true) {
        return;
    }
    let slot = NEXT.fetch_add(1, Ordering::Relaxed);
    if slot < RECORDS {
        let walk = walk(SKIP);
        for (cell, ip) in TABLE[slot].iter().zip(walk.ips) {
            cell.store(ip, Ordering::Relaxed);
        }
        READY[slot].store(true, Ordering::Release);
    }
    let _ = INSIDE.try_with(|c| c.set(false));
}

/// Prints the window's stacks: the most frequent first, each frame as `object+offset`.
/// Runs after the window (it closes it if the run ended first). Allocates freely: not in a
/// window.
pub fn report(seen: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if ON.load(Ordering::SeqCst) {
        close(seen);
    }
    if !ARMED.load(Ordering::SeqCst) {
        println!("allocation call sites: the window never opened (STYX_ALLOC_TRACE)");
        return;
    }
    let frames = FRAMES_SEEN.load(Ordering::SeqCst);
    let next = NEXT.load(Ordering::SeqCst);
    let mut stacks: HashMap<Vec<usize>, u64> = HashMap::new();
    let mut pending = 0u64;
    for slot in 0..next.min(RECORDS) {
        if !READY[slot].load(Ordering::Acquire) {
            pending += 1;
            continue;
        }
        let stack: Vec<usize> = TABLE[slot]
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .take_while(|&a| a != 0)
            .collect();
        *stacks.entry(stack).or_default() += 1;
    }
    let recorded: u64 = stacks.values().sum();
    let dropped = next.saturating_sub(RECORDS) as u64;
    let mut groups: Vec<(Vec<usize>, u64)> = stacks.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let per_frame = |n: u64| n as f64 / frames.max(1) as f64;
    println!(
        "allocation call sites (STYX_ALLOC_TRACE): {} allocations in {frames} frames ({:.2} per frame; {recorded} recorded, {dropped} dropped: table full, {pending} unfinished), {} distinct stacks",
        recorded + dropped,
        per_frame(recorded + dropped),
        groups.len(),
    );
    let top = TOP.load(Ordering::SeqCst);
    for (i, (stack, count)) in groups.iter().take(top).enumerate() {
        println!(
            "#{:<3} {:.2} per frame ({count} allocations in {frames} frames)",
            i + 1,
            per_frame(*count)
        );
        for &ip in stack {
            println!("      {}", locate(ip));
        }
    }
    println!("symbolize with: scripts/symbolize-alloc-trace.sh BINARY LOGFILE");
}

/// `addr` as `exe+offset` (the executable's offset: its load base removed), or
/// `libname+offset` for a shared object, or the raw address when no object holds it.
fn locate(addr: usize) -> String {
    struct Find {
        addr: usize,
        index: usize,
        found: Option<String>,
    }
    unsafe extern "C" fn each(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: `dl_iterate_phdr` passes a valid `info` and the `Find` we gave it.
        let (info, find) = unsafe { (&*info, &mut *data.cast::<Find>()) };
        let base = info.dlpi_addr as usize;
        // SAFETY: the loader's program headers for this object, `dlpi_phnum` of them.
        let phdrs = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize) };
        let hit = phdrs.iter().any(|ph| {
            let lo = base + ph.p_vaddr as usize;
            ph.p_type == libc::PT_LOAD && (lo..lo + ph.p_memsz as usize).contains(&find.addr)
        });
        if hit {
            let name = if find.index == 0 {
                "exe".to_owned()
            } else if info.dlpi_name.is_null() {
                "?".to_owned()
            } else {
                // SAFETY: the loader's NUL-terminated name for this object.
                let full = unsafe { CStr::from_ptr(info.dlpi_name) }.to_string_lossy();
                full.rsplit('/').next().unwrap_or_default().to_owned()
            };
            find.found = Some(format!("{name}+{:#x}", find.addr - base));
            return 1;
        }
        find.index += 1;
        0
    }
    let mut find = Find {
        addr,
        index: 0,
        found: None,
    };
    // SAFETY: `each` only uses the `Find` it is given, for this call's duration.
    unsafe { libc::dl_iterate_phdr(Some(each), (&raw mut find).cast()) };
    find.found.unwrap_or_else(|| format!("{addr:#x}"))
}
