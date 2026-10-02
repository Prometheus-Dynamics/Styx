//! Candidate generation: every (backend, mode, route) that can satisfy the requirements.

use std::sync::Arc;

use styx_codec::{Codec, CodecDescriptor, CodecKind, CodecRegistryHandle};
use styx_core::prelude::*;

use super::cost::{self, StepCost, megapixels};
use super::native::{self, native_isp, raw_bayer};
use super::{PlanRejection, PlanStep, StepExecution, StepKind};
#[cfg(feature = "libcamera")]
use crate::BackendHandle;
use crate::BackendKind;
use crate::prelude::{Mode, ProbedBackend, ProbedDevice};

/// How frames get from the capture format to what the consumer asked for.
#[derive(Clone)]
pub(crate) enum Route {
    /// The capture format is already acceptable.
    Direct,
    /// Planar/semi-planar YUV reduced to its Y plane without copying.
    LumaView,
    /// A codec converts the capture format.
    Decode {
        decoder: Arc<dyn Codec>,
        hardware: bool,
    },
    /// An encoder compresses frames (after a decoder, for compressed capture formats).
    Encode {
        decoder: Option<Arc<dyn Codec>>,
        encoder: Arc<dyn Codec>,
        hardware: bool,
    },
}

impl Route {
    /// Routes that prepare frames the same way compare equal.
    pub(crate) fn same_as(&self, other: &Route) -> bool {
        match (self, other) {
            (Route::Direct, Route::Direct) | (Route::LumaView, Route::LumaView) => true,
            (
                Route::Decode {
                    decoder: a,
                    hardware: ha,
                },
                Route::Decode {
                    decoder: b,
                    hardware: hb,
                },
            ) => ha == hb && same_codec(a, b),
            (
                Route::Encode {
                    decoder: da,
                    encoder: ea,
                    hardware: ha,
                },
                Route::Encode {
                    decoder: db,
                    encoder: eb,
                    hardware: hb,
                },
            ) => {
                ha == hb
                    && same_codec(ea, eb)
                    && match (da, db) {
                        (Some(a), Some(b)) => same_codec(a, b),
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

fn same_codec(a: &Arc<dyn Codec>, b: &Arc<dyn Codec>) -> bool {
    let (a, b) = (a.descriptor(), b.descriptor());
    a.impl_name == b.impl_name && a.input == b.input && a.output == b.output
}

pub(crate) struct Candidate<'a> {
    pub backend: &'a ProbedBackend,
    pub mode: Mode,
    pub fps: Option<f32>,
    pub route: Route,
    pub steps: Vec<PlanStep>,
    pub total: StepCost,
    pub isp_pyramid_level: Option<u8>,
    /// The decoder scales to 1/`decode_scale` of the capture size (1 = full size).
    pub decode_scale: u8,
    /// The ISP delivers frames at this size instead of the mode's (libcamera on Raspberry Pi).
    pub isp_output: Option<(u32, u32)>,
    pub notes: Vec<String>,
}

pub(crate) fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::V4l2 => "v4l2",
        BackendKind::Libcamera => "libcamera",
        BackendKind::Virtual => "virtual",
        BackendKind::Netcam => "netcam",
        BackendKind::File => "file",
        BackendKind::Simulation => "simulation",
        BackendKind::Replay => "replay",
        BackendKind::Native => "native",
    }
}

/// Raspberry Pi CSI sensors behind libcamera's PiSP/VC4 pipelines (two processed outputs).
pub(crate) fn has_isp_second_output(backend: &ProbedBackend) -> bool {
    match &backend.handle {
        #[cfg(feature = "libcamera")]
        BackendHandle::Libcamera { id } => id.starts_with("/base/") && id.contains("/i2c@"),
        _ => false,
    }
}

fn mode_fps(mode: &Mode) -> Option<f32> {
    let from_list = mode
        .intervals
        .iter()
        .map(|i| i.fps())
        .fold(None, |best: Option<f32>, fps| {
            Some(best.map_or(fps, |b| b.max(fps)))
        });
    from_list.or_else(|| mode.interval_stepwise.map(|s| s.min.fps()))
}

pub(crate) fn describe(backend: &ProbedBackend, mode: &Mode) -> String {
    format!(
        "{} {} {}x{}",
        backend_name(backend.kind),
        mode.format.code,
        mode.format.resolution.width,
        mode.format.resolution.height
    )
}

pub(crate) fn candidates<'a>(
    device: &'a ProbedDevice,
    req: &FrameRequirements,
    registry: &CodecRegistryHandle,
    rejected: &mut Vec<PlanRejection>,
) -> Vec<Candidate<'a>> {
    let mut out = Vec::new();
    for backend in &device.backends {
        if let Some(wanted) = &req.overrides.backend
            && !wanted.eq_ignore_ascii_case(backend_name(backend.kind))
        {
            continue;
        }
        for mode in &backend.descriptor.modes {
            let label = describe(backend, mode);
            match candidate(backend, mode, req, registry) {
                Ok(candidate) => out.push(candidate),
                Err(reason) => rejected.push(PlanRejection {
                    candidate: label,
                    reason,
                }),
            }
        }
    }
    out
}

