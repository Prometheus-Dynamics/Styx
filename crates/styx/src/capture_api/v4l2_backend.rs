use std::mem;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smallvec::smallvec;
use styx_core::prelude::*;
use v4l::buffer::Metadata as V4l2Metadata;
use v4l::buffer::Type;
use v4l::device::Handle;
use v4l::memory::Memory;
use v4l::v4l_sys::*;
use v4l::v4l2;
use v4l::{format::FourCC, prelude::*, video::Capture as _};

use crate::capture_api::controls::apply_v4l2_controls;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::{ExternalBackingTracker, StageMetrics};
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

struct V4l2MappedBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for V4l2MappedBuffer {}
unsafe impl Sync for V4l2MappedBuffer {}

struct V4l2MmapManager {
    handle: Arc<Handle>,
    buf_type: Type,
    buffers: Vec<V4l2MappedBuffer>,
    state: Mutex<V4l2MmapState>,
}

struct V4l2MmapState {
    active: bool,
    queued: Vec<bool>,
    checked_out: Vec<bool>,
    timeout_ms: i32,
}

struct V4l2MmapBacking {
    manager: Arc<V4l2MmapManager>,
    index: Mutex<Option<usize>>,
    tracker: Arc<ExternalBackingTracker>,
    bytes: usize,
}

impl V4l2MmapManager {
    fn new(
        handle: Arc<Handle>,
        buf_type: Type,
        count: u32,
        timeout: Duration,
    ) -> std::io::Result<Arc<Self>> {
        let mut reqbufs = v4l2_requestbuffers {
            count,
            type_: buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                handle.fd(),
                v4l2::vidioc::VIDIOC_REQBUFS,
                &mut reqbufs as *mut _ as *mut std::os::raw::c_void,
            )?;
        }

        let mut buffers = Vec::with_capacity(reqbufs.count as usize);
        for index in 0..reqbufs.count {
            let mut v4l2_buf = v4l2_buffer {
                index,
                type_: buf_type as u32,
                memory: Memory::Mmap as u32,
                ..unsafe { mem::zeroed() }
            };
            unsafe {
                v4l2::ioctl(
                    handle.fd(),
                    v4l2::vidioc::VIDIOC_QUERYBUF,
                    &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
                )?;
                let ptr = v4l2::mmap(
                    std::ptr::null_mut(),
                    v4l2_buf.length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    handle.fd(),
                    v4l2_buf.m.offset as libc::off_t,
                )?;
                let ptr = NonNull::new(ptr.cast::<u8>())
                    .ok_or_else(|| std::io::Error::other("v4l2 mmap returned null"))?;
                buffers.push(V4l2MappedBuffer {
                    ptr,
                    len: v4l2_buf.length as usize,
                });
            }
        }

        Ok(Arc::new(Self {
            handle,
            buf_type,
            buffers,
            state: Mutex::new(V4l2MmapState {
                active: false,
                queued: vec![false; reqbufs.count as usize],
                checked_out: vec![false; reqbufs.count as usize],
                timeout_ms: timeout.as_millis().try_into().unwrap_or(i32::MAX),
            }),
        }))
    }

    fn dequeue(&self) -> std::io::Result<(usize, V4l2Metadata)> {
        let mut state = self.state.lock().unwrap();
        if !state.active {
            for index in 0..self.buffers.len() {
                if !state.queued[index] && !state.checked_out[index] {
                    self.queue_locked(index, &mut state)?;
                }
            }
            self.stream_on_locked(&mut state)?;
        }

        if self.handle.poll(libc::POLLIN, state.timeout_ms)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "VIDIOC_DQBUF",
            ));
        }

        let mut v4l2_buf = v4l2_buffer {
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_DQBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        let index = v4l2_buf.index as usize;
        state.queued[index] = false;
        state.checked_out[index] = true;
        Ok((
            index,
            V4l2Metadata {
                bytesused: v4l2_buf.bytesused,
                flags: v4l2_buf.flags.into(),
                field: v4l2_buf.field,
                timestamp: v4l2_buf.timestamp.into(),
                sequence: v4l2_buf.sequence,
            },
        ))
    }

    fn recycle(&self, index: usize) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if index >= state.checked_out.len() {
            return Ok(());
        }
        state.checked_out[index] = false;
        if state.active {
            self.queue_locked(index, &mut state)?;
        }
        Ok(())
    }

    fn mapped_plane(&self, index: usize) -> Option<&[u8]> {
        let buffer = self.buffers.get(index)?;
        Some(unsafe { std::slice::from_raw_parts(buffer.ptr.as_ptr(), buffer.len) })
    }

    fn mapped_bytes(&self, index: usize) -> Option<usize> {
        self.buffers.get(index).map(|buffer| buffer.len)
    }

    fn stop_stream(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        self.stop_stream_locked(&mut state)
    }

    fn stream_on_locked(&self, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if state.active {
            return Ok(());
        }
        let mut typ = self.buf_type as u32;
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMON,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.active = true;
        Ok(())
    }

    fn stop_stream_locked(&self, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if !state.active {
            return Ok(());
        }
        let mut typ = self.buf_type as u32;
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMOFF,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.active = false;
        for queued in &mut state.queued {
            *queued = false;
        }
        Ok(())
    }

    fn queue_locked(&self, index: usize, state: &mut V4l2MmapState) -> std::io::Result<()> {
        if state.queued[index] {
            return Ok(());
        }
        let mut v4l2_buf = v4l2_buffer {
            index: index as u32,
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_QBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        state.queued[index] = true;
        Ok(())
    }
}

