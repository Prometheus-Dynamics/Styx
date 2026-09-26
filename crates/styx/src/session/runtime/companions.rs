//! Keep pyramid companions attached as frames move through pipeline stages.
//!
//! Stages that re-encode pixels without changing geometry (decoders) keep the input's
//! companions. Geometry transforms apply the same transform to each companion and drop any
//! they cannot transform, so a companion never disagrees with its primary. Encoders produce
//! compressed packets, which carry no companions.

use styx_core::prelude::{CompanionKind, FrameLease};

pub(super) type Companions = Vec<(CompanionKind, FrameLease)>;

/// Re-attach `companions` to a stage's output unless the stage attached its own or changed the
/// frame's timestamp.
pub(super) fn carry_companions(frame: FrameLease, companions: Companions) -> FrameLease {
    if companions.is_empty() || frame.companions().next().is_some() {
        return frame;
    }
    let timestamp = frame.meta().timestamp;
    let mut frame = frame;
    for (kind, companion) in companions {
        if companion.meta().timestamp != timestamp || companion.companions().next().is_some() {
            continue;
        }
        frame = frame
            .with_companion(kind, companion)
            .expect("timestamp and nesting checked above");
    }
    frame
}

/// Apply a packed rotate/mirror to each companion, dropping those it cannot handle.
#[cfg(feature = "hooks")]
pub(super) fn transform_companions(
    companions: Companions,
    transform: styx_core::prelude::FrameTransform,
) -> Companions {
    companions
        .into_iter()
        .filter_map(|(kind, companion)| {
            styx_core::prelude::transform_packed_frame(&companion, transform)
                .ok()
                .map(|transformed| (kind, transformed))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use styx_core::prelude::*;

    fn grey(width: u32, height: u32, ts: u64) -> FrameLease {
        let len = (width * height) as usize;
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        let res = Resolution::new(width, height).unwrap();
        FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), ts),
            buf,
            len,
            width as usize,
        )
    }

    #[test]
    fn decoder_output_keeps_input_companions() {
        let mut input = grey(8, 8, 5).with_box_pyramid(1, 1).unwrap();
        let carried = input.take_companions();
        let decoded = carry_companions(grey(8, 8, 5), carried);
        assert!(decoded.pyramid_level(1).is_some());
    }

    #[test]
    fn mismatched_timestamps_are_not_carried() {
        let mut input = grey(8, 8, 5).with_box_pyramid(1, 1).unwrap();
        let carried = input.take_companions();
        let decoded = carry_companions(grey(8, 8, 6), carried);
        assert!(decoded.pyramid_level(1).is_none());
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn rotated_companions_follow_the_primary() {
        let mut input = grey(8, 4, 1).with_box_pyramid(1, 1).unwrap();
        let transform = FrameTransform {
            rotation: Rotation90::Deg90,
            mirror: false,
        };
        let companions = transform_companions(input.take_companions(), transform);
        let rotated = transform_packed_frame(&input, transform).unwrap();
        let rotated = carry_companions(rotated, companions);
        let half = rotated.pyramid_level(1).expect("companion kept");
        assert_eq!(half.meta().format.resolution.width.get(), 2);
        assert_eq!(half.meta().format.resolution.height.get(), 4);
    }
}