pub(crate) fn candidate<'a>(
    backend: &'a ProbedBackend,
    mode: &Mode,
    req: &FrameRequirements,
    registry: &CodecRegistryHandle,
) -> Result<Candidate<'a>, String> {
    let code = mode.format.code;
    let (width, height) = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    if let Some((min_w, min_h)) = req.min_resolution
        && (width < min_w || height < min_h)
    {
        return Err(format!("below minimum resolution {min_w}x{min_h}"));
    }
    if let Some((max_w, max_h)) = req.max_resolution
        && (width > max_w || height > max_h)
    {
        return Err(format!("above maximum resolution {max_w}x{max_h}"));
    }
    let fps = mode_fps(mode);
    if let (Some(min), Some(fps)) = (req.min_fps, fps)
        && fps + 0.5 < min as f32
    {
        return Err(format!("{fps:.0} fps is below {min} fps"));
    }

    let mp = megapixels(width, height);
    // A native camera with an ISP processes raw frames itself, with the 3A loop: a raw mode
    // through a decoder would give frames nobody exposes or white-balances.
    if backend.kind == BackendKind::Native
        && raw_bayer(code)
        && !req.accepts(code)
        && native_isp(backend).is_some()
    {
        return Err(format!(
            "raw {code} would need a decoder without 3A; the camera's processed modes run AE/AWB"
        ));
    }

    let mut steps = vec![capture_step(backend, mode, fps)];
    let notes = Vec::new();

    let wants_luma = matches!(req.output, OutputFormat::Luma);
    let route = if req.accepts(code) {
        Route::Direct
    } else if wants_luma && code.layout_info().planes.subsampling.is_some() {
        steps.push(PlanStep {
            kind: StepKind::LumaView,
            execution: StepExecution::ZeroCopy,
            detail: format!("Y plane of {code}"),
            cost: StepCost::ZERO,
        });
        Route::LumaView
    } else {
        let scales_down = req
            .output_resolution
            .is_some_and(|(w, h)| w < width || h < height);
        let picked = pick_decoder(code, req, registry, scales_down);
        let (decoder, target) = match picked {
            Ok(picked) => picked,
            Err(reason) => {
                return finish(
                    backend,
                    mode,
                    req,
                    fps,
                    encode_route(code, req, registry, mp, &mut steps)?.ok_or(reason)?,
                    steps,
                    notes,
                );
            }
        };
        let descriptor = decoder.descriptor();
        let hardware = descriptor.is_hardware_accelerated();
        let threads = cost::decode_threads(req.priority, req.overrides.decode_threads);
        steps.push(PlanStep {
            kind: StepKind::Decode,
            execution: if hardware {
                StepExecution::Hardware
            } else {
                StepExecution::Cpu
            },
            detail: format!(
                "{code} -> {target} via {}{}",
                descriptor.impl_name,
                if !hardware && descriptor.impl_name == "turbojpeg-luma" && threads != 1 {
                    " (multi-core when frames carry restart markers)"
                } else {
                    ""
                }
            ),
            cost: decode_cost(code, descriptor, mp, threads),
        });
        Route::Decode { decoder, hardware }
    };
    finish(backend, mode, req, fps, route, steps, notes)
}

