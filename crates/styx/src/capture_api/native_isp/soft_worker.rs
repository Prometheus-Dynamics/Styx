//! The software ISP path's worker: each raw frame through the 3A loop into recycled heap
//! buffers. With a crop (`OUTPUT_CROP`) the ISP processes only that region of the frame, at
//! full resolution, and the regions (`NativeIspConfig::regions`, moved by `region_crop(i)`) in
//! the same pass, each attached as a `CompanionKind::Region` companion; with an overview the
//! whole frame binned (`NativeIspConfig::overview`), from whose pass the statistics come;
//! without any the whole frame. A region and an overview of
//! a 1280x800 frame cost a fraction of the whole frame (`docs/native-stack/pipeline.md`).

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use styx_capture::prelude::*;
use styx_core::prelude::{ClockSource, CompanionKind, FrameRect, Hop};
use styx_core::queue::BoundedTx;
use styx_pipeline::device::SoftPipeline;
use styx_pipeline::{SoftParts, SoftTarget};
use styx_softisp::{OutputBuffers, Scale, Window};

use super::super::handle_metrics::deliver;
use super::super::request::CaptureError;
use super::super::tunables::MAX_NATIVE_REGIONS;
use super::{HeapBacking, aaa_sample, err, frame_meta, hold_soft, layouts, native_aaa};
use crate::metrics::CaptureMetrics;

/// What the worker needs besides the pipeline.
pub(super) struct SoftWorker {
    pub(super) mode: Mode,
    /// The ISP's scale: half for a binned mode.
    pub(super) scale: Scale,
    /// The capture can crop: a full-size mode (the CPU or the GPU ISP processes the region).
    pub(super) crops: bool,
    /// The overview's binning factor.
    pub(super) overview: Option<u32>,
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
    /// The camera's controls (the lens position for the metrics).
    pub(super) controls: styx_native::CameraControls,
    /// Attach each frame's raw frame (`NativeIspConfig::sensor_raw`): 16-bit Bayer, the
    /// sensor's value in the top bits, as the PiSP path hands out (a copy here).
    pub(super) sensor_raw: bool,
}

/// `f`'s raw frame as 16-bit Bayer with the sensor's value in the top bits (`BYR2` and
/// relatives, by the frame's colour filter order), with `meta` but that format.
fn raw16(
    f: &styx_pipeline::device::SoftFrame,
    format: styx_softisp::RawFormat,
    meta: &FrameMeta,
) -> Option<FrameLease> {
    let (w, h) = (format.width as usize, format.height as usize);
    let code = FourCc::new(*match format.pattern {
        styx_softisp::CfaPattern::Bggr => b"BYR2",
        styx_softisp::CfaPattern::Gbrg => b"GB16",
        styx_softisp::CfaPattern::Grbg => b"GR16",
        styx_softisp::CfaPattern::Rggb => b"RG16",
    });
    let shift = 16u32.saturating_sub(u32::from(format.packing.bit_depth()));
    let stride = f.raw.stride as usize;
    let data = f.raw.data();
    let mut row = vec![0u16; w];
    let mut bytes = Vec::with_capacity(w * h * 2);
    for y in 0..h {
        let line = data.get(y * stride..)?;
        styx_pipeline::rawrec::unpack_row(format.packing, line, w, &mut row);
        bytes.extend(row.iter().flat_map(|v| (v << shift).to_le_bytes()));
    }
    styx_core::metrics::copied(styx_core::metrics::CopySite::Raw, bytes.len());
    let format = MediaFormat::new(
        code,
        Resolution::new(format.width, format.height)?,
        ColorSpace::Unknown,
    );
    let mut lease = FrameLease::from_visible_bytes(format, meta.timestamp, &bytes).ok()?;
    *lease.meta_mut() = FrameMeta {
        format,
        ..meta.clone()
    };
    Some(lease)
}

/// Bytes per row of a `code` image `w` pixels wide in the path's buffers.
fn row_bytes(code: FourCc, w: u32) -> usize {
    if code == FourCc::NV12 {
        w as usize
    } else {
        w as usize * 3
    }
}

/// Bytes of a `code` image of `w`x`h`.
fn image_bytes(code: FourCc, (w, h): (u32, u32)) -> usize {
    layouts(code, h as usize, row_bytes(code, w))
        .iter()
        .map(|l| l.offset + l.len)
        .max()
        .unwrap_or(0)
}

/// Buffers for a `code` image of `w`x`h` in `buf`.
fn buffers(code: FourCc, (w, h): (u32, u32), buf: &mut [u8]) -> OutputBuffers<'_> {
    let stride = row_bytes(code, w);
    if code == FourCc::NV12 {
        let (y, uv) = buf.split_at_mut(stride * h as usize);
        OutputBuffers::Nv12 {
            y,
            y_stride: stride,
            uv,
            uv_stride: stride,
        }
    } else {
        OutputBuffers::Rgb24 { data: buf, stride }
    }
}

