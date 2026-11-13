//! The node: connects to a producer, learns its buffers, and runs a PipeWire `Video/Source` stream
//! whose buffers are those memfds. The PipeWire loop calls back into [`Pump`] on a 2 ms timer.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
#[cfg(has_pipewire)]
use std::sync::atomic::Ordering;

#[cfg(has_pipewire)]
use std::ffi::CString;

use crate::learn::LearnError;
#[cfg(has_pipewire)]
use crate::learn::{connect, learn};

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// The node's name, as PipeWire clients see it (`target-object` in `pipewiresrc`).
    pub name: String,
    /// The producer's socket.
    pub socket: PathBuf,
    /// The producer's ring buffer count. Must match the producer; a frame in another index is
    /// counted and dropped.
    pub buffers: usize,
    /// Frame rate offered to consumers, numerator and denominator.
    pub fps: (u32, u32),
}

/// What a run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NodeStats {
    /// Frames queued to PipeWire, each in the producer's own buffer.
    pub frames: u64,
    /// Frames released at once because the consumer still had their buffer.
    pub busy: u64,
    /// Frames PipeWire gave back.
    pub released: u64,
    /// Frames in an index the node did not learn (the producer's ring has more buffers).
    pub unknown: u64,
}

/// Why a run failed.
#[derive(Debug)]
pub enum NodeError {
    /// This build has no PipeWire node: the `pipewire` feature was off, or libpipewire-0.3 was
    /// not found when the crate was built.
    NotBuilt,
    /// The buffers could not be learned.
    Learn(LearnError),
    /// The node name has a NUL byte, or the format has no PipeWire video format.
    BadConfig(&'static str),
    /// The PipeWire shim could not start (code from the shim: -1 memory or loop, -2 format,
    /// -3 connect, -4 a buffer could not be used).
    Start(i32),
    /// The producer went away while the node ran. The node does not reconnect: a new producer
    /// has new buffers, so the node must learn them again.
    ProducerLost,
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBuilt => f.write_str("this build has no PipeWire node (feature `pipewire`)"),
            Self::Learn(err) => write!(f, "{err}"),
            Self::BadConfig(why) => write!(f, "configuration: {why}"),
            Self::Start(code) => write!(f, "the PipeWire node did not start (code {code})"),
            Self::ProducerLost => f.write_str("the producer went away"),
        }
    }
}

impl std::error::Error for NodeError {}

/// Runs the node until `stop` is set, or the producer goes away.
///
/// # Errors
///
/// [`NodeError`]. With `stop` set before the buffers are learned the run ends with
/// [`NodeError::Learn`]`(`[`LearnError::Stopped`]`)`.
pub fn run(config: &NodeConfig, stop: &AtomicBool) -> Result<NodeStats, NodeError> {
    #[cfg(has_pipewire)]
    {
        run_built(config, stop)
    }
    #[cfg(not(has_pipewire))]
    {
        let _ = (config, stop);
        Err(NodeError::NotBuilt)
    }
}

/// Whether this build has the PipeWire node (the feature is on and the library was found).
pub const fn node_built() -> bool {
    cfg!(has_pipewire)
}

/// What the PipeWire shim reads. Mirrors `styx_pw_config` in `c_api/styx_pipewire.c`.
#[cfg(has_pipewire)]
#[repr(C)]
struct Config {
    name: *const std::ffi::c_char,
    fourcc: u32,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    buffers: u32,
    stride: u32,
    fds: *const i32,
    lens: *const u32,
}

/// One frame the shim is handed. Mirrors `styx_pw_frame`.
#[cfg(has_pipewire)]
#[repr(C)]
struct PwFrame {
    index: u32,
    len: u32,
    stride: u32,
    token: *mut std::ffi::c_void,
}

/// The shim's functions. Mirrors `styx_pw_ops`.
#[cfg(has_pipewire)]
#[repr(C)]
struct Ops {
    next: unsafe extern "C" fn(*mut std::ffi::c_void, *mut PwFrame) -> std::ffi::c_int,
    release: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void),
    running: unsafe extern "C" fn(*mut std::ffi::c_void) -> std::ffi::c_int,
}

/// Mirrors `styx_pw_stats`.
#[cfg(has_pipewire)]
#[repr(C)]
#[derive(Default)]
struct ShimStats {
    frames: u64,
    busy: u64,
    released: u64,
}

#[cfg(has_pipewire)]
unsafe extern "C" {
    fn styx_pw_run(
        config: *const Config,
        ops: *const Ops,
        ctx: *mut std::ffi::c_void,
        stats: *mut ShimStats,
    ) -> std::ffi::c_int;
}

/// The Rust side of one run: the reader, and the frames the shim holds (one slot per frame).
#[cfg(has_pipewire)]
struct Pump<'a> {
    reader: styx_socket::Reader,
    slots: Vec<Option<styx_socket::Frame>>,
    stop: &'a AtomicBool,
    lost: bool,
    unknown: u64,
}