/// The rest of a candidate once its route is chosen: hardware policy, scaling, pyramid, ROI and
/// alignment steps, and the total cost.
fn finish<'a>(
    backend: &'a ProbedBackend,
    mode: &Mode,
    req: &FrameRequirements,
    fps: Option<f32>,
    route: Route,
    mut steps: Vec<PlanStep>,
    mut notes: Vec<String>,
) -> Result<Candidate<'a>, String> {
    let (width, height) = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    let mp = megapixels(width, height);
    let isp = backend.kind == BackendKind::Libcamera
        || (!raw_bayer(mode.format.code) && native_isp(backend) == Some("pisp"));
    if matches!(req.overrides.hardware, HardwarePolicy::Required)
        && !isp
        && !matches!(
            route,
            Route::Decode { hardware: true, .. } | Route::Encode { hardware: true, .. }
        )
    {
        return Err("hardware required but this path runs on the CPU".into());
    }

    let decode_scale = decode_scale(&route, req, (width, height));
    if decode_scale > 1
        && let Some(step) = steps.last_mut()
    {
        let (w, h) = (
            width.div_ceil(decode_scale.into()),
            height.div_ceil(decode_scale.into()),
        );
        step.detail = format!("{}, at 1/{decode_scale} size ({w}x{h})", step.detail);
        let factor = cost::scaled_decode_factor(decode_scale);
        step.cost = StepCost::offloaded(step.cost.latency_ms * factor, step.cost.cpu_ms * factor);
    }
    let isp_output = isp_output(backend, &route, req, mode.format.code, (width, height));
    if let Some((w, h)) = isp_output {
        steps.push(PlanStep {
            kind: StepKind::Scale,
            execution: StepExecution::Hardware,
            detail: format!("{w}x{h} from the ISP (the mode's field of view)"),
            cost: StepCost::ZERO,
        });
    } else if decode_scale == 1
        && let Some((tw, th)) = req.output_resolution
        && (tw < width || th < height)
    {
        notes.push(format!(
            "frames are {width}x{height}: this route cannot scale to {tw}x{th}"
        ));
    }
    let (width, height) = isp_output.unwrap_or((
        width.div_ceil(decode_scale.into()),
        height.div_ceil(decode_scale.into()),
    ));

    let isp_pyramid_level = add_pyramid_steps(backend, &route, req, width, height, &mut steps)?;
    let encoded = matches!(route, Route::Encode { .. });
    if encoded && req.pyramid.is_some_and(|p| p.levels > 0) {
        return Err("pyramid levels need uncompressed frames".into());
    }
    if req.roi.is_some() && !encoded {
        steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::ZeroCopy,
            detail: match &route {
                Route::Decode { decoder, .. }
                    if decoder.descriptor().impl_name == "turbojpeg-luma" =>
                {
                    "region of interest; the JPEG decoder skips rows below it".into()
                }
                _ => "region of interest view".into(),
            },
            cost: StepCost::ZERO,
        });
    }
    if let Some(align) = req.stride_alignment.filter(|_| !encoded) {
        match &route {
            Route::Decode { decoder, .. } if decoder.descriptor().impl_name == "turbojpeg-luma" => {
            }
            _ => {
                notes.push(format!(
                    "rows are copied to a {align}-byte stride only if the capture stride is not \
                     already aligned (~{:.2} ms)",
                    cost::REALIGN_MS_PER_MP * mp
                ));
            }
        }
    }

    let total = steps
        .iter()
        .fold(StepCost::ZERO, |sum, step| sum + step.cost);
    Ok(Candidate {
        backend,
        mode: mode.clone(),
        fps,
        route,
        steps,
        total,
        isp_pyramid_level,
        decode_scale,
        isp_output,
        notes,
    })
}

