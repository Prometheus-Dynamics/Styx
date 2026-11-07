//! The exported C functions (`include/styx.h`). Each runs inside [`lemnos_abi::guard`], returns a
//! [`Status`] and writes its results through the caller's pointers; on an error it writes the
//! reason to the [`MessageSink`] it was given, if any.
//!
//! Cameras are named by their index in the list `styx_camera_count` probed last, streams and
//! frames by `lemnos-abi` handles. A frame handle holds its frame (its buffer stays out of the
//! capture's queue) until `styx_frame_release`. Handles come from slabs whose slots are reused,
//! so taking and releasing frames allocates nothing once the slabs have grown to the frames held.
//!
//! Output structs start with `struct_size`: the caller sets it to the size it was built with,
//! and a call refuses a struct smaller than its own, so a newer surface never writes past an
//! older caller's struct.

// With `shared` the functions below are not exported, and only the tests call them.
#![cfg_attr(feature = "shared", allow(dead_code))]

use core::mem::{align_of, offset_of, size_of};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lemnos_abi::{
    AbiVersion, BufferDesc, Features, Handle, MAX_PLANES, MessageSink, PlaneDesc, Slab, Status,
    guard, layout_hash,
};
use styx::prelude::{
    ControlId, ControlValue, FourCc, FrameLease, Frames, ProbedDevice, RecvOutcome, StyxConfig,
    TimestampClock,
};

/// What a stream asks for (`StyxStreamRequest`). Zero in a field: any.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamRequest {
    pub struct_size: u32,
    /// A DRM/V4L2 fourcc (`'N','V','1','2'` little-endian), 0 for any.
    pub fourcc: u32,
    /// The largest frame wanted (both or neither).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// None defined yet: 0.
    pub flags: u32,
}

/// A frame's description (`StyxFrameInfo`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameInfo {
    pub struct_size: u32,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub plane_count: u32,
    /// `STYX_CLOCK_*`: what `timestamp_ns` counts on.
    pub clock: u32,
    /// `STYX_FRAME_DMABUF` (`styx_frame_buffer` works), `STYX_FRAME_HOST` (`styx_frame_plane`).
    pub flags: u32,
    pub reserved: u32,
    /// Frames this stream delivered before this one.
    pub sequence: u64,
    pub timestamp_ns: u64,
}

/// A control's value (`StyxControlValue`): `integer` for `BOOL`, `INT` and `UINT`, `real` for
/// `FLOAT`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControlValueC {
    pub struct_size: u32,
    /// `STYX_CONTROL_*`.
    pub kind: u32,
    pub integer: i64,
    pub real: f64,
}

pub const CLOCK_UNKNOWN: u32 = 0;
pub const CLOCK_MONOTONIC: u32 = 1;
pub const CLOCK_BOOTTIME: u32 = 2;
pub const CLOCK_REALTIME: u32 = 3;
pub const CLOCK_STREAM: u32 = 4;
pub const FRAME_DMABUF: u32 = 1;
pub const FRAME_HOST: u32 = 2;
pub const CONTROL_NONE: u32 = 0;
pub const CONTROL_BOOL: u32 = 1;
pub const CONTROL_INT: u32 = 2;
pub const CONTROL_UINT: u32 = 3;
pub const CONTROL_FLOAT: u32 = 4;

pub(crate) const LAYOUT: u64 = layout_hash(&[
    lemnos_abi::LAYOUT_HASH,
    size_of::<StreamRequest>() as u64,
    align_of::<StreamRequest>() as u64,
    offset_of!(StreamRequest, flags) as u64,
    size_of::<FrameInfo>() as u64,
    align_of::<FrameInfo>() as u64,
    offset_of!(FrameInfo, sequence) as u64,
    offset_of!(FrameInfo, timestamp_ns) as u64,
    size_of::<ControlValueC>() as u64,
    align_of::<ControlValueC>() as u64,
    offset_of!(ControlValueC, integer) as u64,
    offset_of!(ControlValueC, real) as u64,
]);

/// `STYX_LAYOUT_HASH` in `include/styx.h`; the layouts hold no pointers, so it is the same on
/// every target.
pub(crate) const HEADER_LAYOUT_HASH: u64 = 0x5c37_0710_333d_b30e;
const _: () = assert!(
    LAYOUT == HEADER_LAYOUT_HASH,
    "update STYX_LAYOUT_HASH in styx.h"
);

