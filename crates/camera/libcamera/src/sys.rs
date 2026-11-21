//! libcamera's C API as the backend uses it: lit-robotics libcamera-rs's `libcamera-sys` (its
//! C wrapper over libcamera's C++ API, bindings generated against the installed libcamera), under
//! the short names the backend calls, and the control ids of the installed libcamera from the
//! `libcamera` crate. Styx carries no C or C++ of its own for libcamera. Nothing here is safe:
//! [`crate::backend`] wraps each pointer in an owner that frees it exactly once.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_uint;

use libcamera::controls::ControlId as Id;
use libcamera::properties::PropertyId;
pub use libcamera_sys::{
    libcamera_camera_acquire, libcamera_camera_configuration_at,
    libcamera_camera_configuration_destroy, libcamera_camera_configuration_size,
    libcamera_camera_configuration_validate, libcamera_camera_configure, libcamera_camera_controls,
    libcamera_camera_create_request, libcamera_camera_destroy,
    libcamera_camera_generate_configuration, libcamera_camera_id, libcamera_camera_list_destroy,
    libcamera_camera_list_get, libcamera_camera_list_size, libcamera_camera_manager_cameras,
    libcamera_camera_manager_create, libcamera_camera_manager_destroy,
    libcamera_camera_manager_get_id, libcamera_camera_manager_start, libcamera_camera_properties,
    libcamera_camera_queue_request, libcamera_camera_release,
    libcamera_camera_request_completed_connect, libcamera_camera_request_completed_disconnect,
    libcamera_camera_start, libcamera_camera_stop, libcamera_control_from_id,
    libcamera_control_id_enumerators_iter_create, libcamera_control_id_enumerators_iter_destroy,
    libcamera_control_id_enumerators_iter_has_next, libcamera_control_id_enumerators_iter_key,
    libcamera_control_id_enumerators_iter_next, libcamera_control_id_enumerators_iter_value,
    libcamera_control_id_is_input, libcamera_control_id_type, libcamera_control_info_def,
    libcamera_control_info_map_iter_create, libcamera_control_info_map_iter_destroy,
    libcamera_control_info_map_iter_has_next, libcamera_control_info_map_iter_key,
    libcamera_control_info_map_iter_next, libcamera_control_info_map_iter_value,
    libcamera_control_info_max, libcamera_control_info_min, libcamera_control_info_values,
    libcamera_control_list_create, libcamera_control_list_destroy, libcamera_control_list_get,
    libcamera_control_list_set, libcamera_control_name, libcamera_control_value_create,
    libcamera_control_value_destroy, libcamera_control_value_get,
    libcamera_control_value_num_elements, libcamera_control_value_set,
    libcamera_control_value_size, libcamera_framebuffer_allocator_allocate,
    libcamera_framebuffer_allocator_buffers, libcamera_framebuffer_allocator_create,
    libcamera_framebuffer_allocator_destroy, libcamera_framebuffer_allocator_free,
    libcamera_framebuffer_list_get, libcamera_framebuffer_list_size,
    libcamera_framebuffer_plane_fd, libcamera_framebuffer_plane_length,
    libcamera_framebuffer_plane_offset, libcamera_framebuffer_plane_offset_valid,
    libcamera_framebuffer_planes, libcamera_framebuffer_planes_at,
    libcamera_framebuffer_planes_destroy, libcamera_framebuffer_planes_size,
    libcamera_pixel_formats_destroy, libcamera_pixel_formats_get, libcamera_pixel_formats_size,
    libcamera_request_add_buffer, libcamera_request_controls, libcamera_request_cookie,
    libcamera_request_destroy, libcamera_request_metadata, libcamera_request_reuse,
    libcamera_request_sequence, libcamera_request_status, libcamera_sizes_at,
    libcamera_sizes_destroy, libcamera_sizes_size, libcamera_stream_configuration_formats,
    libcamera_stream_configuration_stream, libcamera_stream_formats_pixel_formats,
    libcamera_stream_formats_sizes,
};
use libcamera_sys::{
    libcamera_camera_configuration_status, libcamera_control_type, libcamera_request_reuse_flag,
    libcamera_request_status as request_status, libcamera_stream_role,
};

pub type PixelFormat = libcamera_sys::libcamera_pixel_format_t;
pub type Size = libcamera_sys::libcamera_size_t;

pub type Manager = libcamera_sys::libcamera_camera_manager_t;
pub type Camera = libcamera_sys::libcamera_camera_t;
pub type CameraList = libcamera_sys::libcamera_camera_list_t;
pub type CameraConfiguration = libcamera_sys::libcamera_camera_configuration_t;
pub type StreamConfiguration = libcamera_sys::libcamera_stream_configuration_t;
pub type StreamFormats = libcamera_sys::libcamera_stream_formats_t;
pub type Stream = libcamera_sys::libcamera_stream_t;
pub type PixelFormats = libcamera_sys::libcamera_pixel_formats_t;
pub type Sizes = libcamera_sys::libcamera_sizes_t;
pub type Allocator = libcamera_sys::libcamera_framebuffer_allocator_t;
pub type FrameBuffer = libcamera_sys::libcamera_framebuffer_t;
pub type FrameBufferList = libcamera_sys::libcamera_framebuffer_list_t;
pub type FrameBufferPlanes = libcamera_sys::libcamera_framebuffer_planes_t;
pub type FrameBufferPlane = libcamera_sys::libcamera_framebuffer_plane_t;
pub type Request = libcamera_sys::libcamera_request_t;
pub type ControlList = libcamera_sys::libcamera_control_list_t;
pub type ControlValue = libcamera_sys::libcamera_control_value_t;
pub type ControlInfo = libcamera_sys::libcamera_control_info_t;
pub type ControlInfoMap = libcamera_sys::libcamera_control_info_map_t;
pub type ControlInfoMapIter = libcamera_sys::libcamera_control_info_map_iter_t;
pub type ControlId = libcamera_sys::libcamera_control_id_t;
pub type ControlIdEnumeratorsIter = libcamera_sys::libcamera_control_id_enumerators_iter_t;
pub type CallbackHandle = libcamera_sys::libcamera_callback_handle_t;

