//! Raw declarations of the libcamera C shim (`c_api/`, vendored from libcamera-sys 0.7) and
//! Styx's accessors (`c_api/styx_shim.h`). Only what the backend calls. Nothing here is safe:
//! [`crate::backend`] wraps each pointer in an owner that frees it exactly once.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixelFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Size {
    pub width: c_uint,
    pub height: c_uint,
}

pub const ROLE_VIDEO_RECORDING: c_int = 2;
pub const ROLE_VIEW_FINDER: c_int = 3;

pub const CONFIG_VALID: c_int = 0;
pub const CONFIG_ADJUSTED: c_int = 1;

pub const REUSE_BUFFERS: c_int = 1;

pub const CONTROL_TYPE_BOOL: c_uint = 1;
pub const CONTROL_TYPE_INT32: c_uint = 5;
pub const CONTROL_TYPE_INT64: c_uint = 6;
pub const CONTROL_TYPE_FLOAT: c_uint = 7;

pub const REQUEST_STATUS_COMPLETE: c_int = 1;
pub const REQUEST_STATUS_CANCELLED: c_int = 2;

pub enum Manager {}
pub enum Camera {}
pub enum CameraList {}
pub enum CameraConfiguration {}
pub enum StreamConfiguration {}
pub enum StreamFormats {}
pub enum Stream {}
pub enum PixelFormats {}
pub enum Sizes {}
pub enum Allocator {}
pub enum FrameBuffer {}
pub enum FrameBufferList {}
pub enum FrameBufferPlanes {}
pub enum FrameBufferPlane {}
pub enum Request {}
pub enum ControlList {}
pub enum ControlValue {}
pub enum CallbackHandle {}

pub type RequestCompleted = unsafe extern "C" fn(*mut c_void, *mut Request);

