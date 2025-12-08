use std::thread;
use std::time::Duration;

use smallvec::smallvec;
use styx_core::prelude::*;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::{buffer::Type, format::FourCC, prelude::*, video::Capture as _};

use crate::capture_api::controls::apply_v4l2_controls;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

fn is_encoded_bitstream(code: FourCc) -> bool {
    matches!(
        &code.to_u32().to_le_bytes(),
        b"H264" | b"H265" | b"HEVC" | b"MJPG" | b"JPEG"
    )
}

fn min_stride_for_fourcc(code: FourCc, width: usize) -> usize {
    match &code.to_u32().to_le_bytes() {
        // MIPI packed RAW10/RAW12 bayer.
        b"pBAA" | b"pGAA" | b"pgAA" | b"pRAA" => ((width + 3) / 4) * 5,
        b"pBCC" | b"pGCC" | b"pgCC" | b"pRCC" => ((width + 1) / 2) * 3,

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
    let height = fmt.height as usize;
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
        // Compressed bitstreams vary widely in size; pick a reasonable default chunk size and
        // let `bytesused` drive the actual per-frame allocation.
        (256 * 1024).max(width.saturating_mul(height))
    } else {
        height
            .saturating_mul(stride_bytes)
            .max(width.saturating_mul(height).saturating_mul(3))
    };
    let (pool_min, pool_bytes, pool_spare) =
        crate::capture_api::capture_pool_limits(4, frame_capacity, 8);
    let mut stream = Stream::with_buffers(&dev, Type::VideoCapture, 4)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    // `stream.next()` blocks inside poll() by default. Give it a short timeout so the worker can
    // observe stop signals quickly (otherwise stop/join can hang forever and leak resources).
    stream.set_timeout(Duration::from_millis(50));
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = bounded(queue_depth);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let mode_clone = mode.clone();
    let worker = thread::spawn(move || {
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let height = mode_clone.format.resolution.height.get() as usize;
        let width = mode_clone.format.resolution.width.get() as usize;
        let min_stride = min_stride_for_fourcc(mode_clone.format.code, width);
        let encoded = is_encoded_bitstream(mode_clone.format.code);
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match stream.next() {
                Ok((buf, meta)) => {
                    let bytes_used = (meta.bytesused as usize).min(buf.len());
                    let mut lease = pool.lease();
                    lease.resize(bytes_used);
                    lease
                        .as_mut_slice()
                        .get_mut(..bytes_used)
                        .unwrap()
                        .copy_from_slice(&buf[..bytes_used]);

                    let ts = std::time::Duration::from(meta.timestamp)
                        .as_nanos()
                        .min(u64::MAX as u128) as u64;
                    let meta = FrameMeta::new(mode_clone.format, ts);
                    let stride = if encoded {
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
                    let layout = PlaneLayout {
                        offset: 0,
                        len: bytes_used,
                        stride,
                    };
                    let frame = FrameLease::multi_plane(meta, smallvec![lease], smallvec![layout]);
                    if matches!(tx.send(frame), SendOutcome::Closed) {
                        break;
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
        metrics: StageMetrics::default(),
    })
}
