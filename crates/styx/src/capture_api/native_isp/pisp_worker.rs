//! The PiSP capture worker: frames leased straight from the back end's output buffers
//! (`pisp_lease`), the second output attached to each frame as a `CompanionKind::Scaled`
//! companion, a `CompanionKind::Pyramid` one (the main output scaled by `2^-level`), the
//! overview or a region, and the regions extra back end passes make (`pisp_regions`) as
//! `CompanionKind::Region` companions of the frame they were cut from.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use styx_capture::prelude::*;
use styx_core::prelude::{CompanionKind, FrameRect};
use styx_core::queue::BoundedTx;
use styx_pipeline::PipelineError;
use styx_pipeline::device::PispPipeline;

use super::super::handle_metrics::deliver;
use super::super::request::CaptureError;
use super::super::tunables::NativeIspConfig;
use super::pisp_lease::{Buffers, Leaser, OutputSpec, Placed, be_crop};
use super::pisp_regions::{Regions, hand_back};
use super::{aaa_sample, err};
use crate::metrics::CaptureMetrics;

/// The outputs a processed `mode` capture delivers with `cfg`: the main one (the mode's
/// format and size unless `cfg` says otherwise) and the optional second one, with the kind of
/// companion it is attached as (the overview, a pyramid level of the main output, a region,
/// else another size).
pub(super) fn output_specs(
    mode: &Mode,
    cfg: &NativeIspConfig,
) -> Result<([Option<OutputSpec>; 2], CompanionKind), CaptureError> {
    let res = mode.format.resolution;
    let (width, height) = cfg
        .output_size
        .unwrap_or((res.width.get(), res.height.get()));
    let main = OutputSpec {
        code: cfg.output_format.unwrap_or(mode.format.code),
        width,
        height,
    };
    main.be_format()?;
    if let Some((width, height)) = cfg.overview {
        if cfg.second_output.is_some() {
            crate::trace::warn!(
                backend = "native",
                "second output disabled: the overview uses the second output"
            );
        }
        let overview = OutputSpec {
            code: main.code,
            width: width.clamp(16, res.width.get()) & !1,
            height: height.clamp(16, res.height.get()) & !1,
        };
        return Ok(([Some(main), Some(overview)], CompanionKind::Overview));
    }
    let level = cfg.pyramid_level.min(3);
    if level > 0 && !cfg.pyramid_pass() {
        if cfg.second_output.is_some() {
            crate::trace::warn!(
                backend = "native",
                level,
                "second output disabled: the pyramid companion uses it"
            );
        }
        // Even sizes, as the chroma planes need.
        let half = |v: u32| ((v >> level) & !1).max(2);
        let pyramid = OutputSpec {
            code: main.code,
            width: half(width),
            height: half(height),
        };
        return Ok((
            [Some(main), Some(pyramid)],
            CompanionKind::Pyramid { level },
        ));
    }
    // The first region, cropped at full resolution by the second output (at the mode's size).
    if let Some(k) = cfg.second_output_region() {
        let region = OutputSpec {
            code: cfg.regions[k].and_then(|r| r.format).unwrap_or(main.code),
            width: res.width.get(),
            height: res.height.get(),
        };
        region.be_format()?;
        let index = k as u8 + 1;
        return Ok(([Some(main), Some(region)], CompanionKind::Region { index }));
    }
    let second = cfg.second_output.map(|((width, height), code)| OutputSpec {
        code,
        width,
        height,
    });
    if let Some(s) = second {
        s.be_format()?;
    }
    Ok(([Some(main), second], CompanionKind::Scaled))
}

/// What the worker needs besides the pipeline.
pub(super) struct Worker {
    pub(super) specs: [Option<OutputSpec>; 2],
    /// What the second output is attached as.
    pub(super) second_kind: CompanionKind,
    pub(super) strides: [usize; 2],
    /// The main output's crop the back end makes now (`OUTPUT_CROP`).
    pub(super) crop: Option<FrameRect>,
    /// The other regions.
    pub(super) regions: Regions,
    pub(super) tx: BoundedTx<FrameLease>,
    /// Whether `tx` was made for this capture (closed when it ends) or is the consumer's.
    pub(super) owns_queue: bool,
    pub(super) stop: mpsc::Receiver<()>,
    pub(super) error: Arc<Mutex<Option<CaptureError>>>,
    pub(super) send_timeout: Duration,
    pub(super) timeout: Duration,
    /// The 3A loop's controls and state.
    pub(super) loop_controls: Arc<super::LoopControls>,
    /// Still requests.
    pub(super) still: super::still_runner::StillRunner,
    /// The capture's metrics.
    pub(super) live: CaptureMetrics,
}

