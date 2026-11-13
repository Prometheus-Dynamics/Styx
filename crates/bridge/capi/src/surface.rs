//! The C surface: every function `include/styx.h` declares, and nothing else exported.
//!
//! Each entry point runs inside `status::guard`: a panic is `STYX_E_PANIC`, a failure is its
//! status and a message for `styx_last_error`. Each argument is checked where it can be (NULL,
//! lengths, text, enum values, index ranges); a dangling pointer cannot be, and the header says
//! so. Results go through out-pointers, written only on success.

use std::ffi::{OsStr, c_char};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use styx_controls::{Control, ControlValue};

use crate::camera::{Camera, OpenOptions};
use crate::frame::{Frame, FrameInfo, PlaneOut};
use crate::source::Source;
use crate::status::{
    self, E_INVALID, E_NOT_FOUND, E_TIMEOUT, E_UNSUPPORTED, Fail, OK, Res, bytes, guard, handle,
    handle_mut, put, text, write_bytes,
};

/// The surface's version. A caller built against a different major refuses to run.
pub const ABI_MAJOR: u32 = 1;
/// The surface's minor version: additions only within a major.
pub const ABI_MINOR: u32 = 0;

/// A control value's kinds, as `StyxControlValue.kind`.
pub const VALUE_AUTO: u32 = 0;
pub const VALUE_MANUAL: u32 = 1;
pub const VALUE_FLAG: u32 = 2;

/// A control value as the header's `styx_control_value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct StyxControlValue {
    pub kind: u32,
    pub value: u32,
}

/// The open options as the header's `styx_open_options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct StyxOpenOptions {
    pub width: u32,
    pub height: u32,
    pub fps_milli: u32,
    pub exposure_us: u32,
    pub gain_milli: u32,
}

impl From<StyxOpenOptions> for OpenOptions {
    fn from(options: StyxOpenOptions) -> Self {
        Self {
            width: options.width,
            height: options.height,
            fps_milli: options.fps_milli,
            exposure_us: options.exposure_us,
            gain_milli: options.gain_milli,
        }
    }
}

/// A caller's path, as bytes that name a file.
///
/// # Safety
///
/// As [`status::bytes`].
unsafe fn path(ptr: *const u8, len: usize) -> Res<PathBuf> {
    // SAFETY: forwarded to `bytes` under the same contract.
    let raw = unsafe { bytes(ptr, len)? };
    if raw.is_empty() {
        return Err(Fail::invalid("an empty path"));
    }
    Ok(PathBuf::from(OsStr::from_bytes(raw)))
}

fn control_named(name: &str) -> Res<Control> {
    Control::ALL
        .into_iter()
        .find(|c| c.name() == name)
        .ok_or_else(|| Fail::new(E_NOT_FOUND, format!("no control named {name}")))
}

fn value_from_c(value: StyxControlValue) -> Res<ControlValue> {
    match value.kind {
        VALUE_AUTO => Ok(ControlValue::Auto),
        VALUE_MANUAL => Ok(ControlValue::Manual(value.value)),
        VALUE_FLAG => match value.value {
            0 => Ok(ControlValue::Flag(false)),
            1 => Ok(ControlValue::Flag(true)),
            _ => Err(Fail::invalid("a flag value is 0 or 1")),
        },
        _ => Err(Fail::invalid("an unknown control value kind")),
    }
}

fn value_to_c(value: ControlValue) -> Res<StyxControlValue> {
    match value {
        ControlValue::Auto => Ok(StyxControlValue {
            kind: VALUE_AUTO,
            value: 0,
        }),
        ControlValue::Manual(v) => Ok(StyxControlValue {
            kind: VALUE_MANUAL,
            value: v,
        }),
        ControlValue::Flag(on) => Ok(StyxControlValue {
            kind: VALUE_FLAG,
            value: u32::from(on),
        }),
        ControlValue::Crop(_) => Err(Fail::new(
            E_UNSUPPORTED,
            "a crop is not read through the C surface",
        )),
    }
}

/// Stores a new handle for `value` through `out`, refusing a NULL `out` before anything is made.
///
/// # Safety
///
/// `out` is NULL or writable for a pointer.
unsafe fn store<T>(out: *mut *mut T, value: T) -> Res<()> {
    if out.is_null() {
        return Err(Fail::invalid("NULL output handle pointer"));
    }
    // SAFETY: non-NULL and writable for a pointer by the contract.
    unsafe { put(out, Box::into_raw(Box::new(value))) }
}

