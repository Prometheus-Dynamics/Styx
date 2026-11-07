//! The Rust wrapper: every call goes through the C surface ([`crate::ffi`]), so a Rust consumer
//! links Styx through the same boundary a C one does (statically by default, or the shared
//! library with `shared`).

use core::ffi::c_void;
use core::mem::size_of;
use std::sync::OnceLock;
use std::time::Duration;

use lemnos_abi::{BufferDesc, Handle, MessageSink, Status};

use crate::ffi;
use crate::surface::{
    CONTROL_BOOL, CONTROL_FLOAT, CONTROL_INT, CONTROL_UINT, ControlValueC, FrameInfo,
    StreamRequest, VERSION,
};

/// A failed call: its status and the surface's message, if it gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub status: Status,
    pub message: String,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.message.is_empty() {
            true => write!(f, "status {}", self.status.0),
            false => write!(f, "{} (status {})", self.message, self.status.0),
        }
    }
}

impl std::error::Error for Error {}

/// The sink's callback: appends the message to the `String` behind `context`.
extern "C" fn append(context: *mut c_void, text: *const u8, len: usize) {
    // SAFETY: the surface passes `len` readable bytes at `text`, valid during this callback.
    let bytes = unsafe { core::slice::from_raw_parts(text, len) };
    // SAFETY: `context` is the `String` `call` lent for this call, not otherwise borrowed.
    let message = unsafe { &mut *context.cast::<String>() };
    message.push_str(&String::from_utf8_lossy(bytes));
}

/// Runs a surface call with a sink that collects its message.
fn call(f: impl FnOnce(MessageSink) -> Status) -> Result<u32, Error> {
    let mut message = String::new();
    let sink = MessageSink {
        context: (&raw mut message).cast(),
        write: Some(append),
    };
    let status = f(sink);
    status
        .into_result()
        .map_err(|status| Error { status, message })
}

fn plain(status: Status) -> Result<u32, Error> {
    status.into_result().map_err(|status| Error {
        status,
        message: String::new(),
    })
}

/// The version handshake, once per process.
fn handshake() -> Result<(), Error> {
    static DONE: OnceLock<Status> = OnceLock::new();
    // SAFETY: a valid version for the duration of the call.
    let status = *DONE.get_or_init(|| unsafe { ffi::styx_check_version(&VERSION) });
    plain(status).map(drop)
}

/// A camera: its index in the list [`Camera::list`] probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    index: u32,
    name: String,
}

impl Camera {
    /// Probes the cameras.
    pub fn list() -> Result<Vec<Camera>, Error> {
        handshake()?;
        let n = plain(ffi::styx_camera_count())?;
        (0..n).map(Self::at).collect()
    }

    /// Camera `index` of the last probe (or one a host added).
    pub fn at(index: u32) -> Result<Camera, Error> {
        handshake()?;
        let mut len = 0usize;
        // SAFETY: a null buffer asks for the length only; `len` is valid for the write.
        let status = unsafe { ffi::styx_camera_name(index, core::ptr::null_mut(), 0, &mut len) };
        if status != Status::BUFFER_TOO_SMALL {
            plain(status)?;
        }
        let mut buf = vec![0u8; len + 1];
        // SAFETY: `buf` holds `buf.len()` writable bytes; `len` is valid for the write.
        plain(unsafe { ffi::styx_camera_name(index, buf.as_mut_ptr(), buf.len(), &mut len) })?;
        buf.truncate(len);
        Ok(Camera {
            index,
            name: String::from_utf8_lossy(&buf).into_owned(),
        })
    }

