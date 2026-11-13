//! The libcamera binding: cameras, configuration, dma-buf frames and requests, over the C shim.
//!
//! Frames are meant to be zero-copy, and are not yet run on a camera (proof: PR H6): each libcamera
//! buffer's dma-buf is mapped once (read-only) as a [`FdMemory`] in one [`FramePool`] per stream,
//! and a completed request hands its buffer out as a [`FrameLease`]. Buffer `k` is requeued to
//! libcamera only when no lease on it is out: a parked buffer is retried by the requeue thread
//! with `acquire_at`, which reports `Held` while a consumer still holds it.

use std::ffi::{CStr, CString, c_int, c_void};
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use styx_controls::{Control, ControlValue, FrameMetadata, Update};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool,
};

use crate::controls::{self as map, TYPE_BOOL, TYPE_FLOAT, TYPE_INT32, TYPE_INT64, TYPE_RECTANGLE};
use crate::planes;
use crate::sys;
use crate::{CameraInfo, CaptureConfig, CaptureStats, DeviceEntry, Error, FrameSink, StreamFormat};
use styx_controls::{DeviceControl, DeviceKind, Rect};

/// The shim's control ids (`styx_control_id`).
const CTRL_EXPOSURE_TIME: c_int = 0;
const CTRL_ANALOGUE_GAIN: c_int = 1;
const CTRL_DIGITAL_GAIN: c_int = 2;
const CTRL_SENSOR_TIMESTAMP: c_int = 3;

fn control_id(which: c_int) -> u32 {
    // SAFETY: a pure accessor over libcamera's static control ids; no state is touched.
    unsafe { sys::styx_control_id(which as u32) }
}

/// The process-wide camera manager. libcamera allows one CameraManager per process (a second is
/// a fatal abort), so it is started once and kept for the life of the process.
struct Manager(NonNull<sys::Manager>);

// SAFETY: libcamera serialises its CameraManager calls internally; this value is only used
// through `cameras` and `camera`, which those calls make safe from any thread.
unsafe impl Send for Manager {}
// SAFETY: as for `Send`.
unsafe impl Sync for Manager {}

static MANAGER: std::sync::OnceLock<Result<Manager, String>> = std::sync::OnceLock::new();

fn manager() -> Result<&'static Manager, Error> {
    match MANAGER.get_or_init(|| Manager::start().map_err(|err| err.to_string())) {
        Ok(manager) => Ok(manager),
        Err(message) => Err(Error::Camera(message.clone())),
    }
}

impl Manager {
    fn start() -> Result<Self, Error> {
        // SAFETY: creating a manager takes no arguments and returns an owned pointer or null.
        let raw = NonNull::new(unsafe { sys::libcamera_camera_manager_create() })
            .ok_or_else(|| Error::Camera("camera manager could not be created".into()))?;
        // SAFETY: `raw` is a live manager owned by this value.
        let rc = unsafe { sys::libcamera_camera_manager_start(raw.as_ptr()) };
        if rc != 0 {
            // SAFETY: `raw` is owned here and not used again.
            unsafe { sys::libcamera_camera_manager_destroy(raw.as_ptr()) };
            return Err(Error::Camera(format!("camera manager start failed ({rc})")));
        }
        Ok(Self(raw))
    }

    fn cameras(&self) -> Vec<Camera> {
        // SAFETY: the manager is live; the list is owned here and destroyed below.
        let list = unsafe { sys::libcamera_camera_manager_cameras(self.0.as_ptr()) };
        if list.is_null() {
            return Vec::new();
        }
        // SAFETY: `list` is a live camera list owned here.
        let count = unsafe { sys::libcamera_camera_list_size(list) };
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            // SAFETY: `index` is below the list's size; each returned camera is a new owned handle.
            let cam = unsafe { sys::libcamera_camera_list_get(list, index) };
            if let Some(cam) = NonNull::new(cam) {
                out.push(Camera(cam));
            }
        }
        // SAFETY: the list was created by the shim and is not used after this.
        unsafe { sys::libcamera_camera_list_destroy(list) };
        out
    }

    fn camera(&self, id: &str) -> Option<Camera> {
        let id = CString::new(id).ok()?;
        // SAFETY: `id` is a valid NUL-terminated string for the call's duration.
        let cam = unsafe { sys::libcamera_camera_manager_get_id(self.0.as_ptr(), id.as_ptr()) };
        NonNull::new(cam).map(Camera)
    }
}

/// An owned camera handle.
struct Camera(NonNull<sys::Camera>);

impl Camera {
    fn id(&self) -> String {
        // SAFETY: the camera is live; the returned id string outlives this call only as copied here.
        unsafe {
            let raw = sys::libcamera_camera_id(self.0.as_ptr());
            if raw.is_null() {
                return String::new();
            }
            CStr::from_ptr(raw).to_string_lossy().into_owned()
        }
    }

    /// Generated configuration for `role`; the caller frees it through [`Configuration`].
    fn configuration(&self, role: c_int) -> Option<Configuration> {
        // SAFETY: the camera is live and the role array lives for the call.
        let raw =
            unsafe { sys::libcamera_camera_generate_configuration(self.0.as_ptr(), &role, 1) };
        NonNull::new(raw).map(Configuration)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and destroyed once.
        unsafe { sys::libcamera_camera_destroy(self.0.as_ptr()) };
    }
}

/// An owned camera configuration.
struct Configuration(NonNull<sys::CameraConfiguration>);

impl Configuration {
    fn stream(&mut self) -> Option<NonNull<sys::StreamConfiguration>> {
        // SAFETY: the configuration is live; index 0 is checked against the size.
        unsafe {
            if sys::libcamera_camera_configuration_size(self.0.as_ptr()) == 0 {
                return None;
            }
            NonNull::new(sys::libcamera_camera_configuration_at(self.0.as_ptr(), 0))
        }
    }
}