/// A lease of `buf`, a `code` image of `size` with `meta`, recycled through `returns`.
fn lease(
    mut meta: FrameMeta,
    size: (u32, u32),
    buf: Vec<u8>,
    returns: &mpsc::Sender<Vec<u8>>,
    live: &CaptureMetrics,
) -> FrameLease {
    let code = meta.format.code;
    let res = Resolution::new(size.0, size.1).expect("non-zero image size");
    meta.format = MediaFormat::new(code, res, meta.format.color);
    FrameLease::from_external(
        meta,
        layouts(code, size.1 as usize, row_bytes(code, size.0)),
        Arc::new(live.track(HeapBacking {
            data: Some(buf),
            returns: returns.clone(),
        })),
    )
}

/// The software ISP's copies of a frame: raw rows staged out of uncached receiver buffers, and
/// a GPU ISP's upload and read-back of the frame.
fn count_isp_copies(p: &mut SoftPipeline, f: &styx_pipeline::device::SoftFrame, out_bytes: usize) {
    use styx_core::metrics::{CopySite, copied};
    let soft = p.soft_loop();
    if soft.copies_input() {
        let raw = f.raw.stride as usize * f.raw.height as usize;
        copied(CopySite::SoftIsp, raw);
    }
    if matches!(soft.engine(), styx_pipeline::IspEngine::Gpu { .. }) {
        copied(CopySite::SoftIsp, out_bytes);
    }
}

