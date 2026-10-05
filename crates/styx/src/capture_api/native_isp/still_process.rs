//! The still thread of a processed native capture: held raw frames reprocessed at full
//! quality (the PiSP back end on node group 1, else the software ISP with the MHC demosaic),
//! encoded, and written as DNGs, away from the capture's worker so the stream keeps its rate.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use styx_capture::prelude::FourCc;
use styx_pipeline::device::{IspKind, StillBackEnd};
use styx_pipeline::still::{HeldRaw, StillPixels, StillSource, dng_metadata, soft_still};

use super::super::request::CaptureError;
use super::super::still::{
    StillCapture, StillFormat, StillImage, StillMeta, StillRequest, StillShot,
};
use super::super::still_output::{bayer16_code, encode_jpeg, nv12_to_rgb, thumbnail};
use super::still_runner::StillJob;

/// A held frame for one shot (the decisions' [`styx_pipeline::still_runner::HeldShot`]).
pub(crate) type HeldShot = styx_pipeline::still_runner::HeldShot<Box<HeldRaw>>;

/// One request's held frames.
pub(crate) struct Batch {
    pub job: StillJob,
    pub shots: Vec<HeldShot>,
}

/// What the still thread needs to know about the camera.
pub(crate) struct StillContext {
    pub kind: IspKind,
    pub source: StillSource,
    pub threads: usize,
}

/// The back end node group stills use (the stream's is 0).
const STILL_BE_GROUP: usize = 1;
/// Widest DNG preview.
const PREVIEW_WIDTH: usize = 640;

/// The still thread.
pub(crate) struct StillProcessor {
    tx: mpsc::Sender<Batch>,
}

fn err(msg: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("still: {msg}"))
}

impl StillProcessor {
    /// Starts the thread; it ends when the processor is dropped.
    pub(crate) fn spawn(ctx: Arc<StillContext>) -> Self {
        let (tx, rx) = mpsc::channel::<Batch>();
        let spawned = thread::Builder::new()
            .name("styx-still".into())
            .spawn(move || {
                let mut be = BackEnd::default();
                while let Ok(batch) = rx.recv() {
                    let mut batch = batch;
                    let request = batch.job.request.clone();
                    let shots = batch
                        .shots
                        .iter_mut()
                        .map(|s| process(&ctx, &mut be, &request, s))
                        .collect::<Result<Vec<_>, _>>();
                    let reply = shots.map(|shots| StillCapture {
                        shots,
                        latency: batch.job.requested.elapsed(),
                    });
                    if let Ok(c) = &reply {
                        crate::trace::info!(
                            backend = "native",
                            shots = c.shots.len(),
                            latency_ms = c.latency.as_secs_f64() * 1e3,
                            "still ready"
                        );
                    }
                    let _ = batch.job.reply.send(reply);
                }
                if let Some(b) = be.dev.take()
                    && let Err(e) = b.close()
                {
                    crate::trace::warn!(backend = "native", error = %e, "closing the still back end");
                }
            });
        if let Err(e) = spawned {
            crate::trace::warn!(backend = "native", error = %e, "no still thread");
        }
        Self { tx }
    }

    /// Hands a request's held frames over.
    pub(crate) fn submit(&self, batch: Batch) {
        if let Err(mpsc::SendError(b)) = self.tx.send(batch) {
            let _ = b.job.reply.send(Err(err("the still thread is gone")));
        }
    }
}

/// The still back end, opened on first use; not tried again once it failed.
#[derive(Default)]
struct BackEnd {
    dev: Option<StillBackEnd>,
    failed: bool,
}

/// `raw` processed into `pixels` with `settings`, and what processed it.
fn reprocess(
    ctx: &StillContext,
    be: &mut BackEnd,
    raw: &HeldRaw,
    settings: &styx_pipeline::IspSettings,
    pixels: StillPixels,
) -> Result<(Vec<u8>, &'static str), CaptureError> {
    let raw16 = raw.packing == (styx_softisp::RawPacking::U16Le { bits: 16 });
    if ctx.kind == IspKind::Pisp && raw16 && !be.failed {
        if be.dev.as_ref().is_some_and(|d| !d.fits(raw, pixels))
            && let Some(old) = be.dev.take()
        {
            let _ = old.close();
        }
        let opened = match be.dev.take() {
            Some(d) => Ok(d),
            None => StillBackEnd::open(STILL_BE_GROUP, raw, pixels),
        };
        match opened.and_then(|mut d| {
            let out = d.process(raw, settings, Duration::from_secs(1));
            be.dev = Some(d);
            out
        }) {
            Ok((bytes, took)) => {
                crate::trace::debug!(
                    backend = "native",
                    job_ms = took.as_secs_f64() * 1e3,
                    "still back end job"
                );
                return Ok((bytes, "pisp"));
            }
            Err(e) => {
                crate::trace::warn!(backend = "native", error = %e, "still back end unavailable; software ISP");
                be.failed = true;
                if let Some(d) = be.dev.take() {
                    let _ = d.close();
                }
            }
        }
    }
    let out = soft_still(raw, settings, pixels, ctx.threads).map_err(err)?;
    Ok((out, "software"))
}