/// Frame `f` as delivered: the main output's lease with its companions (the second output's,
/// the extra passes'). Buffers not leased go back through `returns`. `None`: no main output.
fn frame_leases(
    p: &PispPipeline,
    f: &styx_pipeline::device::PispFrame,
    buffers: &mut Buffers,
    w: &Worker,
    returns: &mpsc::Sender<(usize, u32)>,
) -> Result<Option<FrameLease>, String> {
    let mut leaser = Leaser {
        p,
        f,
        buffers,
        returns,
        live: &w.live,
    };
    let mut unleased = Vec::new();
    let placed = |i: usize, spec: OutputSpec| Placed {
        spec,
        stride: w.strides[i],
        size: None,
        crop: (i == 0).then_some(w.crop).flatten(),
    };
    let mut main = None;
    let mut second = None;
    let mut failed = None;
    for (i, spec) in w.specs.iter().enumerate() {
        let (Some(spec), Some(index)) = (spec, f.job.outputs[i]) else {
            continue;
        };
        let what = match i {
            0 => Some((CompanionKind::Scaled, placed(0, *spec))),
            _ => w.regions.second(w.second_kind, placed(1, *spec)),
        };
        let Some((kind, at)) = what.filter(|_| failed.is_none()) else {
            unleased.push((i, index));
            continue;
        };
        match leaser.lease(at, (i, index)) {
            Ok(l) if i == 0 => main = Some(l),
            Ok(l) => second = Some((kind, l)),
            Err(e) => {
                unleased.push((i, index));
                failed = Some(e);
            }
        }
    }
    let main_placed = w.specs[0].map(|s| placed(0, s));
    let passes = match (main_placed, failed.is_none()) {
        (Some(at), true) => w.regions.pass_companions(&mut leaser, f, at, &mut unleased),
        _ => {
            for pass in f.passes.iter().flatten() {
                unleased.push((pass.output, pass.index));
            }
            Ok(Vec::new())
        }
    };
    hand_back(returns, &unleased);
    if let Some(e) = failed {
        return Err(format!("output buffer: {e}"));
    }
    let passes = passes.map_err(|e| format!("output buffer: {e}"))?;
    let Some(mut frame) = main else {
        return Ok(None);
    };
    for (kind, lease) in second.into_iter().chain(passes) {
        frame = frame
            .with_companion(kind, lease)
            .map_err(|e| e.to_string())?;
    }
    Ok(Some(frame))
}