/// Reports the version: `major` and `minor` of this library's surface.
///
/// # Safety
///
/// `major` and `minor` are NULL or point to writable `uint32_t`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_version(major: *mut u32, minor: *mut u32) -> i32 {
    guard(|| {
        // SAFETY: the out-pointers are NULL or writable, per the contract above.
        unsafe {
            put(major, ABI_MAJOR)?;
            put(minor, ABI_MINOR)
        }
    })
}

/// The name of a status, as a static NUL-terminated string (NULL for an unknown status).
#[unsafe(no_mangle)]
pub extern "C" fn styx_status_name(status: i32) -> *const c_char {
    let name: &'static [u8] = match status {
        OK => b"ok\0",
        E_INVALID => b"invalid argument\0",
        E_NOT_FOUND => b"not found\0",
        status::E_IO => b"i/o failure\0",
        E_TIMEOUT => b"timeout\0",
        E_UNSUPPORTED => b"unsupported\0",
        status::E_BUSY => b"busy or not publishing\0",
        status::E_PROTOCOL => b"protocol mismatch\0",
        status::E_PANIC => b"internal failure\0",
        status::E_STOPPED => b"stopped\0",
        status::E_RANGE => b"value out of range\0",
        _ => return std::ptr::null(),
    };
    name.as_ptr().cast()
}

/// Copies this thread's last failure message into `buf` (see `write_bytes` in the header: the
/// full length goes to `len`, the text is cut to `cap - 1` and NUL-terminated).
///
/// # Safety
///
/// `buf` has `cap` writable bytes (or is NULL with `cap` zero); `len` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_last_error(buf: *mut u8, cap: usize, len: *mut usize) -> i32 {
    guard(|| {
        status::check_text_out(buf, cap, len)?;
        let message = status::last_error();
        // SAFETY: buf and len are as the contract above says.
        unsafe { write_bytes(message.as_bytes(), buf, cap, len) }
    })
}

/// Opens the in-process source: cameras on the Lemnos daemon at `path`, chosen by `styx-select`.
///
/// # Safety
///
/// `path` has `len` readable bytes (NULL only when `len` is zero); `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_source_local(
    path_ptr: *const u8,
    len: usize,
    out: *mut *mut Source,
) -> i32 {
    guard(|| {
        // SAFETY: the path bytes and out-pointer are as the contract above says.
        let lemnos = unsafe { path(path_ptr, len)? };
        // SAFETY: as above.
        unsafe { store(out, Source::local(lemnos)) }
    })
}

/// Opens the service source: the cameras `styx-service` serves on the control socket at `path`.
///
/// # Safety
///
/// `path` has `len` readable bytes (NULL only when `len` is zero); `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_source_service(
    path_ptr: *const u8,
    len: usize,
    out: *mut *mut Source,
) -> i32 {
    guard(|| {
        // SAFETY: as in `styx_source_local`.
        let control = unsafe { path(path_ptr, len)? };
        // SAFETY: as in `styx_source_local`.
        unsafe { store(out, Source::service(control)) }
    })
}

/// Frees a source. NULL is a no-op. Cameras opened from it stay open.
///
/// # Safety
///
/// `source` is NULL or a source this library returned, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_source_close(source: *mut Source) {
    let _ = guard(|| {
        if !source.is_null() {
            // SAFETY: a source this library made with Box::into_raw, freed once (the contract).
            drop(unsafe { Box::from_raw(source) });
        }
        Ok(())
    });
}

/// The number of cameras the source lists now.
///
/// # Safety
///
/// `source` is a live source; `count` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_source_camera_count(source: *const Source, count: *mut usize) -> i32 {
    guard(|| {
        if count.is_null() {
            return Err(Fail::invalid("NULL count output"));
        }
        // SAFETY: a live source, per the contract.
        let source = unsafe { handle(source)? };
        let n = source.cameras()?.len();
        // SAFETY: count is non-NULL and writable, per the contract.
        unsafe { put(count, n) }
    })
}

/// The name of camera `index` in the source's current listing. Each call lists again, so a
/// caller that asks for every name lists once per name.
///
/// # Safety
///
/// `source` is a live source; `buf` has `cap` writable bytes (NULL only when `cap` is zero);
/// `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_source_camera_name(
    source: *const Source,
    index: usize,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> i32 {
    guard(|| {
        status::check_text_out(buf, cap, len)?;
        // SAFETY: a live source, per the contract.
        let source = unsafe { handle(source)? };
        let cameras = source.cameras()?;
        let camera = cameras
            .get(index)
            .ok_or_else(|| Fail::new(E_NOT_FOUND, "no camera at that index"))?;
        // SAFETY: buf and len are as the contract says.
        unsafe { write_bytes(camera.name.as_bytes(), buf, cap, len) }
    })
}