fn process(
    ctx: &StillContext,
    be: &mut BackEnd,
    request: &StillRequest,
    shot: &mut HeldShot,
) -> Result<StillShot, CaptureError> {
    let t = Instant::now();
    if let Some(want) = shot.want {
        // A fixed or bracketed still keeps its own exposure (not the stream's digital gain).
        let raw = &mut shot.raw;
        raw.isp.digital_gain = styx_pipeline::still_runner::fixed_exposure_gain(
            want,
            &raw.sensor,
            raw.params.colour_gains[1],
        );
    }
    let raw = &shot.raw;
    let (w, h) = (raw.width, raw.height);
    let settings = raw.isp.clone().with_spatial_denoise(request.denoise);
    let want_preview = request.dng && request.dng_preview;
    let pixels = match request.format {
        StillFormat::Nv12 => Some(StillPixels::Nv12),
        StillFormat::Raw if !want_preview => None,
        _ => Some(StillPixels::Rgb24),
    };
    let processed = match pixels {
        Some(p) => Some((p, reprocess(ctx, be, raw, &settings, p)?)),
        None => None,
    };
    let isp = processed.as_ref().map_or("none", |(_, (_, name))| *name);
    let rgb = |p: &(StillPixels, (Vec<u8>, &str))| match p.0 {
        StillPixels::Rgb24 => p.1.0.clone(),
        StillPixels::Nv12 => nv12_to_rgb(&p.1.0, w as usize, h as usize),
    };
    let image = |format: FourCc, data: Vec<u8>| StillImage {
        format,
        width: w,
        height: h,
        data,
    };
    let raw_image = if request.dng || request.format == StillFormat::Raw {
        Some(raw.raw_image().map_err(err)?)
    } else {
        None
    };
    let still = match (request.format, &processed) {
        (StillFormat::Jpeg { quality }, Some(p)) => Some(image(
            FourCc::MJPG,
            encode_jpeg(&p.1.0, (w, h), quality, request.encoder.as_ref())?,
        )),
        (StillFormat::Rgb24, Some(p)) => Some(image(FourCc::RG24, p.1.0.clone())),
        (StillFormat::Nv12, Some(p)) => Some(image(FourCc::NV12, p.1.0.clone())),
        (StillFormat::Raw, _) => raw_image.as_ref().map(|r| {
            let colors = match r.layout {
                styx_dng::SampleLayout::Cfa(c) => c.colors(),
                styx_dng::SampleLayout::Mono => [1; 4],
            };
            image(
                bayer16_code(colors, r.bits),
                r.samples.iter().flat_map(|v| v.to_le_bytes()).collect(),
            )
        }),
        _ => None,
    };
    let dng = match (&raw_image, request.dng) {
        (Some(img), true) => {
            let preview = processed.as_ref().filter(|_| want_preview).map(|p| {
                let (rgb, tw, th) = thumbnail(&rgb(p), w as usize, h as usize, PREVIEW_WIDTH);
                styx_dng::Preview {
                    width: tw,
                    height: th,
                    rgb,
                }
            });
            let meta = dng_metadata(
                raw,
                &ctx.source,
                request.dng_lens_shading,
                SystemTime::now(),
                preview,
            );
            Some(styx_dng::write_dng(img, &meta).map_err(err)?)
        }
        _ => None,
    };
    let s = &raw.sensor;
    let p = &raw.params;
    let landed = shot.landed(raw.sequence, s);
    Ok(StillShot {
        image: still,
        dng,
        meta: StillMeta {
            sequence: raw.sequence,
            timestamp_ns: raw.timestamp.as_nanos() as u64,
            exposure: s.exposure,
            analogue_gain: s.analogue_gain,
            sensor_digital_gain: s.digital_gain,
            isp_digital_gain: raw.isp.digital_gain,
            colour_gains: p.colour_gains,
            colour_temperature: p.colour_temperature,
            lux: p.lux,
            ev: shot.ev,
            target_frame: shot.target,
            landed,
            verified: s.verified,
            isp,
            process_time: t.elapsed(),
        },
    })
}
