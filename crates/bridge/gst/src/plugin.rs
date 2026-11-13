//! The GStreamer plugin: the descriptor GStreamer looks up (`gst_plugin_desc`), and the functions
//! the C element calls for each frame. Only built with the `gstreamer` feature and the libraries.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::time::Duration;

use crate::source::{Delivered, Source};

/// One frame as the C element reads it. Mirrors `styx_gst_frame` in `c_api/styx_gst.c`.
#[repr(C)]
pub struct GstFrame {
    data: *const u8,
    len: usize,
    fd: c_int,
    format: *const c_char,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    pts_ns: u64,
    planes: u32,
    offset: [usize; 4],
    stride: [i32; 4],
    token: *mut c_void,
}

/// The functions the element calls. Mirrors `styx_gst_ops` in `c_api/styx_gst.c`.
#[repr(C)]
pub struct Ops {
    open: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    close: unsafe extern "C" fn(*mut c_void),
    next: unsafe extern "C" fn(*mut c_void, *mut GstFrame, u32) -> c_int,
    release: unsafe extern "C" fn(*mut c_void),
}

// The element's C code reads this table from its streaming threads; it is never written.
static OPS: Ops = Ops {
    open: open_source,
    close: close_source,
    next: next_frame,
    release: release_frame,
};

unsafe extern "C" {
    fn styx_gst_plugin_init(plugin: *mut c_void, ops: *const Ops) -> c_int;
}

/// GStreamer's plugin descriptor (`struct _GstPluginDesc`, gstplugin.h, GStreamer 1.28).
#[repr(C)]
pub struct PluginDesc {
    major_version: c_int,
    minor_version: c_int,
    name: *const c_char,
    description: *const c_char,
    plugin_init: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    version: *const c_char,
    license: *const c_char,
    source: *const c_char,
    package: *const c_char,
    origin: *const c_char,
    release_datetime: *const c_char,
    reserved: [*mut c_void; 4],
}

// SAFETY: every pointer is to a static C string or is null, and the table is never written, so
// reading it from any thread is sound.
unsafe impl Sync for PluginDesc {}

/// The entry GStreamer loads a plugin by.
#[unsafe(no_mangle)]
pub static gst_plugin_desc: PluginDesc = PluginDesc {
    major_version: 1,
    minor_version: 28,
    name: c"styx".as_ptr(),
    description: c"Styx frames from the shared ring".as_ptr(),
    plugin_init: Some(plugin_init),
    version: c"2.0.2-dev".as_ptr(),
    license: c"MIT".as_ptr(),
    source: c"styx-gst".as_ptr(),
    package: c"styx".as_ptr(),
    origin: c"https://github.com/Prometheus-Dynamics/Styx".as_ptr(),
    release_datetime: std::ptr::null(),
    reserved: [null_mut(); 4],
};

unsafe extern "C" fn plugin_init(plugin: *mut c_void) -> c_int {
    // SAFETY: GStreamer calls the init with the plugin object it is loading, valid for the call,
    // and the table `OPS` lives for the whole program.
    unsafe { styx_gst_plugin_init(plugin, &raw const OPS) }
}

unsafe extern "C" fn open_source(socket: *const c_char) -> *mut c_void {
    if socket.is_null() {
        return null_mut();
    }
    // SAFETY: the element passes its NUL-terminated `socket` property, valid for this call.
    let Ok(path) = unsafe { CStr::from_ptr(socket) }.to_str() else {
        return null_mut();
    };
    Box::into_raw(Box::new(Source::new(PathBuf::from(path)))).cast()
}

unsafe extern "C" fn close_source(source: *mut c_void) {
    if source.is_null() {
        return;
    }
    // SAFETY: `source` came from `open_source` and the element closes it once, on stop.
    drop(unsafe { Box::from_raw(source.cast::<Source>()) });
}

unsafe extern "C" fn next_frame(source: *mut c_void, out: *mut GstFrame, timeout_ms: u32) -> c_int {
    if source.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: `source` is the live source from `open_source`; the element calls on one thread at
    // a time (its create function), so this is the only reference to it.
    let source = unsafe { &mut *source.cast::<Source>() };
    let Some(delivered) = source.next(Duration::from_millis(u64::from(timeout_ms))) else {
        return 0;
    };
    let token = Box::into_raw(Box::new(delivered));
    // SAFETY: `token` is a fresh box, valid until `release_frame` takes it back.
    let frame = describe(unsafe { &*token }, token.cast());
    // SAFETY: `out` is the element's frame slot, valid for writes.
    unsafe { out.write(frame) };
    1
}

unsafe extern "C" fn release_frame(token: *mut c_void) {
    if token.is_null() {
        return;
    }
    // SAFETY: `token` is the box `next_frame` made; GStreamer releases each frame once, when the
    // memory that holds it is freed.
    drop(unsafe { Box::from_raw(token.cast::<Delivered>()) });
}

/// The C view of a delivered frame. The bytes and descriptor are the producer's; they stay valid
/// until `token` is released.
fn describe(delivered: &Delivered, token: *mut c_void) -> GstFrame {
    let mut frame = GstFrame {
        data: delivered
            .data()
            .map_or(std::ptr::null(), |bytes| bytes.as_ptr()),
        len: delivered.payload_len(),
        fd: delivered.raw_fd().unwrap_or(-1),
        format: delivered.format().as_ptr(),
        width: delivered.width(),
        height: delivered.height(),
        fps_num: 0,
        fps_den: 1,
        pts_ns: delivered.pts_ns(),
        planes: 0,
        offset: [0; 4],
        stride: [0; 4],
        token,
    };
    for (index, plane) in delivered.layouts().iter().take(4).enumerate() {
        frame.offset[index] = plane.offset;
        frame.stride[index] = plane.stride as i32;
        frame.planes += 1;
    }
    frame
}
