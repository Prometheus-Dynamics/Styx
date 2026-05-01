use styx_core::prelude::FourCc;

pub(crate) const MIN_COMPRESSED_PACKET_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COMPRESSED_PACKET_POOL_BYTES: usize = 512 * 1024;
pub(crate) const SHARED_CODEC_POOL_MIN: usize = 2;
pub(crate) const SHARED_CODEC_POOL_SPARE: usize = 4;

pub(crate) fn estimated_format_bytes(code: FourCc, width: usize, height: usize) -> Option<usize> {
    code.estimated_frame_bytes(width, height)
}

pub(crate) fn estimated_compressed_packet_pool_bytes(
    input: FourCc,
    output: FourCc,
    width: usize,
    height: usize,
    fallback_payload_bytes: usize,
) -> Option<usize> {
    if !output.is_compressed() {
        return None;
    }
    let raw_estimate = input
        .estimated_frame_bytes(width, height)
        .or_else(|| fallback_payload_bytes.checked_mul(4));
    let estimate = raw_estimate
        .and_then(|bytes| bytes.checked_div(4))
        .unwrap_or(fallback_payload_bytes)
        .max(MIN_COMPRESSED_PACKET_BYTES)
        .min(MAX_COMPRESSED_PACKET_POOL_BYTES);
    Some(estimate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_common_format_sizes() {
        assert_eq!(
            estimated_format_bytes(FourCc::RG24, 640, 480),
            Some(921_600)
        );
        assert_eq!(estimated_format_bytes(FourCc::RGBA, 2, 2), Some(16));
        assert_eq!(estimated_format_bytes(FourCc::NV12, 4, 2), Some(12));
        assert_eq!(estimated_format_bytes(FourCc::MJPG, 4, 2), None);
    }

    #[test]
    fn estimates_4k_raw_frame_sizes() {
        let width = 3840;
        let height = 2160;

        assert_eq!(
            estimated_format_bytes(FourCc::RG24, width, height),
            Some(24_883_200)
        );
        assert_eq!(
            estimated_format_bytes(FourCc::NV12, width, height),
            Some(12_441_600)
        );
        assert_eq!(
            estimated_format_bytes(FourCc::RGBA, width, height),
            Some(33_177_600)
        );
    }

    #[test]
    fn estimates_compressed_packet_pool_sizes_without_raw_sized_retention() {
        assert_eq!(
            estimated_compressed_packet_pool_bytes(FourCc::NV12, FourCc::MJPG, 1280, 800, 1024),
            Some(384_000)
        );
        assert_eq!(
            estimated_compressed_packet_pool_bytes(FourCc::RG24, FourCc::MJPG, 3840, 2160, 1024),
            Some(MAX_COMPRESSED_PACKET_POOL_BYTES)
        );
        assert_eq!(
            estimated_compressed_packet_pool_bytes(FourCc::RG24, FourCc::RG24, 1280, 800, 1024),
            None
        );
    }
}