unsafe extern "C" {
    // Camera manager.
    pub fn libcamera_camera_manager_create() -> *mut Manager;
    pub fn libcamera_camera_manager_destroy(mgr: *mut Manager);
    pub fn libcamera_camera_manager_start(mgr: *mut Manager) -> c_int;
    pub fn libcamera_camera_manager_stop(mgr: *mut Manager);
    pub fn libcamera_camera_manager_cameras(mgr: *const Manager) -> *mut CameraList;
    pub fn libcamera_camera_manager_get_id(mgr: *mut Manager, id: *const c_char) -> *mut Camera;
    pub fn libcamera_camera_list_destroy(list: *mut CameraList);
    pub fn libcamera_camera_list_size(list: *mut CameraList) -> usize;
    pub fn libcamera_camera_list_get(list: *mut CameraList, index: usize) -> *mut Camera;

    // Camera.
    pub fn libcamera_camera_destroy(cam: *mut Camera);
    pub fn libcamera_camera_id(cam: *const Camera) -> *const c_char;
    pub fn libcamera_camera_acquire(cam: *mut Camera) -> c_int;
    pub fn libcamera_camera_release(cam: *mut Camera) -> c_int;
    pub fn libcamera_camera_generate_configuration(
        cam: *mut Camera,
        roles: *const c_int,
        role_count: usize,
    ) -> *mut CameraConfiguration;
    pub fn libcamera_camera_configuration_destroy(config: *mut CameraConfiguration);
    pub fn libcamera_camera_configuration_size(config: *const CameraConfiguration) -> usize;
    pub fn libcamera_camera_configuration_at(
        config: *mut CameraConfiguration,
        index: usize,
    ) -> *mut StreamConfiguration;
    pub fn libcamera_camera_configuration_validate(config: *mut CameraConfiguration) -> c_int;
    pub fn libcamera_camera_configure(cam: *mut Camera, config: *mut CameraConfiguration) -> c_int;
    pub fn libcamera_camera_create_request(cam: *mut Camera, cookie: u64) -> *mut Request;
    pub fn libcamera_camera_queue_request(cam: *mut Camera, request: *mut Request) -> c_int;
    pub fn libcamera_camera_start(cam: *mut Camera, controls: *const ControlList) -> c_int;
    pub fn libcamera_camera_stop(cam: *mut Camera) -> c_int;
    pub fn libcamera_camera_request_completed_connect(
        cam: *mut Camera,
        callback: RequestCompleted,
        data: *mut c_void,
    ) -> *mut CallbackHandle;
    pub fn libcamera_camera_request_completed_disconnect(
        cam: *mut Camera,
        handle: *mut CallbackHandle,
    );

    // Stream configuration and formats.
    pub fn libcamera_stream_configuration_formats(
        config: *const StreamConfiguration,
    ) -> *const StreamFormats;
    pub fn libcamera_stream_configuration_stream(config: *const StreamConfiguration)
    -> *mut Stream;
    pub fn libcamera_stream_formats_pixel_formats(
        formats: *const StreamFormats,
    ) -> *mut PixelFormats;
    pub fn libcamera_stream_formats_sizes(
        formats: *const StreamFormats,
        pixel_format: *const PixelFormat,
    ) -> *mut Sizes;
    pub fn libcamera_pixel_formats_destroy(formats: *mut PixelFormats);
    pub fn libcamera_pixel_formats_size(formats: *const PixelFormats) -> usize;
    pub fn libcamera_pixel_formats_get(formats: *const PixelFormats, index: usize) -> PixelFormat;
    pub fn libcamera_sizes_destroy(sizes: *mut Sizes);
    pub fn libcamera_sizes_size(sizes: *const Sizes) -> usize;
    pub fn libcamera_sizes_at(sizes: *const Sizes, index: usize) -> *const Size;

    // Frame buffers.
    pub fn libcamera_framebuffer_allocator_create(cam: *mut Camera) -> *mut Allocator;
    pub fn libcamera_framebuffer_allocator_destroy(alloc: *mut Allocator);
    pub fn libcamera_framebuffer_allocator_allocate(
        alloc: *mut Allocator,
        stream: *mut Stream,
    ) -> c_int;
    pub fn libcamera_framebuffer_allocator_free(
        alloc: *mut Allocator,
        stream: *mut Stream,
    ) -> c_int;
    pub fn libcamera_framebuffer_allocator_buffers(
        alloc: *mut Allocator,
        stream: *mut Stream,
    ) -> *const FrameBufferList;
    pub fn libcamera_framebuffer_list_size(list: *const FrameBufferList) -> usize;
    pub fn libcamera_framebuffer_list_get(
        list: *const FrameBufferList,
        index: usize,
    ) -> *const FrameBuffer;
    pub fn libcamera_framebuffer_planes(fb: *const FrameBuffer) -> *mut FrameBufferPlanes;
    pub fn libcamera_framebuffer_planes_destroy(planes: *mut FrameBufferPlanes);
    pub fn libcamera_framebuffer_planes_size(planes: *const FrameBufferPlanes) -> usize;
    pub fn libcamera_framebuffer_planes_at(
        planes: *mut FrameBufferPlanes,
        index: usize,
    ) -> *mut FrameBufferPlane;
    pub fn libcamera_framebuffer_plane_fd(plane: *mut FrameBufferPlane) -> c_int;
    pub fn libcamera_framebuffer_plane_offset(plane: *const FrameBufferPlane) -> usize;
    pub fn libcamera_framebuffer_plane_length(plane: *const FrameBufferPlane) -> usize;
    pub fn libcamera_framebuffer_plane_offset_valid(plane: *const FrameBufferPlane) -> bool;

    // Requests.
    pub fn libcamera_request_destroy(request: *mut Request);
    pub fn libcamera_request_controls(request: *mut Request) -> *mut ControlList;
    pub fn libcamera_request_metadata(request: *mut Request) -> *const ControlList;
    pub fn libcamera_request_add_buffer(
        request: *mut Request,
        stream: *const Stream,
        buffer: *mut FrameBuffer,
    ) -> c_int;
    pub fn libcamera_request_find_buffer(
        request: *const Request,
        stream: *const Stream,
    ) -> *mut FrameBuffer;
    pub fn libcamera_request_sequence(request: *const Request) -> u32;
    pub fn libcamera_request_cookie(request: *const Request) -> u64;
    pub fn libcamera_request_status(request: *const Request) -> c_int;
    pub fn libcamera_request_reuse(request: *mut Request, flags: c_int);

    // Controls.
    pub fn libcamera_control_list_create() -> *mut ControlList;
    pub fn libcamera_control_list_destroy(list: *mut ControlList);
    pub fn libcamera_control_list_set(
        list: *mut ControlList,
        id: c_uint,
        value: *const ControlValue,
    );
    pub fn libcamera_control_list_get(list: *const ControlList, id: c_uint) -> *const ControlValue;
    pub fn libcamera_control_value_create() -> *mut ControlValue;
    pub fn libcamera_control_value_destroy(value: *mut ControlValue);
    pub fn libcamera_control_value_set(
        value: *mut ControlValue,
        kind: c_uint,
        data: *const c_void,
        is_array: bool,
        count: usize,
    );
    pub fn libcamera_control_value_num_elements(value: *const ControlValue) -> usize;
    pub fn libcamera_control_value_get(value: *const ControlValue) -> *const c_void;

    // Styx accessors (c_api/styx_shim.cpp).
    pub fn styx_stream_fourcc(config: *const StreamConfiguration) -> u32;
    pub fn styx_stream_modifier(config: *const StreamConfiguration) -> u64;
    pub fn styx_stream_width(config: *const StreamConfiguration) -> c_uint;
    pub fn styx_stream_height(config: *const StreamConfiguration) -> c_uint;
    pub fn styx_stream_stride(config: *const StreamConfiguration) -> c_uint;
    pub fn styx_stream_frame_size(config: *const StreamConfiguration) -> c_uint;
    pub fn styx_stream_buffer_count(config: *const StreamConfiguration) -> c_uint;
    pub fn styx_stream_set_format(
        config: *mut StreamConfiguration,
        fourcc: u32,
        modifier: u64,
        width: c_uint,
        height: c_uint,
    );
    pub fn styx_stream_set_buffer_count(config: *mut StreamConfiguration, count: c_uint);
    pub fn styx_control_id(which: c_uint) -> c_uint;
}