/// Opens camera `name` from `source`. `options` is NULL for the defaults. The in-process path
/// needs `fps_milli`; over the service the service's config decides and the options are ignored.
///
/// # Safety
///
/// `source` is a live source; `name` has `name_len` readable bytes; `options` is NULL or points
/// to an options struct; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_camera_open(
    source: *const Source,
    name_ptr: *const u8,
    name_len: usize,
    options: *const StyxOpenOptions,
    out: *mut *mut Camera,
) -> i32 {
    guard(|| {
        // SAFETY: a live source, readable name bytes, and an options struct or NULL, per the
        // contract.
        let source = unsafe { handle(source)? };
        // SAFETY: name has name_len readable bytes, per the contract.
        let name = unsafe { text(name_ptr, name_len)? };
        let options = if options.is_null() {
            OpenOptions::default()
        } else {
            // SAFETY: non-NULL and readable for one options struct.
            let given: &StyxOpenOptions = unsafe { handle(options)? };
            OpenOptions::from(*given)
        };
        let camera = Camera::open(source, name, options)?;
        // SAFETY: out is as the contract says.
        unsafe { store(out, camera) }
    })
}

/// Closes a camera: its stream stops and its device is released. NULL is a no-op.
///
/// # Safety
///
/// `camera` is NULL or a camera this library returned, not yet closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_camera_close(camera: *mut Camera) {
    let _ = guard(|| {
        if !camera.is_null() {
            // SAFETY: made with Box::into_raw, freed once (the contract).
            drop(unsafe { Box::from_raw(camera) });
        }
        Ok(())
    });
}

/// Sets control `name` (one of the names in `styx-controls`, as `exposure`) to `value`, applied
/// from frame `frame` on.
///
/// # Safety
///
/// `camera` is a live camera, not used by another thread during the call; `name` has `len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_camera_set_control(
    camera: *mut Camera,
    name_ptr: *const u8,
    len: usize,
    value: StyxControlValue,
    frame: u64,
) -> i32 {
    guard(|| {
        // SAFETY: a live camera and readable name bytes, per the contract.
        let camera = unsafe { handle_mut(camera)? };
        // SAFETY: name has len readable bytes, per the contract.
        let control = control_named(unsafe { text(name_ptr, len)? })?;
        camera.set_control(control, value_from_c(value)?, frame)
    })
}

/// Reads control `name`'s value on the camera's newest frame into `out`.
///
/// # Safety
///
/// As `styx_camera_set_control`; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_camera_get_control(
    camera: *mut Camera,
    name_ptr: *const u8,
    len: usize,
    out: *mut StyxControlValue,
) -> i32 {
    guard(|| {
        // SAFETY: as in `styx_camera_set_control`, and out is writable.
        let camera = unsafe { handle_mut(camera)? };
        // SAFETY: name has len readable bytes, per the contract.
        let control = control_named(unsafe { text(name_ptr, len)? })?;
        let value = value_to_c(camera.get_control(control)?)?;
        // SAFETY: out is writable, per the contract.
        unsafe { put(out, value) }
    })
}

/// Waits up to `timeout_ms` for the next frame. On success `*out` is a frame, held until
/// `styx_frame_release`. `E_TIMEOUT` when none came.
///
/// # Safety
///
/// `camera` is a live camera, not used by another thread during the call; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_camera_next_frame(
    camera: *mut Camera,
    timeout_ms: u32,
    out: *mut *mut Frame,
) -> i32 {
    guard(|| {
        // SAFETY: a live camera, per the contract.
        let camera = unsafe { handle_mut(camera)? };
        match camera.next_frame(Duration::from_millis(u64::from(timeout_ms)))? {
            // SAFETY: out is writable, per the contract.
            Some(frame) => unsafe { store(out, frame) },
            None => Err(Fail::new(E_TIMEOUT, "no frame within the timeout")),
        }
    })
}

/// A frame's identity, format, timing and controls.
///
/// # Safety
///
/// `frame` is a live frame; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_frame_describe(frame: *const Frame, out: *mut FrameInfo) -> i32 {
    guard(|| {
        // SAFETY: a live frame and writable out, per the contract.
        let frame = unsafe { handle(frame)? };
        // SAFETY: out is writable, per the contract.
        unsafe { put(out, frame.info()) }
    })
}