#[cfg(has_pipewire)]
impl Pump<'_> {
    /// The next frame, held in a free slot, or `None` (nothing new, or no slot free).
    fn next(&mut self) -> Option<PwFrame> {
        let slot = self.slots.iter().position(Option::is_none)?;
        match self.reader.next_frame() {
            Ok(Some(frame)) => {
                let index = frame.buffer();
                if index >= self.slots.len() {
                    self.unknown += 1;
                    return None;
                }
                let lease = frame.lease();
                let stride = lease
                    .layouts()
                    .first()
                    .map_or(0, |plane| plane.stride as u32);
                let len = lease.layouts().span().unwrap_or(0) as u32;
                let index = index as u32;
                self.slots[slot] = Some(frame);
                Some(PwFrame {
                    index,
                    len,
                    stride,
                    token: (slot + 1) as *mut std::ffi::c_void,
                })
            }
            Ok(None) => None,
            Err(_) => {
                self.lost = true;
                None
            }
        }
    }
}

#[cfg(has_pipewire)]
fn run_built(config: &NodeConfig, stop: &AtomicBool) -> Result<NodeStats, NodeError> {
    let name = CString::new(config.name.as_str())
        .map_err(|_| NodeError::BadConfig("the node name has a NUL byte"))?;
    let mut reader = match connect(&config.socket, stop) {
        Some(reader) => reader,
        None => return Err(NodeError::Learn(LearnError::Stopped)),
    };
    let buffers = learn(&mut reader, config.buffers, stop).map_err(NodeError::Learn)?;
    let fds = buffers.raw_fds();
    let shim = Config {
        name: name.as_ptr(),
        fourcc: buffers.fourcc.to_u32(),
        width: buffers.resolution.width.get(),
        height: buffers.resolution.height.get(),
        fps_num: config.fps.0,
        fps_den: config.fps.1.max(1),
        buffers: config.buffers as u32,
        stride: buffers.stride,
        fds: fds.as_ptr(),
        lens: buffers.lens.as_ptr(),
    };
    let ops = Ops {
        next: next_frame,
        release: release_frame,
        running,
    };
    let mut pump = Pump {
        reader,
        slots: (0..config.buffers).map(|_| None).collect(),
        stop,
        lost: false,
        unknown: 0,
    };
    let mut stats = ShimStats::default();
    // SAFETY: `shim`, `fds`, `lens` and `name` live until the call returns, and the shim copies
    // what it keeps (its own descriptors are duplicates). `ops` is a static table of functions
    // that take `ctx`, which is `pump` and lives for the call; the shim calls them only on the
    // thread that runs it, inside the call.
    let rc = unsafe {
        styx_pw_run(
            &raw const shim,
            &raw const ops,
            (&raw mut pump).cast(),
            &raw mut stats,
        )
    };
    if pump.lost {
        return Err(NodeError::ProducerLost);
    }
    if rc != 0 {
        return Err(NodeError::Start(rc));
    }
    Ok(NodeStats {
        frames: stats.frames,
        busy: stats.busy,
        released: stats.released,
        unknown: pump.unknown,
    })
}

#[cfg(has_pipewire)]
unsafe extern "C" fn next_frame(ctx: *mut std::ffi::c_void, out: *mut PwFrame) -> std::ffi::c_int {
    // SAFETY: `ctx` is the `Pump` `run_built` passed to the shim for this call, and the shim only
    // calls from the thread that runs the call, so this is the only reference.
    let pump = unsafe { &mut *ctx.cast::<Pump<'_>>() };
    match pump.next() {
        Some(frame) => {
            // SAFETY: `out` is the shim's frame slot, valid for a write.
            unsafe { out.write(frame) };
            1
        }
        None => 0,
    }
}

#[cfg(has_pipewire)]
unsafe extern "C" fn release_frame(ctx: *mut std::ffi::c_void, token: *mut std::ffi::c_void) {
    // SAFETY: as in `next_frame`.
    let pump = unsafe { &mut *ctx.cast::<Pump<'_>>() };
    let slot = (token as usize).wrapping_sub(1);
    if let Some(entry) = pump.slots.get_mut(slot) {
        *entry = None;
    }
}

#[cfg(has_pipewire)]
unsafe extern "C" fn running(ctx: *mut std::ffi::c_void) -> std::ffi::c_int {
    // SAFETY: as in `next_frame`.
    let pump = unsafe { &*ctx.cast::<Pump<'_>>() };
    i32::from(!pump.stop.load(Ordering::Acquire) && !pump.lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_node_says_so() {
        if !node_built() {
            let config = NodeConfig {
                name: "styx".into(),
                socket: PathBuf::from("/nonexistent/styx-pw-test.sock"),
                buffers: 4,
                fps: (30, 1),
            };
            assert!(matches!(
                run(&config, &AtomicBool::new(false)),
                Err(NodeError::NotBuilt)
            ));
        }
    }

    #[test]
    fn stop_before_connecting_ends_the_run_without_a_producer() {
        let config = NodeConfig {
            name: "styx".into(),
            socket: PathBuf::from("/nonexistent/styx-pw-test.sock"),
            buffers: 4,
            fps: (30, 1),
        };
        let stop = AtomicBool::new(true);
        let result = run(&config, &stop);
        if node_built() {
            assert!(matches!(result, Err(NodeError::Learn(LearnError::Stopped))));
        } else {
            assert!(matches!(result, Err(NodeError::NotBuilt)));
        }
    }
}
