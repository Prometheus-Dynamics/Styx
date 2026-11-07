//! The surface as the wrapper calls it: this crate's own exports ([`crate::surface`]) in a static
//! build, and with `shared` the installed `libstyx_capi.so.1`, linked by the normal dynamic
//! linker (never dlopen). A process holds the surface once: with `shared` this crate exports
//! nothing and the handle tables are the library's.

#[cfg(not(feature = "shared"))]
pub(crate) use crate::surface::{
    styx_camera_add_virtual, styx_camera_count, styx_camera_name, styx_check_version,
    styx_control_get, styx_control_set, styx_frame_buffer, styx_frame_plane, styx_frame_release,
    styx_stream_close, styx_stream_next, styx_stream_open,
};

#[cfg(feature = "shared")]
pub(crate) use shared::*;

#[cfg(feature = "shared")]
mod shared {
    use lemnos_abi::{AbiVersion, BufferDesc, Handle, MessageSink, Status};

    use crate::surface::{ControlValueC, FrameInfo, StreamRequest};

    // The declarations of include/styx.h.
    #[link(name = "libstyx_capi.so.1", kind = "dylib", modifiers = "+verbatim")]
    unsafe extern "C" {
        pub(crate) unsafe fn styx_check_version(caller: *const AbiVersion) -> Status;
        pub(crate) safe fn styx_camera_count() -> Status;
        pub(crate) unsafe fn styx_camera_add_virtual(
            name: *const core::ffi::c_char,
            fourcc: u32,
            width: u32,
            height: u32,
            fps: u32,
        ) -> Status;
        pub(crate) unsafe fn styx_camera_name(
            index: u32,
            name: *mut u8,
            capacity: usize,
            len: *mut usize,
        ) -> Status;
        pub(crate) unsafe fn styx_stream_open(
            index: u32,
            request: *const StreamRequest,
            stream: *mut Handle,
            sink: MessageSink,
        ) -> Status;
        pub(crate) safe fn styx_stream_close(stream: Handle) -> Status;
        pub(crate) unsafe fn styx_stream_next(
            stream: Handle,
            timeout_ms: u32,
            frame: *mut Handle,
            info: *mut FrameInfo,
        ) -> Status;
        pub(crate) unsafe fn styx_frame_buffer(frame: Handle, desc: *mut BufferDesc) -> Status;
        pub(crate) unsafe fn styx_frame_plane(
            frame: Handle,
            plane: u32,
            data: *mut *const u8,
            len: *mut u64,
            stride: *mut u64,
        ) -> Status;
        pub(crate) safe fn styx_frame_release(frame: Handle) -> Status;
        pub(crate) unsafe fn styx_control_get(
            stream: Handle,
            id: u32,
            value: *mut ControlValueC,
            sink: MessageSink,
        ) -> Status;
        pub(crate) unsafe fn styx_control_set(
            stream: Handle,
            id: u32,
            value: *const ControlValueC,
            sink: MessageSink,
        ) -> Status;
    }
}