/// This surface's version (`styx_version`): 1.0.
pub const VERSION: AbiVersion = AbiVersion::new(1, 0, LAYOUT);

struct Stream {
    frames: Frames,
    delivered: u64,
}

/// The cameras the last `styx_camera_count` found.
static CAMERAS: Mutex<Vec<ProbedDevice>> = Mutex::new(Vec::new());
/// Open streams. The table's lock is held only to find one; each has its own, so a call
/// waiting for a frame on one stream does not block another.
static STREAMS: Mutex<Slab<Arc<Mutex<Stream>>>> = Mutex::new(Slab::new());
/// Frames handed out and not released.
static FRAMES: Mutex<Slab<FrameLease>> = Mutex::new(Slab::new());

fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds `device` to the camera list (tests, and hosts that build a device themselves): its
/// index.
pub fn add_camera(device: ProbedDevice) -> u32 {
    let mut cameras = lock(&CAMERAS);
    cameras.push(device);
    (cameras.len() - 1) as u32
}

fn with_stream(stream: Handle, f: impl FnOnce(&mut Stream) -> Status) -> Status {
    let entry = match lock(&STREAMS).get(stream) {
        Ok(entry) => Arc::clone(entry),
        Err(status) => return status,
    };
    let mut entry = entry.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut entry)
}

fn with_frame(frame: Handle, f: impl FnOnce(&FrameLease) -> Status) -> Status {
    match lock(&FRAMES).get(frame) {
        Ok(lease) => f(lease),
        Err(status) => status,
    }
}

fn count(n: usize) -> Status {
    Status(i32::try_from(n).unwrap_or(i32::MAX))
}

/// # Safety
/// `out` is null or valid for a write of a `T`.
unsafe fn put<T>(out: *mut T, value: T) -> Status {
    if out.is_null() {
        return Status::INVALID_ARGUMENT;
    }
    // SAFETY: not null, and valid for the write by the caller's contract.
    unsafe { out.write(value) };
    Status::OK
}

/// Checks the `struct_size` the caller set at the start of `out` against `T`.
///
/// # Safety
/// `out` is null or valid for a read of a `u32` at its start.
unsafe fn sized<T>(out: *const T) -> Status {
    if out.is_null() {
        return Status::INVALID_ARGUMENT;
    }
    // SAFETY: not null, and its first field is the caller's `struct_size`.
    let caller = unsafe { out.cast::<u32>().read() };
    if (caller as usize) < size_of::<T>() {
        Status::INVALID_ARGUMENT
    } else {
        Status::OK
    }
}

fn clock(clock: Option<TimestampClock>) -> u32 {
    match clock {
        Some(TimestampClock::Monotonic) => CLOCK_MONOTONIC,
        Some(TimestampClock::Boottime) => CLOCK_BOOTTIME,
        Some(TimestampClock::Realtime) => CLOCK_REALTIME,
        Some(TimestampClock::StreamRelative) => CLOCK_STREAM,
        None => CLOCK_UNKNOWN,
    }
}

fn describe(lease: &FrameLease, sequence: u64) -> FrameInfo {
    let meta = lease.meta();
    let res = meta.format.resolution;
    let mut flags = 0;
    if lease.dmabuf_plane(0).is_some() {
        flags |= FRAME_DMABUF;
    }
    if lease.has_host_readable_bytes() {
        flags |= FRAME_HOST;
    }
    FrameInfo {
        struct_size: size_of::<FrameInfo>() as u32,
        fourcc: meta.format.code.to_u32(),
        width: res.width.get(),
        height: res.height.get(),
        plane_count: lease.layout_slice().len() as u32,
        clock: clock(meta.clock),
        flags,
        reserved: 0,
        sequence,
        timestamp_ns: meta.timestamp,
    }
}

/// The surface's version, for the handshake (`styx_check_version`).
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub extern "C" fn styx_version() -> AbiVersion {
    VERSION
}

/// `OK` when this surface serves a caller built against `caller`, else `VERSION_MISMATCH`.
///
/// # Safety
/// `caller` is null or points to a readable `LemnosAbiVersion`.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_check_version(caller: *const AbiVersion) -> Status {
    guard(|| {
        if caller.is_null() {
            return Status::INVALID_ARGUMENT;
        }
        // SAFETY: not null, and readable by the caller's contract.
        let caller = unsafe { caller.read() };
        VERSION.accepts(&caller)
    })
}