impl Drop for V4l2MmapManager {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = self.stop_stream_locked(&mut state);
        }
        for buffer in &self.buffers {
            unsafe {
                let _ = v4l2::munmap(buffer.ptr.as_ptr().cast(), buffer.len);
            }
        }
        let mut reqbufs = v4l2_requestbuffers {
            count: 0,
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            let _ = v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_REQBUFS,
                &mut reqbufs as *mut _ as *mut std::os::raw::c_void,
            );
        }
    }
}

impl V4l2MmapBacking {
    fn new(
        manager: Arc<V4l2MmapManager>,
        index: usize,
        tracker: Arc<ExternalBackingTracker>,
        bytes: usize,
    ) -> Arc<Self> {
        tracker.acquire(bytes);
        Arc::new(Self {
            manager,
            index: Mutex::new(Some(index)),
            tracker,
            bytes,
        })
    }
}

impl ExternalBacking for V4l2MmapBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        match index {
            0 => {
                let buffer_index = *self.index.lock().unwrap();
                self.manager.mapped_plane(buffer_index?)
            }
            _ => None,
        }
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "v4l2_mmap"
    }
}

impl Drop for V4l2MmapBacking {
    fn drop(&mut self) {
        self.tracker.release(self.bytes);
        if let Some(index) = self.index.lock().unwrap().take() {
            let _ = self.manager.recycle(index);
        }
    }
}

fn is_encoded_bitstream(code: FourCc) -> bool {
    matches!(
        &code.to_u32().to_le_bytes(),
        b"H264" | b"H265" | b"HEVC" | b"MJPG" | b"JPEG"
    )
}

fn supports_v4l2_mmap_zero_copy(code: FourCc) -> bool {
    matches!(
        &code.to_u32().to_le_bytes(),
        b"MJPG" | b"JPEG" | b"YUYV" | b"RG24" | b"RGB3" | b"BGR3" | b"RGBA" | b"BGRA"
    )
}

fn build_v4l2_single_plane_layout(
    encoded: bool,
    height: usize,
    stride: usize,
    bytes_used: usize,
) -> Option<PlaneLayout> {
    if encoded {
        return Some(PlaneLayout {
            offset: 0,
            len: bytes_used,
            stride: bytes_used.max(1),
        });
    }
    if height == 0 || stride == 0 {
        return None;
    }
    let required = height.saturating_mul(stride);
    if bytes_used < required {
        return None;
    }
    Some(PlaneLayout {
        offset: 0,
        len: required,
        stride,
    })
}