/// Runs the capture on its own thread until stopped or the queue closes.
pub(super) fn spawn(
    mut p: PispPipeline,
    mut w: Worker,
) -> Result<thread::JoinHandle<()>, CaptureError> {
    let targets = w.still.targets();
    p.set_raw_copy(Some(Box::new(move |s| {
        targets.lock().iter().any(|t| t.wants(s))
    })));
    thread::Builder::new()
        .name("styx-native-pisp".into())
        .spawn(move || {
            w.live.register_thread();
            let tx = &w.tx;
            let (ret_tx, ret_rx) = mpsc::channel::<(usize, u32)>();
            let mut buffers = Buffers::default();
            let mut held_drops = 0u64;
            loop {
                if w.stop.try_recv().is_ok() {
                    break;
                }
                while let Ok((i, index)) = ret_rx.try_recv() {
                    p.release_output(i, index);
                }
                if let Some(c) = w.loop_controls.take() {
                    p.controller().set_controls(c);
                }
                w.still.before_frame(&mut p);
                if let Some(crop) = w.loop_controls.crop.take() {
                    match p.set_output_crop(0, crop.map(be_crop)) {
                        Ok(()) => w.crop = crop,
                        Err(e) => {
                            crate::trace::warn!(backend = "native", error = %e, "output crop refused");
                            w.loop_controls.crop.revert(w.crop);
                        }
                    }
                }
                w.regions.apply(&mut p, &w.loop_controls.regions, w.crop);
                let mut f = match p.next(w.timeout) {
                    Ok(f) => f,
                    // Consumers hold every output buffer: this frame is dropped (never wait
                    // for them: the camera keeps running), the next one after a buffer comes
                    // back is processed.
                    Err(PipelineError::OutputsHeld(output)) => {
                        held_drops += 1;
                        w.live.isp_skipped();
                        if held_drops == 1 {
                            crate::trace::info!(
                                backend = "native",
                                output,
                                "consumers hold every back end buffer: dropping frames until one is released"
                            );
                        } else {
                            crate::trace::debug!(backend = "native", output, held_drops, "frame dropped: buffers held");
                        }
                        continue;
                    }
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
                w.loop_controls.report(&p.step().params);
                let step = p.step();
                let ae = (step.params.ae.total_exposure, step.params.ae.locked);
                let request = step.sensor;
                let raw = f.raw.take();
                w.still
                    .after_frame(&mut p, &f.sensor, (f.request_lands, request), ae, raw);
                w.live.isp_time(f.times.be_job, f.times.total);
                w.live
                    .aaa(&aaa_sample(&p.step().params, p.controls(), f.sequence));
                let frame = match frame_leases(&p, &f, &mut buffers, &w, &ret_tx) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
                if deliver(&w.live, tx, frame, "native-pisp", w.send_timeout) {
                    break;
                }
            }
            // Leases still out return their buffers to a closed channel; the back end frees
            // its buffers once they are dropped.
            if let Err(e) = p.close() {
                crate::trace::warn!(backend = "native", error = %e, "closing the PiSP path");
            }
            if w.owns_queue {
                tx.close();
            }
        })
        .map_err(|e| err(format!("worker: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_specs_follow_the_config() {
        let res = Resolution::new(1280, 800).unwrap();
        let format = MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb);
        let mode = Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: Default::default(),
            interval_stepwise: None,
        };
        let (specs, kind) = output_specs(&mode, &NativeIspConfig::default()).unwrap();
        assert_eq!(specs[0].unwrap().code, FourCc::NV12);
        assert!(specs[1].is_none());
        assert_eq!(kind, CompanionKind::Scaled);
        let cfg = NativeIspConfig {
            output_size: Some((640, 400)),
            output_format: Some(FourCc::RG24),
            second_output: Some(((320, 200), FourCc::NV12)),
            ..Default::default()
        };
        let (specs, _) = output_specs(&mode, &cfg).unwrap();
        assert_eq!(
            (specs[0].unwrap().width, specs[0].unwrap().code),
            (640, FourCc::RG24)
        );
        assert_eq!(specs[1].unwrap().height, 200);
        let bad = NativeIspConfig {
            output_format: Some(FourCc::YUYV),
            ..Default::default()
        };
        assert!(output_specs(&mode, &bad).is_err());
        // A pyramid level takes the second output: the main output halved, in its format.
        let pyramid = NativeIspConfig {
            pyramid_level: 1,
            second_output: Some(((320, 200), FourCc::RG24)),
            ..Default::default()
        };
        let (specs, kind) = output_specs(&mode, &pyramid).unwrap();
        assert_eq!(kind, CompanionKind::Pyramid { level: 1 });
        // An overview takes the second output: the whole frame at its size, in the main format.
        let overview = NativeIspConfig {
            overview: Some((321, 200)),
            pyramid_level: 1,
            ..Default::default()
        };
        let (o, kind) = output_specs(&mode, &overview).unwrap();
        assert_eq!(kind, CompanionKind::Overview);
        let o = o[1].unwrap();
        assert_eq!((o.width, o.height, o.code), (320, 200, FourCc::NV12));
        let s = specs[1].unwrap();
        assert_eq!((s.width, s.height, s.code), (640, 400, FourCc::NV12));
        let quarter = NativeIspConfig {
            output_size: Some((1280, 720)),
            pyramid_level: 2,
            ..Default::default()
        };
        let (specs, kind) = output_specs(&mode, &quarter).unwrap();
        assert_eq!(kind, CompanionKind::Pyramid { level: 2 });
        assert_eq!(
            (specs[1].unwrap().width, specs[1].unwrap().height),
            (320, 180)
        );
    }
}
