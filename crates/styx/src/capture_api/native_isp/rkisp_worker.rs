//! The Rockchip ISP path's worker: each main path frame (NV12) copied into a recycled heap
//! buffer, the 3A loop run on its statistics. The copy keeps the ISP's few buffers moving
//! while consumers hold frames; handing the driver's buffers out as dma-bufs is the next step
//! once the path has run on a board.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use styx_capture::prelude::*;
use styx_core::prelude::{ClockSource, Hop};
use styx_core::queue::BoundedTx;
use styx_native::{NativeCamera, StreamSettings};
use styx_pipeline::device::RkispPipeline;
use styx_pipeline::styx_algo::Tuning;

use super::super::handle_metrics::deliver;
use super::super::request::CaptureError;
use super::soft_worker::{image_bytes, lease, reuse};
use super::{aaa_sample, err, frame_meta, native_aaa};
use crate::metrics::CaptureMetrics;

/// What the worker needs besides the pipeline.
pub(super) struct RkispWorker {
    pub(super) mode: Mode,
    /// Spare output buffers kept for reuse (`NativeIspConfig::output_buffers`).
    pub(super) keep: usize,
    pub(super) tx: BoundedTx<FrameLease>,
    /// Whether `tx` was made for this capture (closed when it ends) or is the consumer's.
    pub(super) owns_queue: bool,
    pub(super) stop: mpsc::Receiver<()>,
    pub(super) error: Arc<Mutex<Option<CaptureError>>>,
    pub(super) send_timeout: Duration,
    pub(super) timeout: Duration,
    pub(super) loop_controls: Arc<super::LoopControls>,
    pub(super) still: super::still_runner::StillRunner,
    pub(super) live: CaptureMetrics,
    /// The clock the frames are stamped in.
    pub(super) clock: ClockSource,
    /// The camera's controls (set by [`open`]).
    pub(super) controls: Option<styx_native::CameraControls>,
}

/// Copies an NV12 image of `w`x`h` at `stride` (chroma after `h` rows) into `out`, tightly
/// packed; `false` when `src` is short.
fn copy_nv12(src: &[u8], (w, h): (u32, u32), stride: u32, out: &mut [u8]) -> bool {
    let (w, h, stride) = (w as usize, h as usize, stride as usize);
    let rows = h + h / 2;
    if stride < w || src.len() < stride * (rows - 1) + w || out.len() < w * rows {
        return false;
    }
    for (row, dst) in out.chunks_exact_mut(w).take(rows).enumerate() {
        dst.copy_from_slice(&src[row * stride..row * stride + w]);
    }
    true
}

/// Opens `camera` through the ISP at the worker's mode (sensor code `code`, frame interval
/// and tuning in `loop_setup`), applies the loop's initial controls, starts it and spawns the
/// worker.
pub(super) fn open(
    (code, camera): (Option<u32>, NativeCamera),
    (interval, tuning, loop_controls): (
        Option<styx_native::Fraction>,
        &Tuning,
        &super::LoopControls,
    ),
    config: &super::super::tunables::StyxConfig,
    mut w: RkispWorker,
) -> Result<(styx_native::CameraControls, thread::JoinHandle<()>), CaptureError> {
    let res = w.mode.format.resolution;
    let (width, height) = (res.width.get(), res.height.get());
    let settings = StreamSettings {
        width,
        height,
        fourcc: None,
        code,
        interval,
        hflip: config.backends.native.hflip,
        vflip: config.backends.native.vflip,
    };
    let mut p = RkispPipeline::open(camera, &settings, tuning, (width, height)).map_err(err)?;
    if let Some(c) = loop_controls.take() {
        p.algorithms().set_controls(c);
    }
    p.start().map_err(err)?;
    let controls = p.controls().clone();
    w.controls = Some(controls.clone());
    Ok((controls, spawn(p, w)?))
}