impl Drop for Configuration {
    fn drop(&mut self) {
        // SAFETY: the configuration is owned and destroyed once.
        unsafe { sys::libcamera_camera_configuration_destroy(self.0.as_ptr()) };
    }
}

/// The formats a camera offers for the viewfinder role, as Styx codes and sizes.
fn formats(camera: &Camera) -> Vec<StreamFormat> {
    let Some(mut config) = camera.configuration(sys::ROLE_VIEW_FINDER) else {
        return Vec::new();
    };
    let Some(stream) = config.stream() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: `stream` points into the live configuration; the formats, lists and sizes are
    // owned here and destroyed before return.
    unsafe {
        let formats = sys::libcamera_stream_configuration_formats(stream.as_ptr());
        let pixel_formats = sys::libcamera_stream_formats_pixel_formats(formats);
        for index in 0..sys::libcamera_pixel_formats_size(pixel_formats) {
            let pf = sys::libcamera_pixel_formats_get(pixel_formats, index);
            let Some(code) = styx_format::drm::from_drm(pf.fourcc, pf.modifier) else {
                continue;
            };
            let sizes = sys::libcamera_stream_formats_sizes(formats, &pf);
            for at in 0..sys::libcamera_sizes_size(sizes) {
                let size = sys::libcamera_sizes_at(sizes, at);
                if !size.is_null() {
                    out.push(StreamFormat {
                        code,
                        width: (*size).width,
                        height: (*size).height,
                    });
                }
            }
            sys::libcamera_sizes_destroy(sizes);
        }
        sys::libcamera_pixel_formats_destroy(pixel_formats);
    }
    out
}

/// Every camera libcamera lists, with the formats it offers.
pub(crate) fn list_cameras() -> Result<Vec<CameraInfo>, Error> {
    let manager = manager()?;
    Ok(manager
        .cameras()
        .iter()
        .map(|camera| CameraInfo {
            id: camera.id(),
            formats: formats(camera),
        })
        .collect())
}

/// Monotonic and boot clocks, read together: libcamera timestamps are `CLOCK_BOOTTIME`.
fn clock_ns(clock: libc::clockid_t) -> i128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for the call.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
}

/// A request's sensor timestamp (`CLOCK_BOOTTIME`, ns) as `CLOCK_MONOTONIC` ns. The
/// boot-minus-monotonic offset is sampled per frame, when the frame completes, so the mapping
/// follows any clock step at the cost of the sampling latency.
fn to_monotonic(boot_ns: i128) -> u64 {
    let offset = clock_ns(libc::CLOCK_BOOTTIME) - clock_ns(libc::CLOCK_MONOTONIC);
    (boot_ns - offset).clamp(0, u64::MAX as i128) as u64
}

/// Control values the backend applies to every request it queues, by libcamera control id.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Value {
    Bool(bool),
    I32(i32),
    I64(i64),
    F32(f32),
    /// `libcamera::Rectangle`: x, y, width, height.
    Rect([i32; 4]),
}

/// The values requested from now on: one entry per control id, replaced when set again.
type Requested = Vec<(u32, Value)>;

/// Everything the completion callback and the requeue thread share. Boxed so its address is
/// stable while libcamera holds the callback's pointer.
struct Inner {
    camera: NonNull<sys::Camera>,
    stream: NonNull<sys::Stream>,
    pool: FramePool<FrameMetadata>,
    templates: Vec<FrameEntry>,
    requests: Vec<NonNull<sys::Request>>,
    buffers: Vec<NonNull<sys::FrameBuffer>>,
    requested: Mutex<Requested>,
    parked: Mutex<Vec<usize>>,
    sink: Mutex<FrameSink>,
    stopping: AtomicBool,
    frames: AtomicU64,
    held: AtomicU64,
    dropped: AtomicU64,
    requeues: AtomicU64,
}

// SAFETY: the raw pointers name libcamera objects that libcamera allows to be queued from any
// thread; the Rust state they reach is behind a Mutex or atomic, and the pool is Send + Sync.
unsafe impl Send for Inner {}
// SAFETY: as above; `Inner` is only shared between the completion callback and the requeue
// thread, both of which go through its Mutexes and atomics.
unsafe impl Sync for Inner {}

impl Inner {
    /// Requeue `k` now that no lease on it is out: its request is reused with the same buffer
    /// and the current controls.
    fn requeue(&self, k: usize) {
        self.submit(k, true);
    }

    /// Queue request `k`. `reuse` is set once the request has completed before (its buffer is
    /// added again); a fresh request already holds its buffer.
    fn submit(&self, k: usize, reuse: bool) {
        let request = self.requests[k].as_ptr();
        if reuse {
            // SAFETY: the request is one of ours and has completed, so it may be reused.
            unsafe {
                sys::libcamera_request_reuse(request, sys::REUSE_BUFFERS);
                sys::libcamera_request_add_buffer(
                    request,
                    self.stream.as_ptr(),
                    self.buffers[k].as_ptr(),
                );
            }
        }
        self.apply_controls(request);
        // SAFETY: the camera is live until `Capture::drop` stops it, which joins this thread first.
        let rc = unsafe { sys::libcamera_camera_queue_request(self.camera.as_ptr(), request) };
        if rc == 0 {
            self.requeues.fetch_add(1, Ordering::Relaxed);
        } else {
            self.parked.lock().unwrap().push(k);
        }
    }

    fn apply_controls(&self, request: *mut sys::Request) {
        let requested = self.requested.lock().unwrap();
        // SAFETY: the request's control list is live while the request is.
        let list = unsafe { sys::libcamera_request_controls(request) };
        set_controls(list, &requested);
    }