/// Size the ISP should deliver for `req.output_resolution`: the smallest even size with the
/// mode's aspect ratio (so its field of view) that covers it. Only for cameras behind a
/// scaling ISP, on routes that pass frames through without decoding.
fn isp_output(
    backend: &ProbedBackend,
    route: &Route,
    req: &FrameRequirements,
    code: FourCc,
    mode: (u32, u32),
) -> Option<(u32, u32)> {
    let (tw, th) = req.output_resolution?;
    // Routes that pass frames through, or decode uncompressed ones (any size the ISP makes).
    let scalable = match route {
        Route::Direct | Route::LumaView => true,
        Route::Decode { .. } => !code.is_compressed(),
        // The encoder takes any size the ISP makes.
        Route::Encode { decoder, .. } => decoder.is_none() && !code.is_compressed(),
    };
    if !has_isp_second_output(backend)
        || !scalable
        || matches!(req.overrides.hardware, HardwarePolicy::Disabled)
        || (tw >= mode.0 && th >= mode.1)
    {
        return None;
    }
    let scale = (f64::from(tw) / f64::from(mode.0)).max(f64::from(th) / f64::from(mode.1));
    let even = |v: f64| ((v.ceil() as u32).next_multiple_of(2)).max(2);
    let (w, h) = (
        even(f64::from(mode.0) * scale),
        even(f64::from(mode.1) * scale),
    );
    (w < mode.0 || h < mode.1).then_some((w.min(mode.0), h.min(mode.1)))
}

/// 1/N size the decoder produces for `req.output_resolution`: turbojpeg scales MJPEG in the DCT
/// domain by 2, 4 or 8. 1 when nothing is to be gained or the decoder cannot scale.
fn decode_scale(route: &Route, req: &FrameRequirements, source: (u32, u32)) -> u8 {
    let (Some(target), Route::Decode { decoder, .. }) = (req.output_resolution, route) else {
        return 1;
    };
    if !matches!(
        decoder.descriptor().impl_name,
        "turbojpeg" | "turbojpeg-luma"
    ) {
        return 1;
    }
    [8u8, 4, 2]
        .into_iter()
        .find(|&d| {
            source.0.div_ceil(d.into()) >= target.0 && source.1.div_ceil(d.into()) >= target.1
        })
        .unwrap_or(1)
}

fn capture_step(backend: &ProbedBackend, mode: &Mode, fps: Option<f32>) -> PlanStep {
    if let Some(step) = native::processed_capture_step(backend, mode, fps) {
        return step;
    }
    let (execution, latency, how) = match backend.kind {
        BackendKind::Libcamera if has_isp_second_output(backend) => (
            StepExecution::Hardware,
            cost::ISP_CAPTURE_LATENCY_MS,
            "ISP, cached dma-heap buffers",
        ),
        BackendKind::Libcamera | BackendKind::V4l2 => (
            StepExecution::ZeroCopy,
            cost::uvc_capture_latency_ms(fps),
            "camera exposure, encode and transfer",
        ),
        BackendKind::Native => (
            StepExecution::ZeroCopy,
            cost::native_capture_latency_ms(fps),
            "sensor driven by Styx, raw frames in dma-bufs",
        ),
        BackendKind::Replay => (
            StepExecution::ZeroCopy,
            0.0,
            "recording; pyramid levels it contains are reused, not recomputed",
        ),
        _ => (StepExecution::ZeroCopy, 0.0, "synthetic source"),
    };
    PlanStep {
        kind: StepKind::Capture,
        execution,
        detail: format!("{} ({how})", describe(backend, mode)),
        cost: StepCost::offloaded(latency, 0.0),
    }
}

