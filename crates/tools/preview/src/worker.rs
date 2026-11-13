//! The preview thread: takes the newest waiting frame, encodes and publishes it, and runs the
//! handshakes for readers that attach. It is the only thread that touches the output ring.

use std::sync::Arc;

use styx_jpeg::JpegEncoder;
use styx_socket::{Limits, Producer};

use crate::encode::{Failure, encode_publish};
use crate::{Shared, lock};

/// Runs until the preview is dropped.
pub(crate) fn run<M: Send + Sync + 'static, K: Send + 'static>(
    shared: &Arc<Shared<M, K>>,
    mut producer: Producer,
    mut encoder: JpegEncoder,
    capacity: usize,
) {
    loop {
        let (frame, sockets) = {
            let mut state = lock(&shared.state);
            loop {
                if state.closing {
                    return;
                }
                if state.latest.is_some() || !state.attach.is_empty() {
                    break;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            (state.latest.take(), std::mem::take(&mut state.attach))
        };

        let mut handshake_failures = 0_u64;
        for socket in sockets {
            if producer.accept(socket, Limits::default()).is_err() {
                handshake_failures += 1;
            }
        }

        // The frame and what keeps it alive are released together, once it is published or dropped.
        let outcome = frame.map(|(frame, _keep)| {
            let started = std::time::Instant::now();
            let result = encode_publish(&mut producer, &mut encoder, &frame, capacity);
            (result, started.elapsed())
        });

        let mut state = lock(&shared.state);
        state.stats.failed += handshake_failures;
        if let Some((result, elapsed)) = outcome {
            match result {
                Ok(bytes) => {
                    state.stats.published += 1;
                    state.stats.jpeg_bytes += bytes as u64;
                    state.stats.encode_ns += elapsed.as_nanos() as u64;
                }
                Err(Failure::NoBuffer) => state.stats.no_buffer += 1,
                Err(Failure::Refused | Failure::Encode | Failure::Publish) => {
                    state.stats.failed += 1;
                }
            }
        }
        state.stats.readers = producer.stats().readers;
    }
}
