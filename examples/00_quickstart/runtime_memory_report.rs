use std::sync::Arc;
use std::time::Duration;

use styx::memory::runtime_memory_report;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("process-only report before capture:");
    println!("{}", runtime_memory_report());

    let device = CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("virtual-memory")
            .resolution(640, 360)
            .fps(30),
    )
    .into_device();
    let mode = device.default_mode().ok_or("virtual device missing mode")?;
    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));
    let mut pipeline = device
        .pipeline()
        .config(
            StyxConfig::new()
                .capture_queue_depth(2)
                .capture_pool(2, 1 << 18, 4),
        )
        .decoder(decoder)
        .without_encoder()
        .start()?;

    let mut frames = 0u32;
    while frames < 8 {
        match pipeline.next_blocking_result(Duration::from_millis(50))? {
            RecvOutcome::Data(frame) => {
                frames = frames.saturating_add(1);
                std::hint::black_box(frame.payload_bytes());
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }

    println!("pipeline-attached report after {frames} frames:");
    println!("{}", pipeline.runtime_memory_report());
    pipeline.stop();
    Ok(())
}