    /// The completion callback's work for request `k` (libcamera thread).
    fn complete(&self, request: *mut sys::Request) {
        // SAFETY: libcamera passes a live request; its cookie is the buffer index we set.
        let (status, k) = unsafe {
            (
                sys::libcamera_request_status(request),
                sys::libcamera_request_cookie(request) as usize,
            )
        };
        if status == sys::REQUEST_STATUS_CANCELLED || k >= self.buffers.len() {
            return;
        }
        // SAFETY: the sequence lookup reads the completed request.
        let sequence = unsafe { sys::libcamera_request_sequence(request) };
        let boot_ns = request_sensor_timestamp(request);
        let mut entry = self.templates[k];
        entry.id = u64::from(sequence);
        entry.timestamp_ns = match boot_ns {
            Some(boot) => to_monotonic(boot as i128),
            None => clock_ns(libc::CLOCK_MONOTONIC) as u64,
        };

        match self.pool.acquire_at(k, entry) {
            Ok(mut lease) => {
                fill_metadata(&mut lease, request);
                self.frames.fetch_add(1, Ordering::Relaxed);
                self.parked.lock().unwrap().push(k);
                (self.sink.lock().unwrap())(lease);
            }
            Err(AcquireError::Held { .. }) => {
                // A consumer still holds buffer k's last frame: the new frame is dropped, and the
                // buffer is not requeued until that lease is gone.
                self.held.fetch_add(1, Ordering::Relaxed);
                self.parked.lock().unwrap().push(k);
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.requeue(k);
            }
        }
    }

    /// Requeue every parked buffer whose lease has been dropped. Called from the requeue thread.
    fn sweep(&self) {
        let mut parked = self.parked.lock().unwrap();
        let mut i = 0;
        while i < parked.len() {
            let k = parked[i];
            match self.pool.acquire_at(k, self.templates[k]) {
                Ok(lease) => {
                    // Free again: give the buffer back to libcamera.
                    drop(lease);
                    parked.swap_remove(i);
                    drop(parked);
                    self.requeue(k);
                    parked = self.parked.lock().unwrap();
                }
                Err(_) => i += 1,
            }
        }
    }
}

fn set_controls(list: *mut sys::ControlList, requested: &[(u32, Value)]) {
    for &(id, value) in requested {
        match value {
            Value::Bool(v) => set_value(list, id, TYPE_BOOL, &v as *const bool as *const c_void),
            Value::I32(v) => set_value(list, id, TYPE_INT32, &v as *const i32 as *const c_void),
            Value::I64(v) => set_value(list, id, TYPE_INT64, &v as *const i64 as *const c_void),
            Value::F32(v) => set_value(list, id, TYPE_FLOAT, &v as *const f32 as *const c_void),
            Value::Rect(v) => set_value(list, id, TYPE_RECTANGLE, v.as_ptr() as *const c_void),
        }
    }
}

fn set_value(list: *mut sys::ControlList, id: u32, kind: u32, data: *const c_void) {
    // SAFETY: the value is created, filled from `data` (one element of `kind`), copied into the
    // list, and destroyed here. `list` is a live control list of the caller.
    unsafe {
        let value = sys::libcamera_control_value_create();
        sys::libcamera_control_value_set(value, kind, data, false, 1);
        sys::libcamera_control_list_set(list, id, value);
        sys::libcamera_control_value_destroy(value);
    }
}

/// The sensor timestamp in a completed request's metadata, `CLOCK_BOOTTIME` ns.
fn request_sensor_timestamp(request: *mut sys::Request) -> Option<u64> {
    // SAFETY: the metadata list is live while the completed request is; the value is read only
    // when it has an element, and element 0 is an i64 for this control.
    unsafe {
        let list = sys::libcamera_request_metadata(request);
        if list.is_null() {
            return None;
        }
        let value = sys::libcamera_control_list_get(list, control_id(CTRL_SENSOR_TIMESTAMP));
        if value.is_null() || sys::libcamera_control_value_num_elements(value) == 0 {
            return None;
        }
        let data = sys::libcamera_control_value_get(value) as *const i64;
        if data.is_null() {
            None
        } else {
            Some(*data as u64)
        }
    }
}

/// Reads one control's first element of type `T`, when the request reports it.
fn request_value<T: Copy>(request: *mut sys::Request, which: c_int) -> Option<T> {
    // SAFETY: as in `request_sensor_timestamp`; the caller names a control whose element type is
    // `T` (exposure: i32, gains: f32).
    unsafe {
        let list = sys::libcamera_request_metadata(request);
        if list.is_null() {
            return None;
        }
        let value = sys::libcamera_control_list_get(list, control_id(which));
        if value.is_null() || sys::libcamera_control_value_num_elements(value) == 0 {
            return None;
        }
        let data = sys::libcamera_control_value_get(value) as *const T;
        if data.is_null() { None } else { Some(*data) }
    }
}

fn fill_metadata(lease: &mut FrameLease<FrameMetadata>, request: *mut sys::Request) {
    let exposure_us = request_value::<i32>(request, CTRL_EXPOSURE_TIME);
    let analogue = request_value::<f32>(request, CTRL_ANALOGUE_GAIN);
    let digital = request_value::<f32>(request, CTRL_DIGITAL_GAIN);
    let timestamp = lease.timestamp_ns();
    if let Some(meta) = lease.meta_mut() {
        *meta = FrameMetadata {
            controls: meta.controls,
            sensor_timestamp_ns: timestamp,
            exposure_us: exposure_us.map_or(0, |v| v.max(0) as u32),
            analogue_gain_milli: analogue.map_or(0, |g| (g * 1000.0) as u32),
            digital_gain_milli: digital.map_or(0, |g| (g * 1000.0) as u32),
            lux: None,
            colour_temperature_k: None,
        };
        if let Some(v) = exposure_us {
            meta.controls[Control::Exposure.index()] = ControlValue::Manual(v.max(0) as u32);
        }
        if let Some(g) = analogue {
            meta.controls[Control::Gain.index()] = ControlValue::Manual((g * 1000.0) as u32);
        }
    }
}

