#[cfg(feature = "async")]
use std::sync::Arc;
#[cfg(feature = "async")]
use std::time::Duration;

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
    let mode = device.default_mode().ok_or("virtual device missing mode")?;

    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));
    let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
        .decoder(decoder)
        .start()?;

    let mut frames = 0;
    while frames < 25 {
        // This example uses a passthrough decoder, so processing on the current async task is cheap.
        // Use `spawn_tokio_worker` for CPU-heavy decode, encode, graph, hook, or sink pipelines.
        match pipeline.next_async_receive_result().await? {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:02} ts={} fmt={:?} stride={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().first().copied().unwrap_or_default()
                );
            }
            RecvOutcome::Empty => tokio::time::sleep(Duration::from_millis(2)).await,
            RecvOutcome::Closed => break,
        }
    }

    let metrics = pipeline.metrics();
    let end_to_end = metrics.end_to_end.snapshot();
    let source_to_sink = metrics.source_to_sink.snapshot();
    let copies = metrics.copies.snapshot();
    println!(
        "async capture avg_ms={:.2?} decode avg_ms={:.2?} e2e_p50_ms={:.2?} e2e_p95_ms={:.2?} source_p50_ms={:.2?} source_p95_ms={:.2?} copies={} bytes_moved={} samples={}",
        metrics.capture.avg_millis(),
        metrics.decode.avg_millis(),
        end_to_end.p50_millis,
        end_to_end.p95_millis,
        source_to_sink.p50_millis,
        source_to_sink.p95_millis,
        copies.copies,
        copies.bytes_moved,
        metrics.capture.samples()
    );

    pipeline.stop();
    Ok(())
}

#[cfg(feature = "async")]
fn virtual_device() -> ProbedDevice {
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("virtual-async")
            .resolution(320, 180)
            .fps(30),
    )
    .into_device()
}