/// This surface's optional features: none in 1.0.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub extern "C" fn styx_features() -> Features {
    Features::NONE
}

/// Probes the cameras and returns how many there are (a positive status). Indices name them in
/// every call after, until the next probe.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub extern "C" fn styx_camera_count() -> Status {
    guard(|| {
        let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
        let mut cameras = lock(&CAMERAS);
        *cameras = probe.devices;
        count(cameras.len())
    })
}

/// Camera `index`'s name, NUL-terminated, into `name` (`capacity` bytes); its length without
/// the NUL into `len`. `BUFFER_TOO_SMALL` (nothing copied, `len` still written) when it does
/// not fit.
///
/// # Safety
/// `name` is null or valid for `capacity` bytes; `len` is null or valid for a `usize`.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_camera_name(
    index: u32,
    name: *mut u8,
    capacity: usize,
    len: *mut usize,
) -> Status {
    guard(|| {
        let cameras = lock(&CAMERAS);
        let Some(camera) = cameras.get(index as usize) else {
            return Status::NOT_FOUND;
        };
        let out = if name.is_null() {
            None
        } else {
            // SAFETY: not null, and valid for `capacity` bytes by the caller's contract.
            Some(unsafe { core::slice::from_raw_parts_mut(name, capacity) })
        };
        let written = lemnos_abi::write_text(&camera.identity.display, out);
        if !len.is_null() {
            // SAFETY: not null, and valid for the write by the caller's contract.
            unsafe { len.write(written.needed) };
        }
        written.status
    })
}

/// Adds a synthetic camera (a moving test pattern, `fourcc` frames of `width` x `height` at
/// `fps`) named `name`, for tests and demos without hardware; its index is the status.
///
/// # Safety
/// `name` is a NUL-terminated string.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_camera_add_virtual(
    name: *const core::ffi::c_char,
    fourcc: u32,
    width: u32,
    height: u32,
    fps: u32,
) -> Status {
    guard(|| {
        if name.is_null() || width == 0 || height == 0 || fps == 0 {
            return Status::INVALID_ARGUMENT;
        }
        // SAFETY: not null, NUL-terminated by the caller's contract, read only in this call.
        let name = unsafe { core::ffi::CStr::from_ptr(name) };
        let Ok(name) = name.to_str() else {
            return Status::INVALID_ARGUMENT;
        };
        let device = styx::prelude::VirtualSourceConfig::new()
            .name(name)
            .format(FourCc::new(fourcc.to_le_bytes()))
            .resolution(width, height)
            .fps(fps)
            .into_device();
        count(add_camera(device) as usize)
    })
}

/// Plans and starts a stream of camera `index` for `request`, and writes its handle.
///
/// # Safety
/// `request` points to a readable `StyxStreamRequest`; `stream` is valid for a handle.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_stream_open(
    index: u32,
    request: *const StreamRequest,
    stream: *mut Handle,
    sink: MessageSink,
) -> Status {
    guard(|| {
        // SAFETY: null or readable for its `struct_size` by the caller's contract.
        let status = unsafe { sized(request) };
        if !status.is_ok() || stream.is_null() {
            return Status::INVALID_ARGUMENT;
        }
        // SAFETY: checked above: not null, and as large as `StreamRequest`.
        let r = unsafe { request.read() };
        let Some(device) = lock(&CAMERAS).get(index as usize).cloned() else {
            return Status::NOT_FOUND;
        };
        let mut req = match r.fourcc {
            0 => Frames::any(),
            code => Frames::formats([FourCc::new(code.to_le_bytes())]),
        };
        if r.width > 0 && r.height > 0 {
            req = req.size_at_most(r.width, r.height);
        }
        if r.fps > 0 {
            req = req.fps(r.fps);
        }
        let started = styx::planner::plan_many(&device, &[req])
            .map_err(|e| e.to_string())
            .and_then(|plan| plan.start().map_err(|e| e.to_string()));
        let frames = match started.map(|mut v| v.pop()) {
            Ok(Some(frames)) => frames,
            Ok(None) => return Status::NOT_SUPPORTED,
            Err(why) => {
                sink.write(&why);
                return Status::NOT_SUPPORTED;
            }
        };
        let entry = Arc::new(Mutex::new(Stream {
            frames,
            delivered: 0,
        }));
        match lock(&STREAMS).insert(entry) {
            // SAFETY: not null (checked above), valid for a handle by the caller's contract.
            Ok(handle) => unsafe { put(stream, handle) },
            Err(status) => status,
        }
    })
}