fn min_stride_for_fourcc(code: FourCc, width: usize) -> usize {
    match &code.to_u32().to_le_bytes() {
        // MIPI packed RAW10/RAW12 bayer.
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" => width.div_ceil(4) * 5,
        b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" => width.div_ceil(2) * 3,

        // 8-bit bayer.
        b"BA81" | b"RGGB" | b"GRBG" | b"GBRG" | b"BGGR" => width,

        // 10/12/14/16-bit bayer (stored in 16-bit words) and mono16.
        b"BA10" | b"BA12" | b"BA14" | b"BG10" | b"BG12" | b"BG14" | b"BG16" | b"GB10" | b"GB12"
        | b"GB14" | b"GB16" | b"RG10" | b"RG12" | b"RG14" | b"RG16" | b"GR10" | b"GR12"
        | b"GR14" | b"GR16" | b"BYR2" | b"R16 " => width.saturating_mul(2),

        // Common packed YUV/RGB defaults.
        b"YUYV" => width.saturating_mul(2),
        b"NV12" => width, // luma plane; backend uses bytesused/stride anyway
        b"RG24" | b"RGB3" | b"BGR3" => width.saturating_mul(3),
        b"RGBA" | b"BGRA" | b"RGB0" | b"BGR0" => width.saturating_mul(4),
        _ => width.saturating_mul(3),
    }
}