extern "C" fn on_request_completed(data: *mut c_void, request: *mut sys::Request) {
    // SAFETY: `data` is the `Inner` registered in `Capture::open`; it is alive until the callback
    // is disconnected and the camera stopped, which `Capture::drop` does before dropping it.
    let inner = unsafe { &*(data as *const Inner) };
    inner.complete(request);
}

/// The public listing of one registered control.
fn entry_of(listed: &Listed) -> DeviceEntry {
    DeviceEntry {
        control: DeviceControl {
            id: listed.id,
            name: styx_controls::DeviceName::new(&listed.name),
            kind: listed.kind(),
            min: listed.min as i64,
            max: listed.max as i64,
            step: 0,
            default: listed.default as i64,
            read_only: !listed.input,
        },
        mapped: map::mapped_control(&listed.name),
    }
}

/// A capture: a started camera whose frames reach the sink as leases.
pub struct Capture {
    camera: Camera,
    allocator: NonNull<sys::Allocator>,
    callback: *mut sys::CallbackHandle,
    inner: Box<Inner>,
    thread: Option<JoinHandle<()>>,
    format: MediaFormat,
    /// Every control the camera registers, as read at open.
    listed: Vec<Listed>,
    /// The same controls, as the public listing reports them.
    entries: Vec<DeviceEntry>,
    /// The camera's maximum crop (`ScalerCropMaximum`), when it reports one: zoom is set from it.
    crop_max: Option<Rect>,
}

/// One control of the camera's registry, as read at open. Numbers are `f64`: a float control's
/// range is kept whole, and an integer's is exact in it.
#[derive(Debug, Clone)]
struct Listed {
    id: u32,
    name: String,
    ty: u32,
    menu: bool,
    input: bool,
    min: f64,
    max: f64,
    default: f64,
}

impl Listed {
    fn kind(&self) -> DeviceKind {
        map::device_kind(self.ty, self.menu)
    }
}

/// Reads every control of `camera`'s registry: its id, name, type, whether it is a menu, whether
/// it is settable (an input), and its range and default.
fn list_controls(camera: &Camera) -> Vec<Listed> {
    let mut out = Vec::new();
    // SAFETY: the camera is acquired and live. The map is the camera's own, read while it lives;
    // the iterator is created here and destroyed below. Each control id is looked up in libcamera's
    // registry, which holds static controls for the process; the pointers read from it are not owned.
    unsafe {
        let map = sys::libcamera_camera_controls(camera.0.as_ptr());
        if map.is_null() {
            return out;
        }
        let iter = sys::libcamera_control_info_map_iter_create(map);
        if iter.is_null() {
            return out;
        }
        while sys::libcamera_control_info_map_iter_has_next(iter) {
            let id = sys::libcamera_control_info_map_iter_key(iter);
            let info = sys::libcamera_control_info_map_iter_value(iter) as *mut sys::ControlInfo;
            sys::libcamera_control_info_map_iter_next(iter);
            let control = sys::libcamera_control_from_id(id);
            if control.is_null() || info.is_null() {
                continue;
            }
            let name = CStr::from_ptr(sys::libcamera_control_name(control))
                .to_string_lossy()
                .into_owned();
            let ty = sys::libcamera_control_id_type(control);
            let input = sys::libcamera_control_id_is_input(control);
            let enumerators = sys::libcamera_control_id_enumerators_iter_create(control);
            let menu = !enumerators.is_null()
                && sys::libcamera_control_id_enumerators_iter_has_next(enumerators);
            if !enumerators.is_null() {
                sys::libcamera_control_id_enumerators_iter_destroy(enumerators);
            }
            out.push(Listed {
                id,
                name,
                ty,
                menu,
                input,
                min: number(sys::libcamera_control_info_min(info), ty),
                max: number(sys::libcamera_control_info_max(info), ty),
                default: number(sys::libcamera_control_info_def(info), ty),
            });
        }
        sys::libcamera_control_info_map_iter_destroy(iter);
    }
    out
}

/// A control value's first element as a number, for the types a passthrough carries.
fn number(value: *const sys::ControlValue, ty: u32) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    // SAFETY: the value is a live control value of the camera; its first element is read only
    // when it has one, as the type `ty` says it is.
    unsafe {
        if sys::libcamera_control_value_num_elements(value) == 0 {
            return 0.0;
        }
        let data = sys::libcamera_control_value_get(value);
        if data.is_null() {
            return 0.0;
        }
        match ty {
            TYPE_BOOL => f64::from(u8::from(*(data as *const bool))),
            TYPE_INT32 => f64::from(*(data as *const i32)),
            TYPE_INT64 => *(data as *const i64) as f64,
            TYPE_FLOAT => f64::from(*(data as *const f32)),
            _ => 0.0,
        }
    }
}

/// The camera's `ScalerCropMaximum`, when it reports one.
fn crop_maximum(camera: &Camera) -> Option<Rect> {
    // SAFETY: the camera is live; its property list is read while it lives, and the id is the
    // one the shim names for `ScalerCropMaximum`.
    unsafe {
        let properties = sys::libcamera_camera_properties(camera.0.as_ptr());
        if properties.is_null() {
            return None;
        }
        let value = sys::libcamera_control_list_get(properties, sys::styx_property_id(0));
        if value.is_null() || sys::libcamera_control_value_num_elements(value) == 0 {
            return None;
        }
        let data = sys::libcamera_control_value_get(value) as *const [i32; 4];
        if data.is_null() {
            return None;
        }
        let [x, y, width, height] = *data;
        (width > 0 && height > 0).then(|| Rect {
            x: x.max(0) as u32,
            y: y.max(0) as u32,
            width: width as u32,
            height: height as u32,
        })
    }
}

