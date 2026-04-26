use std::env;
use std::time::{Duration, Instant};

use styx::prelude::*;

fn find_device(selector: Option<&str>) -> Result<ProbedDevice, Box<dyn std::error::Error>> {
    let devices = probe_all();
    if devices.is_empty() {
        return Err("no devices found".into());
    }

    if let Some(selector) = selector {
        devices
            .into_iter()
            .find(|dev| {
                dev.identity.display.contains(selector)
                    || dev.identity.keys.iter().any(|key| key.contains(selector))
            })
            .ok_or_else(|| format!("no device matched selector `{selector}`").into())
    } else {
        devices
            .into_iter()
            .find(|dev| {
                dev.backends.iter().any(|backend| {
                    matches!(backend.kind, BackendKind::V4l2)
                        && backend.descriptor.modes.iter().any(is_hw_decode_candidate)
                })
            })
            .ok_or_else(|| "no V4L2 device with H264/H265/HEVC modes found".into())
    }
}

fn is_hw_decode_candidate(mode: &Mode) -> bool {
    matches!(
        &mode.format.code.to_u32().to_le_bytes(),
        b"H264" | b"H265" | b"HEVC"
    )
}

fn decoder_impl_for(code: FourCc) -> Option<&'static str> {
    match &code.to_u32().to_le_bytes() {
        b"H264" => Some("h264_v4l2request_nv12"),
        b"H265" | b"HEVC" => Some("hevc_v4l2request_nv12"),
        _ => None,
    }
}

fn pick_fastest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals.iter().copied().max_by(|a, b| {
        let left = (a.denominator.get() as u64).saturating_mul(b.numerator.get() as u64);
        let right = (b.denominator.get() as u64).saturating_mul(a.numerator.get() as u64);
        left.cmp(&right)
    })
}

fn pick_mode(device: &ProbedDevice) -> Result<Mode, Box<dyn std::error::Error>> {
    let backend = device
        .backends
        .iter()
        .find(|backend| matches!(backend.kind, BackendKind::V4l2))
        .ok_or("device missing V4L2 backend")?;
    backend
        .descriptor
        .modes
        .iter()
        .filter(|mode| is_hw_decode_candidate(mode))
        .max_by_key(|mode| {
            (
                mode.format.resolution.width.get() as u64
                    * mode.format.resolution.height.get() as u64,
                pick_fastest_interval(mode)
                    .map(|iv| iv.denominator.get() / iv.numerator.get().max(1))
                    .unwrap_or(0),
            )
        })
        .cloned()
        .ok_or_else(|| "device has no H264/H265/HEVC V4L2 modes".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(all(
        target_os = "linux",
        feature = "v4l2",
        feature = "codec-ffmpeg"
    )) {
        println!("Enable `v4l2` and `codec-ffmpeg` on Linux to run this example.");
        return Ok(());
    }

    let args = env::args().skip(1).collect::<Vec<_>>();
    let (selector, seconds) = match args.as_slice() {
        [] => (None, 5),
        [only] => match only.parse::<u64>() {
            Ok(seconds) => (None, seconds.max(1)),
            Err(_) => (Some(only.as_str()), 5),
        },
        [selector, seconds, ..] => (
            Some(selector.as_str()),
            seconds.parse::<u64>().unwrap_or(5).max(1),
        ),
    };

    let device = find_device(selector)?;
    let mode = pick_mode(&device)?;
    let decoder_impl =
        decoder_impl_for(mode.format.code).ok_or("selected mode has no v4l2request decoder")?;
    let interval = pick_fastest_interval(&mode);
    let registry = CodecRegistry::with_enabled_codecs_for_max(
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    )?;

    let mut request = CaptureRequest::new(&device)
        .backend(BackendKind::V4l2)
        .mode(mode.id.clone());
    if let Some(interval) = interval {
        request = request.interval(interval);
    }

    let mut pipeline = MediaPipelineBuilder::new(request)
        .decoder_from_registry(
            &registry.handle(),
            mode.format.code,
            Some(decoder_impl),
            true,
        )?
        .encode_enabled(false)
        .start()?;

    println!("device: {}", device.identity.display);
    println!(
        "mode: {} {}x{} decoder={}",
        mode.format.code, mode.format.resolution.width, mode.format.resolution.height, decoder_impl
    );

    let start = Instant::now();
    let mut frames = 0u32;
    let mut dmabuf_frames = 0u32;
    let mut exported_dmabuf = 0u32;
    let mut exported_memfd = 0u32;
    let mut export_errors = 0u32;

    while start.elapsed() < Duration::from_secs(seconds) {
        match pipeline.next_blocking(Duration::from_millis(500)) {
            RecvOutcome::Data(frame) => {
                frames = frames.saturating_add(1);
                if frame.residency() == FrameResidency::Dmabuf {
                    dmabuf_frames = dmabuf_frames.saturating_add(1);
                }
                match frame.export_descriptor_and_backing() {
                    Ok((_descriptor, FrameBackingExport::DmabufPlanes { .. })) => {
                        exported_dmabuf = exported_dmabuf.saturating_add(1);
                    }
                    Ok((_descriptor, FrameBackingExport::Memfd { .. })) => {
                        exported_memfd = exported_memfd.saturating_add(1);
                    }
                    Err(_) => {
                        export_errors = export_errors.saturating_add(1);
                    }
                }
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }

    println!(
        "frames={} dmabuf_frames={} exported_dmabuf={} exported_memfd={} export_errors={}",
        frames, dmabuf_frames, exported_dmabuf, exported_memfd, export_errors
    );
    if frames > 0 && dmabuf_frames == frames && exported_dmabuf == frames {
        println!("result: drm-prime dmabuf decode/export path active");
    } else if frames > 0 {
        println!("result: decoder produced frames, but not all were dmabuf-exportable");
    } else {
        println!("result: no decoded frames observed");
    }

    Ok(())
}
