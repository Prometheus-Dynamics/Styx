#[cfg(feature = "async")]
use std::sync::Arc;
#[cfg(feature = "async")]
use std::time::Duration;

#[cfg(feature = "async")]
use styx::DeviceIdentity;
#[cfg(feature = "async")]
use styx::prelude::*;

#[cfg(not(feature = "async"))]
fn main() {
    eprintln!("Enable the `async` feature to run this example.");
}

#[cfg(feature = "async")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();

    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));
    let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
        .decoder(decoder)
        .start()?;

    let mut frames = 0;
    while frames < 25 {
        match pipeline.next_async().await {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:02} ts={} fmt={:?} stride={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().get(0).copied().unwrap_or_default()
                );
            }
            RecvOutcome::Empty => tokio::time::sleep(Duration::from_millis(2)).await,
            RecvOutcome::Closed => break,
        }
    }

    let metrics = pipeline.metrics();
    println!(
        "async capture avg_ms={:.2?} decode avg_ms={:.2?} samples={}",
        metrics.capture.avg_millis(),
        metrics.decode.avg_millis(),
        metrics.capture.samples()
    );

    pipeline.stop();
    Ok(())
}

#[cfg(feature = "async")]
fn virtual_device() -> ProbedDevice {
    let res = Resolution::new(320, 180).unwrap();
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format: format.clone(),
            interval: None,
        },
        format,
        intervals: Vec::new().into(),
        interval_stepwise: None,
    };
    let descriptor = CaptureDescriptor {
        modes: vec![mode.clone()],
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
            display: "virtual-async".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
