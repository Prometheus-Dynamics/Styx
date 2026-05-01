use styx_core::prelude::FourCc;

pub(crate) fn estimated_format_bytes(code: FourCc, width: usize, height: usize) -> Option<usize> {
    code.estimated_frame_bytes(width, height)
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
}