/// The decoder from `code` to the first output the registry can produce. With `scales_down`,
/// decoders that can scale while decoding are preferred.
fn pick_decoder(
    code: FourCc,
    req: &FrameRequirements,
    registry: &CodecRegistryHandle,
    scales_down: bool,
) -> Result<(Arc<dyn Codec>, FourCc), String> {
    let targets: Vec<FourCc> = match &req.output {
        OutputFormat::Luma => vec![FourCc::GREY],
        OutputFormat::Formats(formats) => formats.clone(),
    };
    let accept = |d: &CodecDescriptor| codec_allowed(d, req) && d.kind == CodecKind::Decoder;
    let scaling =
        |d: &CodecDescriptor| accept(d) && matches!(d.impl_name, "turbojpeg" | "turbojpeg-luma");
    for target in &targets {
        if scales_down && let Ok(decoder) = registry.lookup_for_output_where(code, *target, scaling)
        {
            return Ok((decoder, *target));
        }
        if let Ok(decoder) = registry.lookup_for_output_where(code, *target, accept) {
            return Ok((decoder, *target));
        }
    }
    Err(format!(
        "no enabled decoder from {code} to {} allowed by the overrides",
        targets
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/")
    ))
}

/// Whether the overrides allow codec `d` (forbid list, hardware policy and, for decoders, a
/// named decoder).
fn codec_allowed(d: &CodecDescriptor, req: &FrameRequirements) -> bool {
    let name = d.impl_name;
    if req
        .overrides
        .forbid
        .iter()
        .any(|f| f.eq_ignore_ascii_case(name))
    {
        return false;
    }
    if d.kind == CodecKind::Decoder
        && let Some(only) = &req.overrides.decoder
        && !only.eq_ignore_ascii_case(name)
    {
        return false;
    }
    let hardware = d.is_hardware_accelerated();
    match req.overrides.hardware {
        HardwarePolicy::Auto => true,
        HardwarePolicy::Disabled => !hardware,
        HardwarePolicy::Required => hardware,
    }
}

/// For a consumer that wants compressed frames the camera does not produce: an encoder from the
/// capture format, or a decoder to a format an encoder takes and that encoder. Hardware encoders
/// first. `None` when the consumer takes no compressed format.
fn encode_route(
    code: FourCc,
    req: &FrameRequirements,
    registry: &CodecRegistryHandle,
    mp: f32,
    steps: &mut Vec<PlanStep>,
) -> Result<Option<Route>, String> {
    let OutputFormat::Formats(formats) = &req.output else {
        return Ok(None);
    };
    let targets: Vec<FourCc> = formats
        .iter()
        .copied()
        .filter(|f| f.is_compressed())
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    let decoder_ok = |d: &CodecDescriptor| d.kind == CodecKind::Decoder && codec_allowed(d, req);
    for &target in &targets {
        for hardware in [true, false] {
            let encoder_ok = |d: &CodecDescriptor| {
                d.kind == CodecKind::Encoder
                    && d.is_hardware_accelerated() == hardware
                    && codec_allowed(d, req)
            };
            let found = registry
                .lookup_for_output_where(code, target, encoder_ok)
                .ok()
                .map(|encoder| (None, encoder))
                .or_else(|| {
                    [FourCc::NV12, FourCc::YUYV, FourCc::RG24]
                        .into_iter()
                        .find_map(|mid| {
                            let encoder = registry
                                .lookup_for_output_where(mid, target, encoder_ok)
                                .ok()?;
                            let decoder = registry
                                .lookup_for_output_where(code, mid, decoder_ok)
                                .ok()?;
                            Some((Some(decoder), encoder))
                        })
                });
            let Some((decoder, encoder)) = found else {
                continue;
            };
            if let Some(decoder) = &decoder {
                let d = decoder.descriptor();
                steps.push(PlanStep {
                    kind: StepKind::Decode,
                    execution: if d.is_hardware_accelerated() {
                        StepExecution::Hardware
                    } else {
                        StepExecution::Cpu
                    },
                    detail: format!("{code} -> {} via {}", d.output, d.impl_name),
                    cost: decode_cost(code, d, mp, 1),
                });
            }
            let e = encoder.descriptor();
            steps.push(PlanStep {
                kind: StepKind::Encode,
                execution: if hardware {
                    StepExecution::Hardware
                } else {
                    StepExecution::Cpu
                },
                detail: format!("{} -> {target} via {} ({})", e.input, e.impl_name, e.name),
                cost: encode_cost(e, mp),
            });
            return Ok(Some(Route::Encode {
                decoder,
                encoder,
                hardware,
            }));
        }
    }
    Err(format!(
        "no enabled encoder to {} from {code} allowed by the overrides",
        targets
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/")
    ))
}