pub(super) fn start_v4l2(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
) -> Result<CaptureHandle, CaptureError> {
    let path = match &backend.handle {
        BackendHandle::V4l2 { path } => path.clone(),
        _ => return Err(CaptureError::Backend("v4l2 path missing".into())),
    };

    let dev = Device::with_path(&path).map_err(|e| CaptureError::Backend(e.to_string()))?;

    let repr = mode.format.code.to_u32().to_le_bytes();
    let fourcc = FourCC::new(&repr);
    let mut fmt = dev
        .format()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    fmt.width = mode.format.resolution.width.get();
    fmt.height = mode.format.resolution.height.get();
    fmt.fourcc = fourcc;
    dev.set_format(&fmt)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    if let Some(iv) = interval {
        let mut params = dev
            .params()
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        params.interval.numerator = iv.numerator.get();
        params.interval.denominator = iv.denominator.get();
        dev.set_params(&params)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
    }

    if !controls.is_empty() {
        apply_v4l2_controls(&path, &controls)?;
    }

    let width = fmt.width as usize;
    let encoded = is_encoded_bitstream(mode.format.code);
    let min_stride = min_stride_for_fourcc(mode.format.code, width);
    let stride_bytes = if encoded {
        0
    } else if fmt.stride > 0 {
        (fmt.stride as usize).max(min_stride)
    } else {
        min_stride.max(1)
    };
    let frame_capacity = if encoded {
        (256 * 1024).max(width.saturating_mul(fmt.height as usize))
    } else {
        (fmt.height as usize)
            .saturating_mul(stride_bytes)
            .max(width.saturating_mul(fmt.height as usize).saturating_mul(3))
    };
    let (pool_min, pool_bytes, pool_spare) =
        crate::capture_api::capture_pool_limits(4, frame_capacity, 8);
    let manager = V4l2MmapManager::new(
        dev.handle(),
        Type::VideoCapture,
        4,
        Duration::from_millis(50),
    )
    .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = bounded(queue_depth);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let mode_clone = mode.clone();
    let backing_tracker = Arc::new(ExternalBackingTracker::new("v4l2_mmap"));
    let manager_for_worker = Arc::clone(&manager);
    let tracker_for_worker = Arc::clone(&backing_tracker);
    let worker = thread::spawn(move || {
        let zero_copy_enabled = supports_v4l2_mmap_zero_copy(mode_clone.format.code);
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let height = mode_clone.format.resolution.height.get() as usize;
        let width = mode_clone.format.resolution.width.get() as usize;
        let min_stride = min_stride_for_fourcc(mode_clone.format.code, width);
        let encoded = is_encoded_bitstream(mode_clone.format.code);
        loop {
            if stop_rx.try_recv().is_ok() {
                let _ = manager_for_worker.stop_stream();
                break;
            }
            match manager_for_worker.dequeue() {
                Ok((index, meta)) => {
                    let mapped_len = manager_for_worker.mapped_bytes(index).unwrap_or_default();
                    let bytes_used = (meta.bytesused as usize).min(mapped_len);
                    let ts = std::time::Duration::from(meta.timestamp)
                        .as_nanos()
                        .min(u64::MAX as u128) as u64;
                    let meta = FrameMeta::new(mode_clone.format, ts)
                        .with_capture_instant(std::time::Instant::now())
                        .with_transition(ResidencyTransition {
                            from: if zero_copy_enabled {
                                FrameResidency::HostExternal
                            } else if encoded {
                                FrameResidency::CompressedPacket
                            } else {
                                FrameResidency::HostOwned
                            },
                            to: if zero_copy_enabled {
                                FrameResidency::HostExternal
                            } else if encoded {
                                FrameResidency::CompressedPacket
                            } else {
                                FrameResidency::HostOwned
                            },
                            reason: ResidencyTransitionReason::Capture,
                            copied: !zero_copy_enabled,
                        })
                        .with_backend(BackendFrameMeta::V4l2(V4l2FrameMeta {
                            sequence: meta.sequence,
                            bytes_used: meta.bytesused,
                            field: meta.field,
                            flags: u32::from(meta.flags),
                            zero_copy: zero_copy_enabled,
                        }));
                    let effective_stride = if encoded {
                        bytes_used.max(1)
                    } else {
                        let inferred_stride = if height > 0 {
                            bytes_used / height
                        } else {
                            bytes_used
                        };
                        if stride_bytes >= min_stride {
                            stride_bytes
                        } else if inferred_stride >= min_stride {
                            inferred_stride
                        } else {
                            min_stride.max(1)
                        }
                    };
                    let Some(layout) = build_v4l2_single_plane_layout(
                        encoded,
                        height,
                        effective_stride,
                        bytes_used,
                    ) else {
                        let _ = manager_for_worker.recycle(index);
                        continue;
                    };
                    let frame = if zero_copy_enabled {
                        let backing = V4l2MmapBacking::new(
                            Arc::clone(&manager_for_worker),
                            index,
                            Arc::clone(&tracker_for_worker),
                            bytes_used,
                        );
                        FrameLease::from_external(meta, smallvec![layout], backing)
                    } else {
                        let mut lease = pool.lease();
                        lease.resize(bytes_used);
                        if let Some(src) = manager_for_worker.mapped_plane(index) {
                            lease.as_mut_slice()[..bytes_used].copy_from_slice(&src[..bytes_used]);
                        }
                        let _ = manager_for_worker.recycle(index);
                        FrameLease::multi_plane(meta, smallvec![lease], smallvec![layout])
                    };
                    match tx.send_timeout(frame, Duration::from_millis(10)) {
                        SendWaitOutcome::Closed(_frame) => {
                            let _ = manager_for_worker.stop_stream();
                            break;
                        }
                        SendWaitOutcome::Timeout(_frame) => {}
                        SendWaitOutcome::Ok => {}
                    }
                }
                Err(err) => {
                    // Timeouts are expected due to the short poll timeout above.
                    if err.kind() != std::io::ErrorKind::TimedOut {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
    });

    Ok(CaptureHandle {
        backend: BackendKind::V4l2,
        control: ControlPlane::V4l2 { path },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        metrics: StageMetrics::default(),
        external_backings: vec![backing_tracker],
    })
}

#[cfg(test)]
mod tests {
    use super::{build_v4l2_single_plane_layout, supports_v4l2_mmap_zero_copy};
    use styx_core::prelude::FourCc;

    #[test]
    fn encoded_layout_uses_bytes_used() {
        let layout = build_v4l2_single_plane_layout(true, 1080, 0, 4096).expect("layout");
        assert_eq!(layout.len, 4096);
        assert_eq!(layout.stride, 4096);
    }

    #[test]
    fn raw_layout_uses_stride_times_height() {
        let layout = build_v4l2_single_plane_layout(false, 2, 6, 12).expect("layout");
        assert_eq!(layout.len, 12);
        assert_eq!(layout.stride, 6);
    }

    #[test]
    fn raw_layout_rejects_short_buffer() {
        assert!(build_v4l2_single_plane_layout(false, 2, 6, 10).is_none());
    }

    #[test]
    fn zero_copy_whitelist_accepts_initial_validated_formats() {
        for code in [
            FourCc::new(*b"MJPG"),
            FourCc::new(*b"JPEG"),
            FourCc::new(*b"YUYV"),
            FourCc::new(*b"RG24"),
            FourCc::new(*b"RGB3"),
            FourCc::new(*b"BGR3"),
            FourCc::new(*b"RGBA"),
            FourCc::new(*b"BGRA"),
        ] {
            assert!(
                supports_v4l2_mmap_zero_copy(code),
                "expected {code} to be whitelisted"
            );
        }
    }

    #[test]
    fn zero_copy_whitelist_rejects_deferred_formats() {
        for code in [
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H264"),
            FourCc::new(*b"BA81"),
        ] {
            assert!(
                !supports_v4l2_mmap_zero_copy(code),
                "expected {code} to use fallback"
            );
        }
    }
}