/// A Styx rectangle as libcamera's, all four values as the camera takes them.
fn rect_raw(rect: Rect) -> [i32; 4] {
    [
        rect.x as i32,
        rect.y as i32,
        rect.width as i32,
        rect.height as i32,
    ]
}

/// Sets `id` to `value` in `requested`, replacing an earlier value for the same id.
fn upsert(requested: &mut Requested, id: u32, value: Value) {
    match requested.iter_mut().find(|(known, _)| *known == id) {
        Some(slot) => slot.1 = value,
        None => requested.push((id, value)),
    }
}

// SAFETY: the raw handles are owned by this value and used only under `&self`/`&mut self`, from
// libcamera-safe calls; the callback reaches `Inner` through its own synchronisation.
unsafe impl Send for Capture {}

impl Capture {
    /// Opens camera `id` in `config` and starts it. Every frame goes to `sink`, on libcamera's
    /// thread, as a lease. The frame's buffer returns to the camera when the lease is dropped.
    pub fn open(id: &str, config: CaptureConfig, sink: FrameSink) -> Result<Self, Error> {
        let camera = manager()?
            .camera(id)
            .ok_or_else(|| Error::Camera(format!("no camera {id}")))?;
        // SAFETY: the camera is live; the status is checked below.
        if unsafe { sys::libcamera_camera_acquire(camera.0.as_ptr()) } != 0 {
            return Err(Error::Camera(format!("camera {id} is busy")));
        }
        let listed = list_controls(&camera);
        let crop_max = crop_maximum(&camera);
        let mut capture = Self::configure(camera, config, sink)?;
        capture.entries = listed.iter().map(entry_of).collect();
        capture.listed = listed;
        capture.crop_max = crop_max;
        capture.start()?;
        Ok(capture)
    }