/// Stops a stream. Its frames not released stay valid until they are.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub extern "C" fn styx_stream_close(stream: Handle) -> Status {
    guard(|| {
        let entry = match lock(&STREAMS).remove(stream) {
            Ok(entry) => entry,
            Err(status) => return status,
        };
        if let Ok(entry) = Arc::try_unwrap(entry) {
            entry
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .frames
                .stop();
        }
        Status::OK
    })
}

/// Waits up to `timeout_ms` for the stream's next frame; writes its handle and description.
/// `TIMED_OUT` when none came, `-EPIPE` when the stream ended.
///
/// # Safety
/// `frame` is valid for a handle; `info` is null or a `StyxFrameInfo` with `struct_size` set.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_stream_next(
    stream: Handle,
    timeout_ms: u32,
    frame: *mut Handle,
    info: *mut FrameInfo,
) -> Status {
    guard(|| {
        if frame.is_null() {
            return Status::INVALID_ARGUMENT;
        }
        // SAFETY: null or holding its `struct_size` by the caller's contract.
        if !info.is_null() && !unsafe { sized(info) }.is_ok() {
            return Status::INVALID_ARGUMENT;
        }
        let mut lease = None;
        let mut sequence = 0;
        let status = with_stream(stream, |s| {
            match s
                .frames
                .next_frame(Duration::from_millis(u64::from(timeout_ms)))
            {
                RecvOutcome::Data(f) => {
                    sequence = s.delivered;
                    s.delivered += 1;
                    lease = Some(f);
                    Status::OK
                }
                RecvOutcome::Empty => Status::TIMED_OUT,
                RecvOutcome::Closed => Status::from_errno(32),
            }
        });
        let Some(lease) = lease else {
            return status;
        };
        let described = describe(&lease, sequence);
        let handle = match lock(&FRAMES).insert(lease) {
            Ok(handle) => handle,
            Err(status) => return status,
        };
        if !info.is_null() {
            // SAFETY: not null and as large as `FrameInfo` (checked above).
            unsafe { info.write(described) };
        }
        // SAFETY: not null (checked above), valid for a handle by the caller's contract.
        unsafe { put(frame, handle) }
    })
}

/// The frame's dma-bufs (`BufferDesc`, fds borrowed: valid until `styx_frame_release`, not
/// `OWNED`). `NOT_SUPPORTED` for a frame in host memory only.
///
/// # Safety
/// `desc` is a `LemnosBufferDesc` with `struct_size` set.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_frame_buffer(frame: Handle, desc: *mut BufferDesc) -> Status {
    guard(|| {
        // SAFETY: null or holding its `struct_size` by the caller's contract.
        if !unsafe { sized(desc) }.is_ok() {
            return Status::INVALID_ARGUMENT;
        }
        with_frame(frame, |lease| {
            let meta = lease.meta();
            let res = meta.format.resolution;
            let mut out = BufferDesc::image(
                res.width.get(),
                res.height.get(),
                meta.format.code.to_u32(),
                0,
            );
            let layouts = lease.layout_slice();
            if layouts.is_empty() || layouts.len() > MAX_PLANES {
                return Status::NOT_SUPPORTED;
            }
            for (i, layout) in layouts.iter().enumerate() {
                let Some(plane) = lease.dmabuf_plane(i) else {
                    return Status::NOT_SUPPORTED;
                };
                use std::os::fd::AsRawFd;
                let desc = PlaneDesc {
                    fd: plane.fd.as_raw_fd(),
                    reserved: 0,
                    offset: plane.offset as u64,
                    stride: layout.stride as u64,
                    size: layout.len as u64,
                };
                out = match out.with_plane(desc) {
                    Ok(out) => out,
                    Err(status) => return status,
                };
            }
            // SAFETY: not null and as large as `BufferDesc` (checked above).
            unsafe { desc.write(out) };
            Status::OK
        })
    })
}

