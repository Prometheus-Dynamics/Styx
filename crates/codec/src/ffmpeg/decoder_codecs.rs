use super::*;

/// MJPEG decoder via FFmpeg.
pub struct FfmpegMjpegDecoder(pub FfmpegVideoDecoder);

impl FfmpegMjpegDecoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        Self::new_rgb24_for_input(FourCc::MJPG)
    }

    pub fn new_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::RG24,
            false,
            None,
            None,
            true,
            true,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::MJPG,
            FourCc::NV12,
            true,
            None,
            None,
            true,
            true,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::MJPG, zero_copy, threads, pool_limits)
    }

    pub fn with_options_for_input(
        input: FourCc,
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::RG24,
            zero_copy,
            threads,
            pool_limits,
            true,
            true,
        )
        .map(Self)
    }
}

impl Codec for FfmpegMjpegDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.0.process_shared(input, pool)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegMjpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}

pub struct FfmpegH264Decoder(pub FfmpegVideoDecoder);

impl FfmpegH264Decoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::H264,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::H264,
            FourCc::NV12,
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::H264,
            FourCc::RG24,
            zero_copy,
            threads,
            pool_limits,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "h264_v4l2request",
            "h264",
            "h264_v4l2request_nv12",
            FourCc::H264,
            FourCc::NV12,
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "h264_v4l2request",
            "h264",
            "h264_v4l2request",
            FourCc::H264,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "h264_v4l2m2m",
            "h264",
            "h264_v4l2m2m",
            FourCc::H264,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH264Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.0.process_shared(input, pool)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegH264Decoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}

pub struct FfmpegH265Decoder(pub FfmpegVideoDecoder);

impl FfmpegH265Decoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        Self::new_rgb24_for_input(FourCc::H265)
    }

    pub fn new_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::H265,
            FourCc::NV12,
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::H265, zero_copy, threads, pool_limits)
    }

    pub fn with_options_for_input(
        input: FourCc,
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::RG24,
            zero_copy,
            threads,
            pool_limits,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_nv12_zero_copy() -> Result<Self, CodecError> {
        Self::new_v4l2request_nv12_zero_copy_for_input(FourCc::H265)
    }

    pub fn new_v4l2request_nv12_zero_copy_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "hevc_v4l2request",
            "h265",
            "hevc_v4l2request_nv12",
            input,
            FourCc::NV12,
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_rgb24() -> Result<Self, CodecError> {
        Self::new_v4l2request_rgb24_for_input(FourCc::H265)
    }

    pub fn new_v4l2request_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "hevc_v4l2request",
            "h265",
            "hevc_v4l2request",
            input,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        Self::new_v4l2m2m_rgb24_for_input(FourCc::H265)
    }

    pub fn new_v4l2m2m_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "hevc_v4l2m2m",
            "h265",
            "hevc_v4l2m2m",
            input,
            FourCc::RG24,
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH265Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.0.process_shared(input, pool)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegH265Decoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}