    fn configure(camera: Camera, config: CaptureConfig, sink: FrameSink) -> Result<Self, Error> {
        let drm = styx_format::drm::to_drm(config.code)
            .ok_or(Error::Unsupported("format has no libcamera (DRM) code"))?;
        let mut configuration = camera
            .configuration(sys::ROLE_VIDEO_RECORDING)
            .ok_or_else(|| Error::Camera("no configuration for the video role".into()))?;
        let stream_config = configuration
            .stream()
            .ok_or_else(|| Error::Camera("camera has no stream".into()))?;
        // SAFETY: `stream_config` is inside the live configuration; the setters write fields.
        unsafe {
            sys::styx_stream_set_format(
                stream_config.as_ptr(),
                drm.fourcc,
                drm.modifier,
                config.width,
                config.height,
            );
            sys::styx_stream_set_buffer_count(stream_config.as_ptr(), config.buffers);
        }
        // SAFETY: the configuration is live.
        let status =
            unsafe { sys::libcamera_camera_configuration_validate(configuration.0.as_ptr()) };
        if status != sys::CONFIG_VALID && status != sys::CONFIG_ADJUSTED {
            return Err(Error::Camera("camera rejected the configuration".into()));
        }
        // SAFETY: the stream configuration is read back after validation.
        let (fourcc, modifier, width, height, stride, frame_size) = unsafe {
            let s = stream_config.as_ptr();
            (
                sys::styx_stream_fourcc(s),
                sys::styx_stream_modifier(s),
                sys::styx_stream_width(s),
                sys::styx_stream_height(s),
                sys::styx_stream_stride(s) as usize,
                sys::styx_stream_frame_size(s) as usize,
            )
        };
        let code = styx_format::drm::from_drm(fourcc, modifier).ok_or(Error::Unsupported(
            "libcamera chose a format Styx does not know",
        ))?;
        if code != config.code || width != config.width || height != config.height {
            return Err(Error::Camera(format!(
                "libcamera adjusted the configuration to {code:?} {width}x{height}; Styx does not adapt"
            )));
        }
        let resolution = Resolution::new(width, height)
            .ok_or_else(|| Error::Camera("zero-sized configuration".into()))?;
        let format = MediaFormat::with_defaults(code, resolution);

        // SAFETY: the configuration is live and validated; it is applied once.
        if unsafe { sys::libcamera_camera_configure(camera.0.as_ptr(), configuration.0.as_ptr()) }
            != 0
        {
            return Err(Error::Camera("camera configure failed".into()));
        }
        // SAFETY: the stream belongs to the configured camera.
        let stream = NonNull::new(unsafe {
            sys::libcamera_stream_configuration_stream(stream_config.as_ptr())
        })
        .ok_or_else(|| Error::Camera("configured stream is missing".into()))?;

        // SAFETY: the allocator is owned here and freed in `Capture::drop` (after the stream stops).
        let allocator =
            NonNull::new(unsafe { sys::libcamera_framebuffer_allocator_create(camera.0.as_ptr()) })
                .ok_or_else(|| {
                    Error::Camera("framebuffer allocator could not be created".into())
                })?;
        // SAFETY: the allocator and stream are live.
        if unsafe {
            sys::libcamera_framebuffer_allocator_allocate(allocator.as_ptr(), stream.as_ptr())
        } < 0
        {
            // SAFETY: the allocator is owned here and not used again.
            unsafe { sys::libcamera_framebuffer_allocator_destroy(allocator.as_ptr()) };
            return Err(Error::Camera("buffer allocation failed".into()));
        }
        // SAFETY: the buffer list is owned by the allocator, which lives in this value.
        let list = unsafe {
            sys::libcamera_framebuffer_allocator_buffers(allocator.as_ptr(), stream.as_ptr())
        };
        let count = if list.is_null() {
            0
        } else {
            // SAFETY: the list is non-null here and lives as long as the allocator.
            unsafe { sys::libcamera_framebuffer_list_size(list) }
        };
        let mut buffers = Vec::with_capacity(count);
        let mut memories: Vec<Box<dyn BufferMemory>> = Vec::with_capacity(count);
        let mut templates = Vec::with_capacity(count);
        let mut span = None;
        for index in 0..count {
            // SAFETY: `index` is below the list's size.
            let fb = unsafe { sys::libcamera_framebuffer_list_get(list, index) }
                as *mut sys::FrameBuffer;
            let Some(fb) = NonNull::new(fb) else {
                return Err(Error::Camera("allocator returned a null buffer".into()));
            };
            let parts = dmabuf_parts(fb)?;
            let planned = planes::plan(
                code,
                width,
                height,
                stride,
                frame_size,
                &parts.iter().map(|part| part.part).collect::<Vec<_>>(),
            )
            .map_err(|err| Error::Camera(err.to_string()))?;
            match span {
                None => span = Some(planned.span),
                Some(first) if first != planned.span => {
                    return Err(Error::Camera(format!(
                        "{code}: buffer {index} lays its planes out differently from buffer 0"
                    )));
                }
                Some(_) => {}
            }
            // Every part is the same dma-buf, so the first part's fd maps the whole buffer.
            let memory_fd = parts
                .into_iter()
                .next()
                .map(|part| part.fd)
                .ok_or_else(|| Error::Camera("buffer has no planes".into()))?;
            buffers.push(fb);
            memories.push(Box::new(map_dmabuf(memory_fd, planned.extent)?));
            templates.push(FrameEntry::new(0, 0, format, planned.layouts));
        }
        if count == 0 {
            return Err(Error::Camera("camera allocated no buffers".into()));
        }
        let payload = span.unwrap_or(0);
        let size = BufferSize::new(payload, 0);
        let pool = FramePool::new(size, memories)
            .map_err(|err| Error::Camera(format!("frame pool: {err}")))?;

        let mut requests = Vec::with_capacity(count);
        for (k, fb) in buffers.iter().enumerate() {
            // SAFETY: the camera is configured; the request is owned by the Inner built below.
            let request = NonNull::new(unsafe {
                sys::libcamera_camera_create_request(camera.0.as_ptr(), k as u64)
            })
            .ok_or_else(|| Error::Camera("request could not be created".into()))?;
            // SAFETY: the request and buffer are live and belong to the configured stream.
            if unsafe {
                sys::libcamera_request_add_buffer(request.as_ptr(), stream.as_ptr(), fb.as_ptr())
            } != 0
            {
                return Err(Error::Camera("request rejected its buffer".into()));
            }
            requests.push(request);
        }

        let mut inner = Box::new(Inner {
            camera: camera.0,
            stream,
            pool,
            templates,
            requests,
            buffers,
            requested: Mutex::new(Requested::new()),
            parked: Mutex::new(Vec::new()),
            sink: Mutex::new(sink),
            stopping: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            held: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            requeues: AtomicU64::new(0),
        });
        // SAFETY: `inner` is boxed, so its address is stable; it is disconnected before it drops.
        let callback = unsafe {
            sys::libcamera_camera_request_completed_connect(
                camera.0.as_ptr(),
                on_request_completed,
                (&mut *inner) as *mut Inner as *mut c_void,
            )
        };
        Ok(Self {
            camera,
            allocator,
            callback,
            inner,
            thread: None,
            format,
            listed: Vec::new(),
            entries: Vec::new(),
            crop_max: None,
        })
    }