/// Plane `plane`'s bytes in this process: pointer, length and stride, valid until
/// `styx_frame_release`. `NOT_SUPPORTED` for a frame not readable here.
///
/// # Safety
/// `data`, `len` and `stride` are valid for their writes.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_frame_plane(
    frame: Handle,
    plane: u32,
    data: *mut *const u8,
    len: *mut u64,
    stride: *mut u64,
) -> Status {
    guard(|| {
        if data.is_null() || len.is_null() || stride.is_null() {
            return Status::INVALID_ARGUMENT;
        }
        with_frame(frame, |lease| {
            if !lease.has_host_readable_bytes() {
                return Status::NOT_SUPPORTED;
            }
            let Some(p) = lease.plane_at(plane as usize) else {
                return Status::NOT_FOUND;
            };
            let bytes = p.data();
            // The bytes stay where they are while the frame handle holds the lease.
            // SAFETY: not null (checked above), valid for the write by the caller's contract.
            unsafe { data.write(bytes.as_ptr()) };
            // SAFETY: as above.
            unsafe { len.write(bytes.len() as u64) };
            // SAFETY: as above.
            unsafe { stride.write(p.stride() as u64) };
            Status::OK
        })
    })
}

/// Gives the frame back to its capture.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub extern "C" fn styx_frame_release(frame: Handle) -> Status {
    guard(|| {
        // Dropped outside the table's lock: giving a buffer back may wake the capture.
        let lease = lock(&FRAMES).remove(frame);
        match lease {
            Ok(lease) => {
                drop(lease);
                Status::OK
            }
            Err(status) => status,
        }
    })
}

/// Control `id`'s value now.
///
/// # Safety
/// `value` is a `StyxControlValue` with `struct_size` set.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_control_get(
    stream: Handle,
    id: u32,
    value: *mut ControlValueC,
    sink: MessageSink,
) -> Status {
    guard(|| {
        // SAFETY: null or holding its `struct_size` by the caller's contract.
        if !unsafe { sized(value) }.is_ok() {
            return Status::INVALID_ARGUMENT;
        }
        with_stream(stream, |s| match s.frames.get_control(ControlId(id)) {
            Ok(v) => {
                let (kind, integer, real) = match v {
                    ControlValue::Bool(b) => (CONTROL_BOOL, i64::from(b), 0.0),
                    ControlValue::Int(i) => (CONTROL_INT, i64::from(i), 0.0),
                    ControlValue::Uint(u) => (CONTROL_UINT, i64::from(u), 0.0),
                    ControlValue::Float(f) => (CONTROL_FLOAT, 0, f64::from(f)),
                    ControlValue::None => (CONTROL_NONE, 0, 0.0),
                    _ => return Status::NOT_SUPPORTED,
                };
                let out = ControlValueC {
                    struct_size: size_of::<ControlValueC>() as u32,
                    kind,
                    integer,
                    real,
                };
                // SAFETY: not null and as large as `ControlValueC` (checked above).
                unsafe { value.write(out) };
                Status::OK
            }
            Err(e) => {
                sink.write_fmt(format_args!("{e}"));
                Status::NOT_FOUND
            }
        })
    })
}

/// Sets control `id`.
///
/// # Safety
/// `value` points to a readable `StyxControlValue` with `struct_size` set.
#[cfg_attr(not(feature = "shared"), unsafe(no_mangle))]
pub unsafe extern "C" fn styx_control_set(
    stream: Handle,
    id: u32,
    value: *const ControlValueC,
    sink: MessageSink,
) -> Status {
    guard(|| {
        // SAFETY: null or holding its `struct_size` by the caller's contract.
        if !unsafe { sized(value) }.is_ok() {
            return Status::INVALID_ARGUMENT;
        }
        // SAFETY: checked above: not null, and as large as `ControlValueC`.
        let v = unsafe { value.read() };
        let v = match v.kind {
            CONTROL_BOOL => ControlValue::Bool(v.integer != 0),
            CONTROL_INT => match i32::try_from(v.integer) {
                Ok(i) => ControlValue::Int(i),
                Err(_) => return Status::from_errno(34),
            },
            CONTROL_UINT => match u32::try_from(v.integer) {
                Ok(u) => ControlValue::Uint(u),
                Err(_) => return Status::from_errno(34),
            },
            CONTROL_FLOAT => ControlValue::Float(v.real as f32),
            _ => return Status::INVALID_ARGUMENT,
        };
        with_stream(stream, |s| match s.frames.set_control(ControlId(id), v) {
            Ok(()) => Status::OK,
            Err(e) => {
                sink.write_fmt(format_args!("{e}"));
                Status::INVALID_ARGUMENT
            }
        })
    })
}
