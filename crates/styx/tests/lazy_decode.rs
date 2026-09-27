//! Decoding runs when the consumer takes a frame, so frames the capture queue drops for a slow
//! consumer are never decoded. With several cameras on a small CPU this is the difference
//! between decoding what is used and decoding everything the cameras send.

#![cfg(feature = "facade")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use styx::prelude::*;
use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};

/// Passes frames through and counts them.
struct CountingDecoder {
    descriptor: CodecDescriptor,
    decoded: Arc<AtomicU64>,
}

impl Codec for CountingDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        Ok(input)
    }
}

#[test]
fn frames_dropped_for_a_slow_consumer_are_never_decoded() {
    let device = CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("fast-camera")
            .resolution(64, 48)
            .fps(200),
    )
    .into_device();
    let mode = device.default_mode().expect("virtual mode");
    let decoded = Arc::new(AtomicU64::new(0));
    let decoder = CountingDecoder {
        descriptor: CodecDescriptor {
            kind: CodecKind::Decoder,
            input: mode.format.code,
            output: mode.format.code,
            name: "count",
            impl_name: "count",
        },
        decoded: decoded.clone(),
    };
    let mut pipeline = device
        .pipeline()
        .config(StyxConfig::new().latest_frame_only())
        .decoder(Arc::new(decoder))
        .without_encoder()
        .start()
        .expect("pipeline");

    let mut delivered = 0u64;
    let start = Instant::now();
    while delivered < 10 && start.elapsed() < Duration::from_secs(10) {
        if let RecvOutcome::Data(frame) = pipeline.next_blocking(Duration::from_millis(200)) {
            delivered += 1;
            std::hint::black_box(frame);
            // A consumer taking 25 ms per frame on a 200 fps camera.
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    let evicted = pipeline
        .health_report()
        .drop_reasons
        .iter()
        .filter(|d| d.reason == FrameDropReason::CaptureQueueEviction)
        .map(|d| d.count)
        .sum::<u64>();
    pipeline.stop();

    assert_eq!(delivered, 10);
    assert!(
        evicted > 10,
        "the camera should outpace the consumer ({evicted} evicted)"
    );
    assert_eq!(
        decoded.load(Ordering::Relaxed),
        delivered,
        "only delivered frames are decoded"
    );
}