    /// Adds a synthetic camera (a moving test pattern) for tests and demos without hardware.
    pub fn add_virtual(
        name: &str,
        fourcc: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Camera, Error> {
        handshake()?;
        let name = std::ffi::CString::new(name).map_err(|_| Error {
            status: Status::INVALID_ARGUMENT,
            message: "a NUL in the name".into(),
        })?;
        // SAFETY: `name` is NUL-terminated and lives for the call.
        let index = plain(unsafe {
            ffi::styx_camera_add_virtual(name.as_ptr(), fourcc, width, height, fps)
        })?;
        Self::at(index)
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Starts a stream: `fourcc` 0 for any format, `width`/`height` the largest frame wanted (0:
    /// any), `fps` 0 for any.
    pub fn open(&self, fourcc: u32, width: u32, height: u32, fps: u32) -> Result<Stream, Error> {
        let request = StreamRequest {
            struct_size: size_of::<StreamRequest>() as u32,
            fourcc,
            width,
            height,
            fps,
            flags: 0,
        };
        let mut handle = Handle::NULL;
        call(|sink| {
            // SAFETY: `request` and `handle` are valid for the call.
            unsafe { ffi::styx_stream_open(self.index, &request, &mut handle, sink) }
        })?;
        Ok(Stream { handle })
    }
}

/// A control's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlValue {
    Bool(bool),
    Int(i32),
    Uint(u32),
    Float(f32),
}

/// A running stream; stopped when dropped.
#[derive(Debug)]
pub struct Stream {
    handle: Handle,
}

impl Stream {
    /// The next frame within `timeout`, `None` when none came.
    pub fn next(&self, timeout: Duration) -> Result<Option<Frame>, Error> {
        let mut frame = Handle::NULL;
        let mut info = FrameInfo {
            struct_size: size_of::<FrameInfo>() as u32,
            ..FrameInfo::default()
        };
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `frame` and `info` are valid for the call.
        let status = unsafe { ffi::styx_stream_next(self.handle, ms, &mut frame, &mut info) };
        if status == Status::TIMED_OUT {
            return Ok(None);
        }
        plain(status)?;
        Ok(Some(Frame {
            handle: frame,
            info,
        }))
    }

    pub fn control(&self, id: u32) -> Result<Option<ControlValue>, Error> {
        let mut v = ControlValueC {
            struct_size: size_of::<ControlValueC>() as u32,
            ..ControlValueC::default()
        };
        call(|sink| {
            // SAFETY: `v` is valid for the call.
            unsafe { ffi::styx_control_get(self.handle, id, &mut v, sink) }
        })?;
        Ok(match v.kind {
            CONTROL_BOOL => Some(ControlValue::Bool(v.integer != 0)),
            CONTROL_INT => Some(ControlValue::Int(v.integer as i32)),
            CONTROL_UINT => Some(ControlValue::Uint(v.integer as u32)),
            CONTROL_FLOAT => Some(ControlValue::Float(v.real as f32)),
            _ => None,
        })
    }

    pub fn set_control(&self, id: u32, value: ControlValue) -> Result<(), Error> {
        let (kind, integer, real) = match value {
            ControlValue::Bool(b) => (CONTROL_BOOL, i64::from(b), 0.0),
            ControlValue::Int(i) => (CONTROL_INT, i64::from(i), 0.0),
            ControlValue::Uint(u) => (CONTROL_UINT, i64::from(u), 0.0),
            ControlValue::Float(f) => (CONTROL_FLOAT, 0, f64::from(f)),
        };
        let v = ControlValueC {
            struct_size: size_of::<ControlValueC>() as u32,
            kind,
            integer,
            real,
        };
        call(|sink| {
            // SAFETY: `v` is valid for the call.
            unsafe { ffi::styx_control_set(self.handle, id, &v, sink) }
        })
        .map(drop)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = ffi::styx_stream_close(self.handle);
    }
}

/// A frame held from its capture; given back when dropped.
#[derive(Debug)]
pub struct Frame {
    handle: Handle,
    info: FrameInfo,
}

impl Frame {
    pub fn info(&self) -> &FrameInfo {
        &self.info
    }

    /// Its dma-bufs (fds borrowed for the frame's life).
    pub fn buffer(&self) -> Result<BufferDesc, Error> {
        let mut desc = BufferDesc::image(0, 0, 0, 0);
        // SAFETY: `desc` is valid for the call, `struct_size` set by `image`.
        plain(unsafe { ffi::styx_frame_buffer(self.handle, &mut desc) })?;
        Ok(desc)
    }

    /// Plane `index`'s bytes and stride, in this process.
    pub fn plane(&self, index: u32) -> Result<(&[u8], usize), Error> {
        let (mut data, mut len, mut stride) = (core::ptr::null(), 0u64, 0u64);
        // SAFETY: the three outputs are valid for the call.
        plain(unsafe {
            ffi::styx_frame_plane(self.handle, index, &mut data, &mut len, &mut stride)
        })?;
        // SAFETY: the surface gave `len` readable bytes at `data`, held until this frame is
        // released, which `&self` outlives.
        let bytes = unsafe { core::slice::from_raw_parts(data, len as usize) };
        Ok((bytes, stride as usize))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = ffi::styx_frame_release(self.handle);
    }
}