/// Runs the capture on its own thread until stopped or the queue closes.
pub(super) fn spawn(
    mut p: SoftPipeline,
    mut w: SoftWorker,
) -> Result<thread::JoinHandle<()>, CaptureError> {
    let code = w.mode.format.code;
    let res = w.mode.format.resolution;
    let frame = (res.width.get(), res.height.get());
    let len = image_bytes(code, frame);
    let overview = w
        .overview
        .map(|f| (f, crate::planner::soft_overview_size(frame, f)))
        .filter(|(_, (ow, oh))| *ow > 0 && *oh > 0);
    thread::Builder::new()
        .name("styx-native-softisp".into())
        .spawn(move || {
            w.live.register_thread();
            let (ret_tx, ret_rx) = mpsc::channel::<Vec<u8>>();
            let (ov_tx, ov_rx) = mpsc::channel::<Vec<u8>>();
            let mut crop: Option<FrameRect> = None;
            // Each region slot's rectangle, and its buffers coming back.
            let mut regions: [Option<FrameRect>; MAX_NATIVE_REGIONS] = [None; MAX_NATIVE_REGIONS];
            let region_returns: Vec<_> = (0..MAX_NATIVE_REGIONS)
                .map(|_| mpsc::channel::<Vec<u8>>())
                .collect();
            'frames: loop {
                if w.stop.try_recv().is_ok() {
                    break;
                }
                if let Some(c) = w.loop_controls.take() {
                    p.soft_loop().set_controls(c);
                }
                if w.crops {
                    if let Some(c) = w.loop_controls.crop.take() {
                        crop = c;
                    }
                    for (k, region) in regions.iter_mut().enumerate() {
                        if let Some(c) = w.loop_controls.regions.slot(k).and_then(|c| c.take()) {
                            *region = c;
                        }
                    }
                }
                w.still.before_frame(&mut p);
                let size = crop.map_or(frame, |c| (c.width, c.height));
                let mut buf = ret_rx.try_recv().unwrap_or_else(|_| vec![0u8; len]);
                let mut ov_buf = overview.map(|(_, s)| {
                    ov_rx
                        .try_recv()
                        .unwrap_or_else(|_| vec![0u8; image_bytes(code, s)])
                });
                let mut region_bufs: Vec<(usize, FrameRect, Vec<u8>)> = regions
                    .iter()
                    .enumerate()
                    .filter_map(|(k, r)| r.map(|r| (k, r)))
                    .map(|(k, r)| {
                        let bytes = image_bytes(code, (r.width, r.height));
                        let mut b = region_returns[k]
                            .1
                            .try_recv()
                            .unwrap_or_else(|_| vec![0u8; bytes]);
                        b.resize(bytes, 0);
                        (k, r, b)
                    })
                    .collect();
                let out = buffers(code, size, &mut buf);
                let window = |c: FrameRect| Window::new(c.x, c.y, c.width, c.height);
                let target = match (crop, overview, ov_buf.as_mut()) {
                    (None, None, _) if region_bufs.is_empty() => SoftTarget::Frame(w.scale, out),
                    (crop, overview, ov_buf) => {
                        let c = crop.unwrap_or(FrameRect::new(0, 0, frame.0, frame.1));
                        let mut parts = vec![(window(c), out)];
                        parts.extend(region_bufs.iter_mut().map(|(_, r, b)| {
                            (window(*r), buffers(code, (r.width, r.height), b))
                        }));
                        SoftTarget::Parts(SoftParts {
                            regions: parts,
                            overview: overview
                                .zip(ov_buf)
                                .map(|((f, s), b)| (f, buffers(code, s, b))),
                        })
                    }
                };
                let f = match p.next_target(w.timeout, target) {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
                let isp_done = styx_core::prelude::CaptureInstant::now();
                count_isp_copies(&mut p, &f, image_bytes(code, size));
                w.loop_controls.report(&f.output.step.params);
                w.loop_controls.processed(f.sensor.frame);
                let raw = w
                    .still
                    .wants(&f.sensor)
                    .then(|| hold_soft(&f, p.soft_loop().format()));
                let step = &f.output.step;
                let ae = (step.params.ae.total_exposure, step.params.ae.locked);
                let request = step.sensor;
                let (sensor, lands) = (f.sensor, f.request_lands);
                let t = &f.output.timing;
                w.live
                    .isp_time(t.isp, t.settings + t.isp + t.stats + t.algorithms);
                let sample = aaa_sample(&f.output.step.params, &w.controls, f.sensor.frame);
                w.live.aaa(&sample);
                let mut meta = frame_meta(
                    &w.mode,
                    f.sensor.frame,
                    f.raw.timestamp,
                    &f.sensor,
                    super::super::native_backend::native_conversion(w.clock),
                );
                let dequeued = styx_core::prelude::CaptureInstant::from(f.raw.dequeued);
                if let Some(BackendFrameMeta::Native(n)) = meta.backend.as_mut() {
                    n.frame_end_ns = dequeued.as_nanos();
                    n.aaa = Some(native_aaa(&sample, f.output.applied.wb));
                }
                meta.hops.set(Hop::Dequeued, dequeued.as_nanos());
                meta.hops.set(Hop::IspDone, isp_done.as_nanos());
                // The raw frame for a raw consumer, before its buffer goes back.
                let raw_frame = w
                    .sensor_raw
                    .then(|| raw16(&f, p.soft_loop().format(), &meta))
                    .flatten();
                drop(f);
                w.still
                    .after_frame(&mut p, &sensor, (lands, request), ae, raw);
                let mut main_meta = meta.clone();
                main_meta.crop = crop;
                let mut lease = lease(main_meta, size, buf, &ret_tx, &w.live);
                for (k, r, b) in region_bufs {
                    let mut region_meta = meta.clone();
                    region_meta.crop = Some(r);
                    let region = self::lease(
                        region_meta,
                        (r.width, r.height),
                        b,
                        &region_returns[k].0,
                        &w.live,
                    );
                    let kind = CompanionKind::Region { index: k as u8 + 1 };
                    lease = match lease.with_companion(kind, region) {
                        Ok(l) => l,
                        Err(e) => {
                            *w.error.lock() = Some(err(e));
                            break 'frames;
                        }
                    };
                }
                if let (Some((_, s)), Some(b)) = (overview, ov_buf) {
                    let ov = self::lease(meta, s, b, &ov_tx, &w.live);
                    lease = match lease.with_companion(CompanionKind::Overview, ov) {
                        Ok(l) => l,
                        Err(e) => {
                            *w.error.lock() = Some(err(e));
                            break;
                        }
                    };
                }
                if let Some(raw) = raw_frame {
                    lease = match lease.with_companion(CompanionKind::SensorRaw, raw) {
                        Ok(l) => l,
                        Err(e) => {
                            *w.error.lock() = Some(err(e));
                            break;
                        }
                    };
                }
                if deliver(&w.live, &w.tx, lease, "native-softisp", w.send_timeout) {
                    break;
                }
            }
            if let Err(e) = p.close() {
                crate::trace::warn!(backend = "native", error = %e, "closing the software ISP path");
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
    fn region_buffers_have_the_regions_rows() {
        assert_eq!(image_bytes(FourCc::NV12, (320, 200)), 320 * 300);
        assert_eq!(image_bytes(FourCc::RG24, (320, 200)), 320 * 600);
        let mut b = vec![0u8; 320 * 300];
        let OutputBuffers::Nv12 { y, uv, .. } = buffers(FourCc::NV12, (320, 200), &mut b) else {
            panic!("NV12");
        };
        assert_eq!((y.len(), uv.len()), (320 * 200, 320 * 100));
    }
}