fn encode_cost(descriptor: &CodecDescriptor, mp: f32) -> StepCost {
    if descriptor.is_hardware_accelerated() {
        return StepCost::offloaded(
            cost::HW_ENCODE_LATENCY_MS_PER_MP * mp,
            cost::HW_ENCODE_CPU_MS_PER_MP * mp,
        );
    }
    let per_mp = if descriptor.output.is_jpeg_encoded() {
        cost::SW_JPEG_ENCODE_MS_PER_MP
    } else {
        cost::SW_H26X_ENCODE_MS_PER_MP
    };
    StepCost::cpu(per_mp * mp)
}

fn decode_cost(code: FourCc, descriptor: &CodecDescriptor, mp: f32, threads: usize) -> StepCost {
    if descriptor.is_hardware_accelerated() {
        return StepCost::offloaded(
            cost::HW_DECODE_LATENCY_MS_PER_MP * mp,
            cost::HW_DECODE_CPU_MS_PER_MP * mp,
        );
    }
    let per_mp = match (descriptor.impl_name, code.is_jpeg_encoded()) {
        ("turbojpeg-luma", _) if threads == 1 => cost::MJPEG_LUMA_MS_PER_MP,
        // Restart markers are common but not guaranteed; estimate between single and parallel.
        ("turbojpeg-luma", _) => {
            (cost::MJPEG_LUMA_MS_PER_MP + cost::MJPEG_LUMA_PARALLEL_MS_PER_MP) / 2.0
        }
        (_, true) if descriptor.impl_name.contains("ffmpeg") => cost::FFMPEG_SW_MJPEG_MS_PER_MP,
        (_, true) => cost::MJPEG_LUMA_MS_PER_MP * 2.0,
        _ if code.is_compressed() => cost::SW_VIDEO_DECODE_MS_PER_MP,
        _ if raw_bayer(code) => cost::SOFTISP_MS_PER_MP,
        _ if matches!(
            code,
            FourCc::YUYV | FourCc::UYVY | FourCc::YVYU | FourCc::VYUY
        ) =>
        {
            cost::YUYV_LUMA_MS_PER_MP
        }
        _ => cost::RGB_LUMA_MS_PER_MP,
    };
    StepCost::cpu(per_mp * mp)
}

/// Pyramid steps; returns the level the ISP produces, if any.
fn add_pyramid_steps(
    backend: &ProbedBackend,
    route: &Route,
    req: &FrameRequirements,
    width: u32,
    height: u32,
    steps: &mut Vec<PlanStep>,
) -> Result<Option<u8>, String> {
    let Some(pyramid) = req.pyramid.filter(|p| p.levels > 0) else {
        return Ok(None);
    };
    let isp_possible = has_isp_second_output(backend)
        && matches!(route, Route::Direct | Route::LumaView)
        && !matches!(req.overrides.hardware, HardwarePolicy::Disabled);
    let isp_level = match pyramid.source {
        PyramidSource::Software => None,
        PyramidSource::PreferHardware => isp_possible.then_some(1),
        PyramidSource::HardwareOnly => {
            if !isp_possible {
                return Err("hardware pyramid required but no ISP second output".into());
            }
            if pyramid.levels > 1 {
                return Err(format!(
                    "hardware pyramid required for {} levels; the ISP provides one",
                    pyramid.levels
                ));
            }
            Some(1)
        }
    };
    for level in 1..=pyramid.levels {
        let (w, h) = (width >> level, height >> level);
        if isp_level == Some(level) {
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Hardware,
                detail: format!("{w}x{h} from the ISP's second output"),
                cost: StepCost::ZERO,
            });
        } else {
            let source_mp = megapixels(width >> (level - 1), height >> (level - 1));
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Cpu,
                detail: format!("{w}x{h} 2x2 box filter"),
                cost: StepCost::cpu(cost::BOX_LEVEL_MS_PER_MP * source_mp),
            });
        }
    }
    Ok(isp_level)
}
