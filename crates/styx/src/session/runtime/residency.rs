use styx_codec::prelude::*;

#[cfg(target_os = "linux")]
use crate::frame_sizing::estimated_format_bytes;

pub(super) fn default_copy_required_for_transition(
    _from: FrameResidency,
    _to: FrameResidency,
    reason: ResidencyTransitionReason,
) -> bool {
    matches!(
        reason,
        ResidencyTransitionReason::PackedTransform
            | ResidencyTransitionReason::ImageMaterialize
            | ResidencyTransitionReason::ImageHook
            | ResidencyTransitionReason::BackendFallbackCopy
    )
}

pub(super) fn annotate_residency_transition(
    metrics: &crate::metrics::ResidencyMetrics,
    frame: &mut FrameLease,
    from: FrameResidency,
    reason: ResidencyTransitionReason,
) {
    let copied = default_copy_required_for_transition(from, frame.residency(), reason);
    annotate_residency_transition_with_copy(metrics, frame, from, reason, copied);
}

pub(super) fn annotate_residency_transition_with_copy(
    metrics: &crate::metrics::ResidencyMetrics,
    frame: &mut FrameLease,
    from: FrameResidency,
    reason: ResidencyTransitionReason,
    copied: bool,
) {
    let to = frame.residency();
    let transition = ResidencyTransition {
        from,
        to,
        reason,
        copied,
    };
    frame.meta_mut().residency = Some(to);
    frame.meta_mut().last_transition = Some(transition);
    metrics.record(transition);
}

pub(super) fn stage_accepts_residency(
    accepted: &[FrameResidency],
    residency: FrameResidency,
) -> bool {
    accepted.contains(&residency)
}

#[cfg(target_os = "linux")]
pub(super) fn estimate_shared_output_bytes(
    descriptor: &CodecDescriptor,
    frame: &FrameLease,
) -> Option<usize> {
    let res = frame.meta().format.resolution;
    estimated_format_bytes(
        descriptor.output,
        res.width.get() as usize,
        res.height.get() as usize,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn require_exportable_codec_output(
    stage: &str,
    codec: &dyn Codec,
    frame: FrameLease,
    allow_owned_fallback: bool,
) -> Result<FrameLease, CodecError> {
    if allow_owned_fallback {
        return Ok(frame);
    }
    match frame.export_backing() {
        Ok(_) => Ok(frame),
        Err(err) => Err(CodecError::Codec(format!(
            "{stage} {}:{} produced non-exportable output: {err}",
            codec.descriptor().name,
            codec.descriptor().impl_name
        ))),
    }
}