    fn start(&mut self) -> Result<(), Error> {
        // SAFETY: the camera is configured; the empty control list lives for the call.
        let controls = unsafe { sys::libcamera_control_list_create() };
        // SAFETY: `controls` is the list created above; it is destroyed after the start call.
        let rc = unsafe { sys::libcamera_camera_start(self.camera.0.as_ptr(), controls) };
        // SAFETY: the list is not used after this.
        unsafe { sys::libcamera_control_list_destroy(controls) };
        if rc != 0 {
            return Err(Error::Camera(format!("camera start failed ({rc})")));
        }
        for k in 0..self.inner.requests.len() {
            self.inner.submit(k, false);
        }
        let raw = SendPtr(&*self.inner as *const Inner);
        self.thread = Some(
            std::thread::Builder::new()
                .name("styx-libcamera-requeue".into())
                .spawn(move || {
                    let raw = raw;
                    // SAFETY: `Capture::drop` stops and joins this thread before it drops the Inner
                    // the pointer names, so the reference is valid for the whole loop.
                    let inner = unsafe { &*raw.0 };
                    while !inner.stopping.load(Ordering::Acquire) {
                        inner.sweep();
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .map_err(|err| Error::Camera(format!("requeue thread: {err}")))?,
        );
        Ok(())
    }

    /// The control with `name` in the camera's registry, when the camera has it.
    fn named(&self, name: &str) -> Option<&Listed> {
        self.listed.iter().find(|listed| listed.name == name)
    }

    /// The id of the control `name`, or a refusal when the camera does not register it.
    fn id_of(&self, name: &str) -> Result<u32, Error> {
        self.named(name)
            .map(|listed| listed.id)
            .ok_or_else(|| Error::Refused(format!("the camera has no {name} control")))
    }

    /// The range of the float control `name`, as `f32`.
    fn float_range(&self, name: &str) -> Result<(f32, f32), Error> {
        let listed = self
            .named(name)
            .ok_or_else(|| Error::Refused(format!("the camera has no {name} control")))?;
        Ok((listed.min as f32, listed.max as f32))
    }

    /// The values a Styx control's value asks of the camera, by libcamera id. A value the camera
    /// has no control for is refused; a value libcamera cannot take this way is unsupported.
    fn plan(&self, control: Control, value: ControlValue) -> Result<Vec<(u32, Value)>, Error> {
        Ok(match (control, value) {
            (Control::Exposure, ControlValue::Manual(us)) => vec![
                (self.id_of("AeEnable")?, Value::Bool(false)),
                (
                    self.id_of("ExposureTime")?,
                    Value::I32(i32::try_from(us).unwrap_or(i32::MAX)),
                ),
            ],
            (Control::Exposure, ControlValue::Auto) => {
                vec![(self.id_of("AeEnable")?, Value::Bool(true))]
            }
            (Control::Gain, ControlValue::Manual(milli)) => {
                vec![(
                    self.id_of("AnalogueGain")?,
                    Value::F32(milli as f32 / 1_000.0),
                )]
            }
            (Control::WhiteBalance, ControlValue::Auto) => {
                vec![(self.id_of("AwbEnable")?, Value::Bool(true))]
            }
            (Control::WhiteBalance, ControlValue::Manual(_)) => {
                return Err(Error::Unsupported(
                    "libcamera takes white balance as colour gains, which Styx does not model; use Auto",
                ));
            }
            (Control::Focus, ControlValue::Auto) => {
                vec![(self.id_of("AfMode")?, Value::I32(map::AF_MODE_CONTINUOUS))]
            }
            (Control::Focus, ControlValue::Manual(position)) => {
                let (min, max) = self.float_range("LensPosition")?;
                vec![
                    (self.id_of("AfMode")?, Value::I32(map::AF_MODE_MANUAL)),
                    (
                        self.id_of("LensPosition")?,
                        Value::F32(map::lens_position(position, min, max)),
                    ),
                ]
            }
            (
                Control::Brightness | Control::Contrast | Control::Saturation | Control::Sharpness,
                ControlValue::Manual(v),
            ) => {
                let name = match control {
                    Control::Brightness => "Brightness",
                    Control::Contrast => "Contrast",
                    Control::Saturation => "Saturation",
                    _ => "Sharpness",
                };
                let (min, max) = self.float_range(name)?;
                vec![(
                    self.id_of(name)?,
                    Value::F32(map::normalised_to_float(v, min, max)),
                )]
            }
            (Control::Gamma, ControlValue::Manual(v)) => {
                vec![(self.id_of("Gamma")?, Value::F32(map::gamma_to_float(v)))]
            }
            (Control::Crop, ControlValue::Crop(Some(rect))) => {
                vec![(self.id_of("ScalerCrop")?, Value::Rect(rect_raw(rect)))]
            }
            (Control::Crop, ControlValue::Crop(None)) => {
                let whole = self.crop_max.ok_or_else(|| {
                    Error::Refused("the camera reports no ScalerCropMaximum".into())
                })?;
                vec![(self.id_of("ScalerCrop")?, Value::Rect(rect_raw(whole)))]
            }
            (Control::Zoom, ControlValue::Manual(zoom)) => {
                let full = self.crop_max.ok_or_else(|| {
                    Error::Refused("the camera reports no ScalerCropMaximum; zoom needs it".into())
                })?;
                let crop = map::zoom_crop(full, zoom);
                vec![(self.id_of("ScalerCrop")?, Value::Rect(rect_raw(crop)))]
            }
            _ => {
                return Err(Error::Unsupported(
                    "the libcamera backend has no control for this Styx control",
                ));
            }
        })
    }

    /// Sets a Styx control for the requests queued from now on.
    pub fn set_control(&self, update: Update) -> Result<(), Error> {
        update.control.validate(update.value)?;
        let writes = self.plan(update.control, update.value)?;
        let mut requested = self.inner.requested.lock().unwrap();
        for (id, value) in writes {
            upsert(&mut requested, id, value);
        }
        Ok(())
    }

    /// Every control the camera registers, mapped to a Styx control or a passthrough.
    pub fn device_controls(&self) -> &[DeviceEntry] {
        &self.entries
    }

    /// The value of a passthrough control by its libcamera id: the last value set for it, or the
    /// camera's default. libcamera reports what a camera did in each frame's metadata, not through
    /// this call, so this is what was asked, not what the sensor reports.
    pub fn get_device_control(&self, id: u32) -> Result<i64, Error> {
        let listed = self.listed_by_id(id)?;
        let requested = self.inner.requested.lock().unwrap();
        let value = requested
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, v)| *v);
        match value {
            Some(Value::Bool(v)) => Ok(i64::from(v)),
            Some(Value::I32(v)) => Ok(i64::from(v)),
            Some(Value::I64(v)) => Ok(v),
            _ => match listed.kind() {
                DeviceKind::Boolean
                | DeviceKind::Integer
                | DeviceKind::Integer64
                | DeviceKind::Menu => Ok(listed.default as i64),
                _ => Err(Error::Refused(format!(
                    "{} has no integer value",
                    listed.name
                ))),
            },
        }
    }

    /// Sets a passthrough control by its libcamera id, for the requests queued from now on.
    pub fn set_device_control(&self, id: u32, value: i64) -> Result<(), Error> {
        let listed = self.listed_by_id(id)?;
        if map::mapped_control(&listed.name).is_some() {
            return Err(Error::Refused(format!(
                "{} is mapped to a Styx control: set that one",
                listed.name
            )));
        }
        if !listed.input {
            return Err(Error::Refused(format!("{} is read-only", listed.name)));
        }
        let (min, max) = (listed.min as i64, listed.max as i64);
        let stored =
            match listed.kind() {
                DeviceKind::Boolean => match value {
                    0 => Value::Bool(false),
                    1 => Value::Bool(true),
                    _ => return Err(Error::Refused(format!("{} takes 0 or 1", listed.name))),
                },
                DeviceKind::Integer | DeviceKind::Menu => {
                    if !(min..=max).contains(&value) {
                        return Err(Error::Refused(format!(
                            "{} takes {min}..={max}, not {value}",
                            listed.name
                        )));
                    }
                    Value::I32(i32::try_from(value).map_err(|_| {
                        Error::Refused(format!("{} takes 32-bit values", listed.name))
                    })?)
                }
                DeviceKind::Integer64 => Value::I64(value),
                _ => {
                    return Err(Error::Refused(format!(
                        "{} is not an integer control; it is listed, not settable",
                        listed.name
                    )));
                }
            };
        let mut requested = self.inner.requested.lock().unwrap();
        upsert(&mut requested, id, stored);
        Ok(())
    }

    fn listed_by_id(&self, id: u32) -> Result<&Listed, Error> {
        self.listed
            .iter()
            .find(|listed| listed.id == id)
            .ok_or_else(|| Error::Refused(format!("the camera has no control with id {id}")))
    }

    /// Counts since the capture started.
    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            frames: self.inner.frames.load(Ordering::Relaxed),
            held: self.inner.held.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            requeues: self.inner.requeues.load(Ordering::Relaxed),
        }
    }

    /// The camera's id.
    pub fn camera_id(&self) -> String {
        self.camera.id()
    }
}

