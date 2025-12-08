use std::num::NonZeroU32;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::prelude::*;

fn main() -> Result<(), CaptureError> {
    // Tweak queue depth and pool limits globally before touching capture.
    StyxConfig::new()
        .capture_queue_depth(8)
        .capture_pool(4, 1 << 18, 8)
        .apply();

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();

    let handle = CaptureRequest::new(&device).mode(mode.id).start()?;
    println!(
        "virtual capture on {:?} at {:?} (interval {:?})",
        handle.backend(),
        handle.mode().format,
        handle.interval()
    );

    let mut frames = 0;
    while frames < 12 {
        match handle.recv_blocking(Duration::from_millis(2)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                let first = frame
                    .planes()
                    .first()
                    .and_then(|p| p.data().first())
                    .copied()
                    .unwrap_or_default();
                println!(
                    "#{frames:02} ts={} format={:?} first_byte={}",
                    meta.timestamp, meta.format.code, first
                );
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    let metrics = handle.metrics();
    println!(
        "capture samples={} avg_wait_ms={:.2?} fps={:.1?}",
        metrics.samples(),
        metrics.avg_millis(),
        metrics.fps()
    );

    handle.stop();
    Ok(())
}

fn virtual_device() -> ProbedDevice {
    let res_primary = Resolution::new(640, 360).unwrap();
    let res_secondary = Resolution::new(320, 180).unwrap();
    let interval_30fps = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let interval_15fps = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(15).unwrap(),
    };

    let mode_fast = Mode {
        id: ModeId {
            format: MediaFormat::new(FourCc::new(*b"RG24"), res_primary, ColorSpace::Srgb),
            interval: Some(interval_30fps),
        },
        format: MediaFormat::new(FourCc::new(*b"RG24"), res_primary, ColorSpace::Srgb),
        intervals: vec![interval_30fps].into(),
        interval_stepwise: None,
    };
    let mode_slow = Mode {
        id: ModeId {
            format: MediaFormat::new(FourCc::new(*b"RG24"), res_secondary, ColorSpace::Srgb),
            interval: Some(interval_15fps),
        },
        format: MediaFormat::new(FourCc::new(*b"RG24"), res_secondary, ColorSpace::Srgb),
        intervals: vec![interval_15fps].into(),
        interval_stepwise: None,
    };

    let descriptor = CaptureDescriptor {
        modes: vec![mode_fast.clone(), mode_slow],
        controls: Vec::new(),
    };

    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: descriptor.clone(),
        properties: vec![("kind".into(), "virtual".into())],
    };

    ProbedDevice {
        identity: DeviceIdentity {
            display: "virtual-rg24".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
