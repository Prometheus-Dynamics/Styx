use super::*;

pub struct FfmpegMjpegEncoder(pub FfmpegVideoEncoder);

impl FfmpegMjpegEncoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"MJPG"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"MJPG"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::new(*b"MJPG"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegMjpegEncoder {
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

pub struct FfmpegH264Encoder(pub FfmpegVideoEncoder);

impl FfmpegH264Encoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        let enc = FfmpegVideoEncoder::new_by_name(
            "h264_v4l2m2m",
            "h264",
            "h264_v4l2m2m",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )?;
        probe_v4l2m2m_encoder(&enc)?;
        Ok(Self(enc))
    }

    pub fn new_v4l2m2m_nv12() -> Result<Self, CodecError> {
        let enc = FfmpegVideoEncoder::new_by_name(
            "h264_v4l2m2m",
            "h264",
            "h264_v4l2m2m",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )?;
        probe_v4l2m2m_encoder(&enc)?;
        Ok(Self(enc))
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            input,
            FourCc::new(*b"H264"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH264Encoder {
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

pub struct FfmpegH265Encoder(pub FfmpegVideoEncoder);

impl FfmpegH265Encoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        let enc = FfmpegVideoEncoder::new_by_name(
            "hevc_v4l2m2m",
            "h265",
            "hevc_v4l2m2m",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )?;
        probe_v4l2m2m_encoder(&enc)?;
        Ok(Self(enc))
    }

    pub fn new_v4l2m2m_nv12() -> Result<Self, CodecError> {
        let enc = FfmpegVideoEncoder::new_by_name(
            "hevc_v4l2m2m",
            "h265",
            "hevc_v4l2m2m",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )?;
        probe_v4l2m2m_encoder(&enc)?;
        Ok(Self(enc))
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::new(*b"H265"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH265Encoder {
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