/// Plane `index` of a frame: its bytes, stride and length, valid until the frame is released.
///
/// # Safety
///
/// `frame` is a live frame; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_frame_plane(
    frame: *const Frame,
    index: u32,
    out: *mut PlaneOut,
) -> i32 {
    guard(|| {
        // SAFETY: a live frame and writable out, per the contract.
        let frame = unsafe { handle(frame)? };
        let plane = frame.plane(index as usize)?;
        // SAFETY: out is writable, per the contract.
        unsafe { put(out, plane) }
    })
}

/// The frame's metadata record, in `styx-controls` wire form (version 1, fixed length). Copied
/// into `buf` as `styx_last_error`'s message is.
///
/// # Safety
///
/// `frame` is a live frame; `buf` has `cap` writable bytes (NULL only when `cap` is zero); `len`
/// is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_frame_metadata(
    frame: *const Frame,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> i32 {
    guard(|| {
        status::check_text_out(buf, cap, len)?;
        // SAFETY: a live frame, and buf and len as the contract says.
        let frame = unsafe { handle(frame)? };
        let record = frame.metadata();
        // SAFETY: len is non-NULL and writable (checked above).
        unsafe { len.write(record.len()) };
        if cap < record.len() {
            return Err(Fail::invalid(format!(
                "the metadata record is {} bytes; the buffer has {cap}",
                record.len()
            )));
        }
        // SAFETY: buf has cap >= record.len() writable bytes (NULL only when cap is zero, which
        // the length check above refuses since the record is not empty), and record is a local.
        unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), buf, record.len()) };
        Ok(())
    })
}

/// A frame's buffer descriptor, for a zero-copy import elsewhere: `fd` (borrowed, valid until the
/// frame is released), the buffer's `offset` in it and its `len`, and its memory `kind`
/// (`STYX_MEMORY_*`). `E_UNSUPPORTED` for host memory, which has no descriptor.
///
/// # Safety
///
/// `frame` is a live frame; each out-pointer is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_frame_fd(
    frame: *const Frame,
    fd: *mut i32,
    offset: *mut u64,
    len: *mut u64,
    kind: *mut u32,
) -> i32 {
    guard(|| {
        // SAFETY: a live frame, per the contract.
        let frame = unsafe { handle(frame)? };
        let dma = frame.dma().ok_or_else(|| {
            Fail::new(E_UNSUPPORTED, "this frame has no descriptor (host memory)")
        })?;
        // SAFETY: each out-pointer is writable, per the contract.
        unsafe {
            put(fd, dma.fd)?;
            put(offset, dma.offset)?;
            put(len, dma.len)?;
            put(kind, dma.kind)
        }
    })
}

/// Gives a frame back to its capture. NULL is a no-op. The planes and the descriptor are invalid
/// after this.
///
/// # Safety
///
/// `frame` is NULL or a frame this library returned, not yet released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_frame_release(frame: *mut Frame) {
    let _ = guard(|| {
        if !frame.is_null() {
            // SAFETY: made with Box::into_raw, released once (the contract).
            drop(unsafe { Box::from_raw(frame) });
        }
        Ok(())
    });
}

/// The copies the library's frame paths have made in this process, and their bytes: zero on the
/// zero-copy paths.
///
/// # Safety
///
/// `copies` and `bytes` are NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn styx_copies(copies: *mut u64, bytes: *mut u64) -> i32 {
    guard(|| {
        let (mut n, mut total) = (0_u64, 0_u64);
        for counter in styx_frame::copy_counters() {
            n += counter.copies() as u64;
            total += counter.bytes() as u64;
        }
        // SAFETY: both out-pointers are NULL or writable, per the contract.
        unsafe {
            put(copies, n)?;
            put(bytes, total)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_values_and_names_are_checked_before_they_reach_a_camera() {
        let flag = StyxControlValue {
            kind: VALUE_FLAG,
            value: 2,
        };
        assert_eq!(value_from_c(flag).map_err(|f| f.status), Err(E_INVALID));
        let kind = StyxControlValue { kind: 9, value: 0 };
        assert_eq!(value_from_c(kind).map_err(|f| f.status), Err(E_INVALID));
        let manual = StyxControlValue {
            kind: VALUE_MANUAL,
            value: 5,
        };
        assert_eq!(value_from_c(manual).ok(), Some(ControlValue::Manual(5)));
        assert_eq!(
            control_named("nope").map_err(|f| f.status),
            Err(E_NOT_FOUND)
        );
        assert_eq!(control_named("exposure").ok(), Some(Control::Exposure));
        assert_eq!(
            value_to_c(ControlValue::Crop(None)).map_err(|f| f.status),
            Err(E_UNSUPPORTED)
        );
    }
}
