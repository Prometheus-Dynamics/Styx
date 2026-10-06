//! `STYX_ZERO_ALLOC_TRACE=1` (glibc): each counted allocation's call stack, as raw return
//! addresses from `backtrace(3)` into a fixed table (no allocation, a few microseconds), grouped
//! by window, thread kind and stack as the window closes, and printed as the test ends (passed
//! or not) with the rare stacks symbolised by `addr2line`: the call site of an unexpected
//! allocation. Off by default; symbolising waits for the end so that it cannot slow the paths
//! under test (a camera service drops a client that stops reading).

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::Kind;

const DEPTH: usize = 40;
const RECORDS: usize = 8192;

static ON: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// `RECORDS` stacks of `DEPTH` addresses, the first word of each the kind; written by one slot
/// owner each (`NEXT`), read after the window closes.
static mut TABLE: [[usize; DEPTH + 1]; RECORDS] = [[0; DEPTH + 1]; RECORDS];

thread_local! {
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(target_env = "gnu")]
unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, size: std::ffi::c_int) -> std::ffi::c_int;
}

/// Fills `out` with the caller's return addresses (zero after the last).
fn stack(out: &mut [usize]) {
    #[cfg(target_env = "gnu")]
    // SAFETY: `out` holds `out.len()` pointer-sized words.
    unsafe {
        backtrace(out.as_mut_ptr().cast(), out.len() as std::ffi::c_int)
    };
    #[cfg(not(target_env = "gnu"))]
    let _ = out;
}

/// Reads the flag (allocates, so before any window is armed) and loads the unwinder.
pub(super) fn init() {
    let on = cfg!(target_env = "gnu")
        && std::env::var_os("STYX_ZERO_ALLOC_TRACE").is_some_and(|v| v != "0");
    if on {
        // glibc loads libgcc_s on its first backtrace (allocating): now, not in a window.
        stack(&mut [0; 4]);
    }
    NEXT.store(0, Ordering::SeqCst);
    ON.store(on, Ordering::SeqCst);
}

/// Whether this allocation is the tracer's own (and so not to be counted).
pub(super) fn inside() -> bool {
    INSIDE.try_with(Cell::get).unwrap_or(false)
}

/// Records a counted allocation's stack, when tracing.
pub(super) fn record(kind: Kind) {
    if !ON.load(Ordering::Relaxed) || INSIDE.try_with(|c| c.replace(true)).unwrap_or(true) {
        return;
    }
    let slot = NEXT.fetch_add(1, Ordering::Relaxed);
    if slot < RECORDS {
        // SAFETY: `slot` is this call's alone; nothing reads the table until the window closes.
        let row = unsafe { &mut (*std::ptr::addr_of_mut!(TABLE))[slot] };
        row[0] = kind as usize;
        stack(&mut row[1..]);
    }
    let _ = INSIDE.try_with(|c| c.set(false));
}

/// Stacks seen in a window: (window, kind, stack, count).
type Group = (String, usize, Vec<usize>, usize);

/// The closed windows' stacks, grouped, symbolised by [`Flush`].
static GROUPS: parking_lot::Mutex<Vec<Group>> = parking_lot::Mutex::new(Vec::new());

/// Groups the window's stacks by kind and stack (quickly: symbolising waits for [`Flush`], so
/// that a slow report cannot stall the paths under test).
pub(super) fn report(label: &str) {
    if !ON.swap(false, Ordering::SeqCst) {
        return;
    }
    let n = NEXT.load(Ordering::SeqCst).min(RECORDS);
    // SAFETY: the window is closed: no more writers.
    let rows = unsafe { &(&*std::ptr::addr_of!(TABLE))[..n] };
    let mut groups = GROUPS.lock();
    let first = groups.len();
    for row in rows {
        let stack: Vec<usize> = row[1..].iter().copied().take_while(|&a| a != 0).collect();
        match groups[first..]
            .iter_mut()
            .find(|g| g.1 == row[0] && g.2 == stack)
        {
            Some(g) => g.3 += 1,
            None => groups.push((label.to_owned(), row[0], stack, 1)),
        }
    }
}

/// Prints every window's stacks, least frequent first in each (the odd one out), symbolised
/// with `addr2line`. Runs as the test ends, passed or not.
pub(super) struct Flush;

impl Drop for Flush {
    fn drop(&mut self) {
        let mut groups = std::mem::take(&mut *GROUPS.lock());
        if groups.is_empty() {
            return;
        }
        groups.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)));
        // One `addr2line` over every rare stack's addresses (a per-frame stack, seen in most
        // frames, is the expected one: counted, not symbolised).
        let rare = |count: usize| count < 32;
        let mut addrs: Vec<usize> = groups
            .iter()
            .filter(|g| rare(g.3))
            .flat_map(|g| g.2.iter().filter_map(|&a| own_offset(a)))
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("addr2line")
            .args(["-f", "-C", "-p", "-e"])
            .arg(&exe)
            .args(addrs.iter().map(|o| format!("{:#x}", o - 1)))
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let names: Vec<&str> = out.lines().collect();
        for (label, kind, stack, count) in groups {
            let kind = ["Test", "FrameSocket", "ServiceClient", "Other", "Reactor"][kind];
            eprintln!("=== {label}: {count} x {kind}");
            if !rare(count) {
                continue;
            }
            for o in stack.iter().filter_map(|&a| own_offset(a)) {
                let i = addrs.binary_search(&o).unwrap();
                eprintln!("    {}", names.get(i).unwrap_or(&"?"));
            }
        }
    }
}

/// `addr`'s offset in this executable, `None` in another object.
fn own_offset(addr: usize) -> Option<usize> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out-parameter.
    if unsafe { libc::dladdr(addr as *const c_void, &mut info) } == 0 {
        return None;
    }
    let main = {
        let mut m: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: as above; `own_offset` is in this executable.
        unsafe { libc::dladdr(own_offset as *const c_void, &mut m) };
        m.dli_fbase
    };
    (info.dli_fbase == main).then(|| addr - main as usize)
}