#[derive(Clone, Copy)]
struct SendPtr(*const Inner);
// SAFETY: the pointer is only dereferenced on the requeue thread, which `Drop` joins first.
unsafe impl Send for SendPtr {}

impl Drop for Capture {
    fn drop(&mut self) {
        self.inner.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // SAFETY: stopping the camera cancels its requests; no more callbacks run after it returns.
        unsafe {
            sys::libcamera_camera_stop(self.camera.0.as_ptr());
            sys::libcamera_camera_request_completed_disconnect(
                self.camera.0.as_ptr(),
                self.callback,
            );
        }
        for request in self.inner.requests.iter() {
            // SAFETY: each request is owned by Inner and destroyed once.
            unsafe { sys::libcamera_request_destroy(request.as_ptr()) };
        }
        // SAFETY: the allocator is owned here and freed once, after its buffers are unused.
        unsafe {
            sys::libcamera_framebuffer_allocator_free(
                self.allocator.as_ptr(),
                self.inner.stream.as_ptr(),
            );
            sys::libcamera_framebuffer_allocator_destroy(self.allocator.as_ptr());
            sys::libcamera_camera_release(self.camera.0.as_ptr());
        }
        // The Inner (and its pool, whose leases may outlive this) drops with the fields.
    }
}

/// One dma-buf part of a libcamera buffer: its fd (duplicated, so the frame owns it independently
/// of libcamera) and the planning facts.
struct DmaPart {
    fd: OwnedFd,
    part: planes::Part,
}

/// Every dma-buf part of a buffer, in libcamera's plane order.
fn dmabuf_parts(fb: NonNull<sys::FrameBuffer>) -> Result<Vec<DmaPart>, Error> {
    // SAFETY: the planes list is owned here and destroyed before return.
    unsafe {
        let planes = sys::libcamera_framebuffer_planes(fb.as_ptr());
        if planes.is_null() {
            return Err(Error::Camera("buffer has no planes".into()));
        }
        let count = sys::libcamera_framebuffer_planes_size(planes);
        let mut out = Vec::with_capacity(count);
        let mut result = Ok(());
        for index in 0..count {
            let plane = sys::libcamera_framebuffer_planes_at(planes, index);
            if plane.is_null() {
                result = Err(Error::Camera("buffer plane is missing".into()));
                break;
            }
            let fd = sys::libcamera_framebuffer_plane_fd(plane);
            if fd < 0 {
                result = Err(Error::Camera("buffer plane is not a dma-buf".into()));
                break;
            }
            // SAFETY: `fd` is the plane's live descriptor; duplicating it yields a new owned fd.
            let dup = libc::dup(fd);
            if dup < 0 {
                result = Err(Error::Camera("duplicating a buffer fd failed".into()));
                break;
            }
            // SAFETY: `dup` is a fresh descriptor this value now owns.
            let fd = OwnedFd::from_raw_fd(dup);
            let file = match file_id(&fd) {
                Ok(file) => file,
                Err(err) => {
                    result = Err(err);
                    break;
                }
            };
            out.push(DmaPart {
                fd,
                part: planes::Part {
                    file,
                    offset: sys::libcamera_framebuffer_plane_offset(plane),
                    length: sys::libcamera_framebuffer_plane_length(plane),
                    offset_valid: sys::libcamera_framebuffer_plane_offset_valid(plane),
                },
            });
        }
        sys::libcamera_framebuffer_planes_destroy(planes);
        result.map(|()| out)
    }
}

/// The (device, inode) of the dma-buf behind `fd`: two fds with one key name one memory.
fn file_id(fd: &OwnedFd) -> Result<(u64, u64), Error> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is a live descriptor and `stat` is a valid out-pointer for `fstat`.
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(Error::Camera(
            "reading a buffer fd's identity failed".into(),
        ));
    }
    // SAFETY: `fstat` succeeded, so every field of `stat` is initialised.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

fn map_dmabuf(fd: OwnedFd, len: usize) -> Result<FdMemory, Error> {
    FdMemory::dmabuf(fd, len, false)
        .map_err(|err| Error::Camera(format!("mapping a dma-buf failed: {err}")))
}

impl fmt::Debug for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capture")
            .field("camera", &self.camera.id())
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}