/// Runs the capture on its own thread until stopped or the queue closes.
fn spawn(mut p: RkispPipeline, mut w: RkispWorker) -> Result<thread::JoinHandle<()>, CaptureError> {
    let code = w.mode.format.code;
    let res = w.mode.format.resolution;
    let size = (res.width.get(), res.height.get());
    let ((out_w, out_h), stride) = p
        .output_layout()
        .ok_or_else(|| err("the ISP is not open"))?;
    if code != FourCc::NV12 || (out_w, out_h) != size {
        return Err(err(format!(
            "the main path gives NV12 {out_w}x{out_h}, the mode is {code} {}x{}",
            size.0, size.1
        )));
    }
    let len = image_bytes(code, size);
    let controls = w.controls.take().unwrap_or_else(|| p.controls().clone());
    thread::Builder::new()
        .name("styx-native-rkisp".into())
        .spawn(move || {
            w.live.register_thread();
            let (ret_tx, ret_rx) = mpsc::channel::<Vec<u8>>();
            let mut spare = Vec::new();
            loop {
                if w.stop.try_recv().is_ok() {
                    break;
                }
                if let Some(c) = w.loop_controls.take() {
                    p.algorithms().set_controls(c);
                }
                w.still.before_frame(&mut p);
                let f = match p.next(w.timeout) {
                    Ok(f) => f,
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
                let isp_done = styx_core::prelude::CaptureInstant::now();
                let mut buf = reuse(&ret_rx, &mut spare, w.keep, len);
                let copied = copy_nv12(p.data(&f), size, stride, &mut buf);
                if let Err(e) = p.release(&f) {
                    *w.error.lock() = Some(err(e));
                    break;
                }
                if !copied || f.output.error {
                    w.live.isp_skipped();
                    continue;
                }
                styx_core::metrics::copied(styx_core::metrics::CopySite::Capture, len);
                let step = p.algorithms().step();
                let params = step.params.clone();
                let (ae, request) = ((params.ae.total_exposure, params.ae.locked), step.sensor);
                let wb = step.isp.wb;
                w.loop_controls.report(&params);
                w.loop_controls.processed(f.sensor.frame);
                let sample = aaa_sample(&params, &controls, f.sensor.frame);
                w.live.aaa(&sample);
                let mut meta = frame_meta(
                    &w.mode,
                    f.sensor.frame,
                    f.output.timestamp,
                    &f.sensor,
                    super::super::native_backend::native_conversion(w.clock),
                );
                if let Some(BackendFrameMeta::Native(n)) = meta.backend.as_mut() {
                    n.aaa = Some(native_aaa(&sample, wb));
                }
                meta.hops.set(Hop::IspDone, isp_done.as_nanos());
                // No raw frames on this path: a still request's raw frame never comes, and
                // the still runner times it out.
                w.still
                    .after_frame(&mut p, &f.sensor, (f.request_lands, request), ae, None);
                let lease = lease(meta, size, buf, &ret_tx, &w.live);
                if deliver(&w.live, &w.tx, lease, "native-rkisp", w.send_timeout) {
                    break;
                }
            }
            if let Err(e) = p.close() {
                crate::trace::warn!(backend = "native", error = %e, "closing the Rockchip ISP path");
            }
            if w.owns_queue {
                w.tx.close();
            }
        })
        .map_err(|e| err(format!("worker: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_rows_are_packed_from_the_stride() {
        let (w, h, stride) = (4u32, 2u32, 6u32);
        let src: Vec<u8> = (0..stride as u8 * 3).collect();
        let mut out = vec![0u8; 4 * 3];
        assert!(copy_nv12(&src, (w, h), stride, &mut out));
        assert_eq!(out, [0, 1, 2, 3, 6, 7, 8, 9, 12, 13, 14, 15]);
        assert!(
            !copy_nv12(&src[..10], (w, h), stride, &mut out),
            "short source"
        );
    }
}
