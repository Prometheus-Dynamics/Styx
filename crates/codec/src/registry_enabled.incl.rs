impl CodecRegistry {
    pub fn register_enabled_codecs(
        &self,
        max_width: u32,
        max_height: u32,
    ) -> Result<(), crate::CodecError> {
        let max_width = max_width.max(1);
        let max_height = max_height.max(1);

        self.register(
            FourCc::new(*b"MJPG"),
            Arc::new(crate::mjpeg::MjpegDecoder::new(FourCc::new(*b"RG24"))),
        );
        self.register(
            FourCc::new(*b"JPEG"),
            Arc::new(crate::mjpeg::MjpegDecoder::new_for_input(
                FourCc::new(*b"JPEG"),
                FourCc::new(*b"RG24"),
            )),
        );
        self.register(
            FourCc::new(*b"BGR3"),
            Arc::new(crate::decoder::raw::BgrToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"BGRA"),
            Arc::new(crate::decoder::raw::BgraToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"RGBA"),
            Arc::new(crate::decoder::raw::RgbaToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"YUYV"),
            Arc::new(crate::decoder::raw::YuyvToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"YUYV"),
            Arc::new(crate::decoder::raw::YuyvToLumaDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV12"),
            Arc::new(crate::decoder::raw::Nv12ToRgbDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV12"),
            Arc::new(crate::decoder::raw::Nv12ToLumaDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"I420"),
            Arc::new(crate::decoder::raw::I420ToRgbDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YU12"),
            Arc::new(crate::decoder::raw::Yuv420pToRgbDecoder::new(
                FourCc::new(*b"YU12"),
                "yu12-cpu",
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV12"),
            Arc::new(crate::decoder::raw::Yuv420pToRgbDecoder::new(
                FourCc::new(*b"YV12"),
                "yv12-cpu",
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"R8  "),
            Arc::new(crate::decoder::raw::Mono8ToRgbDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"R16 "),
            Arc::new(crate::decoder::raw::Mono16ToRgbDecoder::new(
                max_width, max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV21"),
            Arc::new(crate::decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV21"),
                "nv21-cpu",
                2,
                2,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV16"),
            Arc::new(crate::decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV16"),
                "nv16-cpu",
                2,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV61"),
            Arc::new(crate::decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV61"),
                "nv61-cpu",
                2,
                1,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV24"),
            Arc::new(crate::decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV24"),
                "nv24-cpu",
                1,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV42"),
            Arc::new(crate::decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV42"),
                "nv42-cpu",
                1,
                1,
                false,
                max_width,
                max_height,
            )),
        );

        for code in [*b"RG24", *b"RGB3", *b"RGB6"] {
            self.register(
                FourCc::new(code),
                Arc::new(crate::decoder::raw::PassthroughDecoder::new(FourCc::new(code))),
            );
        }

        let bayer_codes = [
            *b"BA81", *b"BA10", *b"BA12", *b"BA14", *b"BG10", *b"BG12", *b"BG14", *b"BG16",
            *b"GB10", *b"GB12", *b"GB14", *b"GB16", *b"RG10", *b"RG12", *b"RG14", *b"RG16",
            *b"GR10", *b"GR12", *b"GR14", *b"GR16", *b"BYR2", *b"RGGB", *b"GRBG", *b"GBRG",
            *b"BGGR", *b"pBAA", *b"pGAA", *b"pgAA", *b"pRAA", *b"pBCC", *b"pGCC", *b"pgCC",
            *b"pRCC",
        ];
        for code in bayer_codes {
            let fcc = FourCc::new(code);
            if let Some(info) = crate::decoder::raw::bayer_info(fcc) {
                self.register(
                    fcc,
                    crate::decoder::raw::bayer_decoder_for(fcc, info, max_width, max_height),
                );
            }
        }

        self.register(
            FourCc::new(*b"YV12"),
            Arc::new(crate::decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV12"),
                "yv12-cpu",
                2,
                2,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YU16"),
            Arc::new(crate::decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YU16"),
                "yu16-cpu",
                2,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV16"),
            Arc::new(crate::decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV16"),
                "yv16-cpu",
                2,
                1,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YU24"),
            Arc::new(crate::decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YU24"),
                "yu24-cpu",
                1,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV24"),
            Arc::new(crate::decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV24"),
                "yv24-cpu",
                1,
                1,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YVYU"),
            Arc::new(crate::decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"YVYU"),
                "yvyu-cpu",
                [0, 3, 2, 1],
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"UYVY"),
            Arc::new(crate::decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"UYVY"),
                "uyvy-cpu",
                [1, 0, 3, 2],
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"VYUY"),
            Arc::new(crate::decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"VYUY"),
                "vyuy-cpu",
                [1, 2, 3, 0],
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"BG24"),
            Arc::new(crate::decoder::raw::BgrToRgbDecoder::with_input_for_max(
                FourCc::new(*b"BG24"),
                "bg24-swap",
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"XB24"),
            Arc::new(crate::decoder::raw::BgraToRgbDecoder::with_input_for_max(
                FourCc::new(*b"XB24"),
                "xb24-strip",
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"XR24"),
            Arc::new(crate::decoder::raw::RgbaToRgbDecoder::with_input_for_max(
                FourCc::new(*b"XR24"),
                "xr24-strip",
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"BG48"),
            Arc::new(crate::decoder::raw::Rgb48ToRgbDecoder::new(
                FourCc::new(*b"BG48"),
                "bg48-strip",
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"RG48"),
            Arc::new(crate::decoder::raw::Rgb48ToRgbDecoder::new(
                FourCc::new(*b"RG48"),
                "rg48-strip",
                false,
                max_width,
                max_height,
            )),
        );

        #[cfg(feature = "dynamic-image")]
        self.register(
            FourCc::new(*b"ANY "),
            Arc::new(crate::image_any::ImageAnyDecoder::new(FourCc::new(*b"RGBA"))),
        );

        #[cfg(feature = "codec-ffmpeg")]
        {
            use crate::ffmpeg::{
                FfmpegEncoderOptions, FfmpegH264Decoder, FfmpegH264Encoder, FfmpegH265Decoder,
                FfmpegH265Encoder, FfmpegMjpegDecoder, FfmpegMjpegEncoder,
            };
            let default_decoder_threads = std::env::var("STYX_FFMPEG_DECODER_THREADS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0);
            self.register(
                FourCc::new(*b"MJPG"),
                Arc::new(FfmpegMjpegDecoder::with_options_for_input(
                    FourCc::new(*b"MJPG"),
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            self.register(
                FourCc::new(*b"JPEG"),
                Arc::new(FfmpegMjpegDecoder::with_options_for_input(
                    FourCc::new(*b"JPEG"),
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            self.register(
                FourCc::new(*b"H264"),
                Arc::new(FfmpegH264Decoder::with_options(
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            if let Ok(dec) = FfmpegH264Decoder::new_v4l2request_nv12_zero_copy() {
                self.register(FourCc::new(*b"H264"), Arc::new(dec));
            }
            if let Ok(dec) = FfmpegH264Decoder::new_v4l2request_rgb24() {
                self.register(FourCc::new(*b"H264"), Arc::new(dec));
            }
            if let Ok(dec) = FfmpegH264Decoder::new_v4l2m2m_rgb24() {
                self.register(FourCc::new(*b"H264"), Arc::new(dec));
            }
            self.register(
                FourCc::new(*b"H265"),
                Arc::new(FfmpegH265Decoder::with_options_for_input(
                    FourCc::new(*b"H265"),
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            self.register(
                FourCc::new(*b"HEVC"),
                Arc::new(FfmpegH265Decoder::with_options_for_input(
                    FourCc::new(*b"HEVC"),
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            if let Ok(dec) = FfmpegH265Decoder::new_v4l2request_rgb24() {
                self.register(FourCc::new(*b"H265"), Arc::new(dec));
            }
            if let Ok(dec) = FfmpegH265Decoder::new_v4l2request_nv12_zero_copy() {
                self.register(FourCc::new(*b"H265"), Arc::new(dec));
            }
            if let Ok(dec) =
                FfmpegH265Decoder::new_v4l2request_nv12_zero_copy_for_input(FourCc::new(*b"HEVC"))
            {
                self.register(FourCc::new(*b"HEVC"), Arc::new(dec));
            }
            if let Ok(dec) =
                FfmpegH265Decoder::new_v4l2request_rgb24_for_input(FourCc::new(*b"HEVC"))
            {
                self.register(FourCc::new(*b"HEVC"), Arc::new(dec));
            }
            if let Ok(dec) = FfmpegH265Decoder::new_v4l2m2m_rgb24() {
                self.register(FourCc::new(*b"H265"), Arc::new(dec));
            }
            if let Ok(dec) =
                FfmpegH265Decoder::new_v4l2m2m_rgb24_for_input(FourCc::new(*b"HEVC"))
            {
                self.register(FourCc::new(*b"HEVC"), Arc::new(dec));
            }
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegMjpegEncoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegMjpegEncoder::new_nv12()?),
            );
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegMjpegEncoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegH264Encoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegH264Encoder::new_nv12()?),
            );
            if v4l2m2m_probe_enabled() {
                match FfmpegH264Encoder::new_v4l2m2m_rgb24() {
                    Ok(enc) => self.register(FourCc::new(*b"RG24"), Arc::new(enc)),
                    Err(_) => disable_v4l2m2m_probe(),
                }
                if v4l2m2m_probe_enabled()
                    && let Ok(enc) = FfmpegH264Encoder::new_v4l2m2m_nv12()
                {
                    self.register(FourCc::new(*b"NV12"), Arc::new(enc));
                }
            }
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegH264Encoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegH265Encoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegH265Encoder::new_nv12()?),
            );
            if v4l2m2m_probe_enabled() {
                if let Ok(enc) = FfmpegH265Encoder::new_v4l2m2m_rgb24() {
                    self.register(FourCc::new(*b"RG24"), Arc::new(enc));
                } else {
                    disable_v4l2m2m_probe();
                }
                if v4l2m2m_probe_enabled()
                    && let Ok(enc) = FfmpegH265Encoder::new_v4l2m2m_nv12()
                {
                    self.register(FourCc::new(*b"NV12"), Arc::new(enc));
                }
            }
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegH265Encoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
        }

        #[cfg(feature = "codec-mozjpeg")]
        self.register(
            FourCc::new(*b"RG24"),
            Arc::new(crate::jpeg_encoder::MozjpegEncoder::new(
                FourCc::new(*b"RG24"),
                85,
            )),
        );

        #[cfg(feature = "codec-turbojpeg")]
        {
            self.register(
                FourCc::new(*b"R8  "),
                Arc::new(crate::mjpeg_turbojpeg::TurbojpegEncoder::new(
                    FourCc::new(*b"R8  "),
                    85,
                )),
            );
            self.register(
                FourCc::new(*b"GREY"),
                Arc::new(crate::mjpeg_turbojpeg::TurbojpegEncoder::new(
                    FourCc::new(*b"GREY"),
                    85,
                )),
            );
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(crate::mjpeg_turbojpeg::TurbojpegEncoder::new(
                    FourCc::new(*b"RG24"),
                    85,
                )),
            );
            self.register(
                FourCc::new(*b"RGBA"),
                Arc::new(crate::mjpeg_turbojpeg::TurbojpegEncoder::new(
                    FourCc::new(*b"RGBA"),
                    85,
                )),
            );
            self.register(
                FourCc::new(*b"MJPG"),
                Arc::new(crate::mjpeg_turbojpeg::TurbojpegDecoder::new(
                    FourCc::new(*b"RG24"),
                )),
            );
        }

        #[cfg(feature = "codec-zune")]
        self.register(
            FourCc::new(*b"MJPG"),
            Arc::new(crate::mjpeg_zune::ZuneMjpegDecoder::new(FourCc::new(*b"RG24"))),
        );

        Ok(())
    }

    pub fn list_enabled_codecs() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, crate::CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered())
    }

    pub fn list_enabled_decoders() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, crate::CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered_by_kind(crate::CodecKind::Decoder))
    }

    pub fn list_enabled_encoders() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, crate::CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered_by_kind(crate::CodecKind::Encoder))
    }
}