pub type Role = libcamera_stream_role::Type;
pub const ROLE_VIDEO_RECORDING: Role = libcamera_stream_role::LIBCAMERA_STREAM_ROLE_VIDEO_RECORDING;
pub const ROLE_VIEW_FINDER: Role = libcamera_stream_role::LIBCAMERA_STREAM_ROLE_VIEW_FINDER;

pub type ConfigStatus = libcamera_camera_configuration_status::Type;
pub const CONFIG_VALID: ConfigStatus =
    libcamera_camera_configuration_status::LIBCAMERA_CAMERA_CONFIGURATION_STATUS_VALID;
pub const CONFIG_ADJUSTED: ConfigStatus =
    libcamera_camera_configuration_status::LIBCAMERA_CAMERA_CONFIGURATION_STATUS_ADJUSTED;

pub const REUSE_BUFFERS: libcamera_request_reuse_flag::Type =
    libcamera_request_reuse_flag::LIBCAMERA_REQUEST_REUSE_FLAG_REUSE_BUFFERS;

pub type ControlType = libcamera_control_type::Type;
pub const CONTROL_TYPE_BOOL: ControlType = libcamera_control_type::LIBCAMERA_CONTROL_TYPE_BOOL;
pub const CONTROL_TYPE_INT32: ControlType = libcamera_control_type::LIBCAMERA_CONTROL_TYPE_INT32;
pub const CONTROL_TYPE_INT64: ControlType = libcamera_control_type::LIBCAMERA_CONTROL_TYPE_INT64;
pub const CONTROL_TYPE_FLOAT: ControlType = libcamera_control_type::LIBCAMERA_CONTROL_TYPE_FLOAT;
pub const CONTROL_TYPE_RECTANGLE: ControlType =
    libcamera_control_type::LIBCAMERA_CONTROL_TYPE_RECTANGLE;

pub type RequestStatus = request_status::Type;
pub const REQUEST_STATUS_COMPLETE: RequestStatus =
    request_status::LIBCAMERA_REQUEST_STATUS_COMPLETE;
pub const REQUEST_STATUS_CANCELLED: RequestStatus =
    request_status::LIBCAMERA_REQUEST_STATUS_CANCELLED;

// The stream configuration is the C wrapper's `#[repr(C)]` view of libcamera's own struct (the
// wrapper's header checks every field offset against libcamera's C++ type), so its fields are read
// and written in place.

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_fourcc(config: *const StreamConfiguration) -> u32 {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).pixel_format.fourcc }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_modifier(config: *const StreamConfiguration) -> u64 {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).pixel_format.modifier }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_width(config: *const StreamConfiguration) -> c_uint {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).size.width }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_height(config: *const StreamConfiguration) -> c_uint {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).size.height }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_stride(config: *const StreamConfiguration) -> c_uint {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).stride }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_frame_size(config: *const StreamConfiguration) -> c_uint {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).frame_size }
}

/// # Safety
/// `config` points at a live stream configuration.
pub unsafe fn styx_stream_buffer_count(config: *const StreamConfiguration) -> c_uint {
    // SAFETY: the caller's contract; a field read.
    unsafe { (*config).buffer_count }
}

/// # Safety
/// `config` points at a live stream configuration no one else is reading.
pub unsafe fn styx_stream_set_format(
    config: *mut StreamConfiguration,
    fourcc: u32,
    modifier: u64,
    width: c_uint,
    height: c_uint,
) {
    // SAFETY: the caller's contract; field writes.
    unsafe {
        (*config).pixel_format = PixelFormat { fourcc, modifier };
        (*config).size = Size { width, height };
    }
}

/// # Safety
/// `config` points at a live stream configuration no one else is reading.
pub unsafe fn styx_stream_set_buffer_count(config: *mut StreamConfiguration, count: c_uint) {
    // SAFETY: the caller's contract; a field write.
    unsafe { (*config).buffer_count = count }
}

/// Control ids by Styx's name: 0 ExposureTime, 1 AnalogueGain, 2 DigitalGain, 3 SensorTimestamp,
/// 4 AeEnable, 5 FrameDuration. Any other value is 0 (no control).
pub fn styx_control_id(which: c_uint) -> c_uint {
    let id = match which {
        0 => Id::ExposureTime,
        1 => Id::AnalogueGain,
        2 => Id::DigitalGain,
        3 => Id::SensorTimestamp,
        4 => Id::AeEnable,
        5 => Id::FrameDuration,
        _ => return 0,
    };
    id.into()
}

/// Property ids by Styx's name: 0 ScalerCropMaximum. Any other value is 0.
pub fn styx_property_id(which: c_uint) -> c_uint {
    match which {
        0 => PropertyId::ScalerCropMaximum.into(),
        _ => 0,
    }
}
