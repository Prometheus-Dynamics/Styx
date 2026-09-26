//! Per-frame stage timing: stages replace frames (and often their metadata), so the capture
//! context and earlier timings are carried forward and the stage's own duration is recorded.

use std::time::Duration;

use styx_core::prelude::{FrameLease, FrameMeta};

#[derive(Clone, Copy)]
pub(super) enum TimedStage {
    Decode,
    #[cfg(feature = "hooks")]
    Transform,
    #[cfg(feature = "hooks")]
    Hook,
    Encode,
}

pub(super) fn stamp_stage(
    frame: &mut FrameLease,
    input: &FrameMeta,
    stage: TimedStage,
    elapsed: Duration,
) {
    let meta = frame.meta_mut();
    meta.inherit_capture_context(input);
    let slot = match stage {
        TimedStage::Decode => &mut meta.timing.decode,
        #[cfg(feature = "hooks")]
        TimedStage::Transform => &mut meta.timing.transform,
        #[cfg(feature = "hooks")]
        TimedStage::Hook => &mut meta.timing.hook,
        TimedStage::Encode => &mut meta.timing.encode,
    };
    // Hooks can run more than once per frame (frame hook + FrameLease hook); accumulate.
    *slot = Some(slot.unwrap_or_default() + elapsed);
}

#[cfg(all(test, feature = "hooks"))]
mod tests {
    use super::*;
    use std::time::Instant;
    use styx_core::prelude::*;

    #[test]
    fn stage_output_inherits_capture_context_and_accumulates_hooks() {
        let res = Resolution::new(2, 2).unwrap();
        let mut input = FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 1)
            .with_capture_instant(Instant::now());
        input.timing.sensor_to_capture = Some(Duration::from_millis(8));
        let mut buf = BufferPool::with_limits(1, 4, 1).lease();
        buf.resize(4);
        let mut out = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb), 1),
            buf,
            4,
            2,
        );
        stamp_stage(
            &mut out,
            &input,
            TimedStage::Decode,
            Duration::from_millis(2),
        );
        let before_hook = out.meta().clone();
        stamp_stage(
            &mut out,
            &before_hook,
            TimedStage::Hook,
            Duration::from_millis(1),
        );
        stamp_stage(
            &mut out,
            &before_hook,
            TimedStage::Hook,
            Duration::from_millis(1),
        );
        let timing = out.meta().timing;
        assert_eq!(timing.sensor_to_capture, Some(Duration::from_millis(8)));
        assert_eq!(timing.decode, Some(Duration::from_millis(2)));
        assert_eq!(timing.hook, Some(Duration::from_millis(2)));
        assert!(out.meta().capture_instant.is_some());
    }
}
