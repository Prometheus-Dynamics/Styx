use super::*;

/// A codec id (`AVCodecID`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(pub raw::AVCodecID);

#[allow(non_upper_case_globals)]
impl Id {
    pub const H264: Self = Self(raw::AVCodecID::AV_CODEC_ID_H264);
    pub const HEVC: Self = Self(raw::AVCodecID::AV_CODEC_ID_HEVC);
    pub const MJPEG: Self = Self(raw::AVCodecID::AV_CODEC_ID_MJPEG);
}

/// A codec implementation (`AVCodec`), owned by FFmpeg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Codec {
    ptr: *const raw::AVCodec,
}

// SAFETY: AVCodec objects are immutable static descriptions owned by libavcodec.
unsafe impl Send for Codec {}
// SAFETY: as above.
unsafe impl Sync for Codec {}

impl Codec {
    fn from_ptr(ptr: *const raw::AVCodec) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    pub unsafe fn as_ptr(&self) -> *const raw::AVCodec {
        self.ptr
    }

    pub fn name(&self) -> &'static str {
        // SAFETY: AVCodec names are static NUL-terminated strings.
        unsafe { CStr::from_ptr((*self.ptr).name) }
            .to_str()
            .unwrap_or("")
    }

    pub fn id(&self) -> Id {
        // SAFETY: valid static AVCodec.
        Id(unsafe { (*self.ptr).id })
    }

    pub fn is_decoder(&self) -> bool {
        // SAFETY: valid codec pointer.
        unsafe { (loader::loaded().codec.av_codec_is_decoder)(self.ptr) != 0 }
    }

    pub fn is_encoder(&self) -> bool {
        // SAFETY: valid codec pointer.
        unsafe { (loader::loaded().codec.av_codec_is_encoder)(self.ptr) != 0 }
    }

    pub fn video(self) -> Result<VideoCodec, Error> {
        // SAFETY: valid static AVCodec.
        if unsafe { (*self.ptr).type_ } == raw::AVMediaType::AVMEDIA_TYPE_VIDEO {
            Ok(VideoCodec(self))
        } else {
            Err(Error::InvalidData)
        }
    }
}

/// A codec to find on first use, so creating a Styx codec does not load FFmpeg.
#[derive(Debug)]
pub struct CodecRef {
    source: CodecSource,
    found: std::sync::OnceLock<Option<Codec>>,
}

#[derive(Clone, Debug)]
enum CodecSource {
    Decoder(Id),
    DecoderName(String),
    Encoder(Id),
    EncoderName(String),
}

impl CodecRef {
    pub fn decoder(id: Id) -> Self {
        Self::new(CodecSource::Decoder(id))
    }

    pub fn decoder_by_name(name: &str) -> Self {
        Self::new(CodecSource::DecoderName(name.to_string()))
    }

    pub fn encoder(id: Id) -> Self {
        Self::new(CodecSource::Encoder(id))
    }

    pub fn encoder_by_name(name: &str) -> Self {
        Self::new(CodecSource::EncoderName(name.to_string()))
    }

    fn new(source: CodecSource) -> Self {
        Self {
            source,
            found: std::sync::OnceLock::new(),
        }
    }

    /// The codec, loading FFmpeg and looking it up the first time.
    pub fn get(&self) -> Result<Codec, Error> {
        init()?;
        (*self.found.get_or_init(|| match &self.source {
            CodecSource::Decoder(id) => decoder::find(*id),
            CodecSource::DecoderName(name) => decoder::find_by_name(name),
            CodecSource::Encoder(id) => encoder::find(*id),
            CodecSource::EncoderName(name) => encoder::find_by_name(name),
        }))
        .ok_or_else(|| Error::Unavailable(format!("ffmpeg codec {self} not found")))
    }
}

impl fmt::Display for CodecRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            CodecSource::Decoder(id) => write!(f, "decoder {:?}", id.0),
            CodecSource::DecoderName(name) | CodecSource::EncoderName(name) => f.write_str(name),
            CodecSource::Encoder(id) => write!(f, "encoder {:?}", id.0),
        }
    }
}

/// A video codec implementation.
pub struct VideoCodec(Codec);

impl VideoCodec {
    /// Pixel formats the codec accepts or produces, when it reports them.
    pub fn formats(&self) -> Option<std::vec::IntoIter<Pixel>> {
        let get = loader::loaded().get_supported_config?;
        let mut configs: *const std::ffi::c_void = ptr::null();
        let mut count: c_int = 0;
        // SAFETY: AV_CODEC_CONFIG_PIX_FORMAT (0) returns an AVPixelFormat array of `count`.
        let ret = unsafe { get(ptr::null(), self.0.ptr, 0, 0, &mut configs, &mut count) };
        if ret < 0 || configs.is_null() || count <= 0 {
            return None;
        }
        let formats = configs as *const raw::AVPixelFormat;
        // SAFETY: FFmpeg returned `count` formats at `formats`.
        let list = unsafe { std::slice::from_raw_parts(formats, count as usize) };
        Some(
            list.iter()
                .map(|f| Pixel(*f))
                .collect::<Vec<_>>()
                .into_iter(),
        )
    }
}

/// Stream parameters (`AVCodecParameters`), owned.
pub struct Parameters {
    ptr: *mut raw::AVCodecParameters,
}

// SAFETY: owned parameters are plain data.
unsafe impl Send for Parameters {}

impl Parameters {
    /// A copy of `src` (borrowed from a stream).
    pub(crate) fn copy_from(src: *const raw::AVCodecParameters) -> Result<Self, Error> {
        let core = loader::loaded();
        // SAFETY: fresh allocation, then copy from a valid source.
        unsafe {
            let ptr = (core.codec.avcodec_parameters_alloc)();
            if ptr.is_null() {
                return Err(Error::Other {
                    errno: libc::ENOMEM,
                });
            }
            let params = Self { ptr };
            check((core.codec.avcodec_parameters_copy)(ptr, src))?;
            Ok(params)
        }
    }

    pub fn id(&self) -> Id {
        // SAFETY: valid owned parameters.
        Id(unsafe { (*self.ptr).codec_id })
    }

    pub fn as_ptr(&self) -> *const raw::AVCodecParameters {
        self.ptr
    }
}

impl Clone for Parameters {
    fn clone(&self) -> Self {
        Self::copy_from(self.ptr).expect("copy codec parameters")
    }
}

impl Drop for Parameters {
    fn drop(&mut self) {
        // SAFETY: owned pointer from avcodec_parameters_alloc.
        unsafe { (loader::loaded().codec.avcodec_parameters_free)(&mut self.ptr) };
    }
}

pub mod threading {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Type {
        Frame,
        Slice,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Config {
        pub kind: Type,
        pub count: usize,
    }
}

/// An owned codec context (`AVCodecContext`).
pub struct Context {
    ptr: *mut raw::AVCodecContext,
}

// SAFETY: a codec context is used by one thread at a time (callers hold it in a mutex).
unsafe impl Send for Context {}

impl Context {
    fn alloc(codec: *const raw::AVCodec) -> Self {
        // SAFETY: codec may be null (allocates a generic context).
        let ptr = unsafe { (loader::loaded().codec.avcodec_alloc_context3)(codec) };
        assert!(!ptr.is_null(), "avcodec_alloc_context3 failed");
        Self { ptr }
    }

    pub fn new_with_codec(codec: Codec) -> Self {
        Self::alloc(codec.ptr)
    }

    pub fn from_parameters(parameters: Parameters) -> Result<Self, Error> {
        let mut context = Self::alloc(ptr::null());
        context.set_parameters(parameters)?;
        Ok(context)
    }

    pub fn set_parameters(&mut self, parameters: Parameters) -> Result<(), Error> {
        // SAFETY: valid context and parameters.
        check(unsafe {
            (loader::loaded().codec.avcodec_parameters_to_context)(self.ptr, parameters.ptr)
        })
        .map(|_| ())
    }

    pub fn set_threading(&mut self, config: threading::Config) {
        // SAFETY: valid owned context; fields are plain integers.
        unsafe {
            (*self.ptr).thread_count = config.count as c_int;
            (*self.ptr).thread_type = match config.kind {
                // FF_THREAD_FRAME / FF_THREAD_SLICE (libavcodec/avcodec.h).
                threading::Type::Frame => 1,
                threading::Type::Slice => 2,
            };
        }
    }

    pub fn as_ptr(&self) -> *const raw::AVCodecContext {
        self.ptr
    }

    pub unsafe fn as_mut_ptr(&mut self) -> *mut raw::AVCodecContext {
        self.ptr
    }

    pub fn decoder(self) -> decoder::Decoder {
        decoder::Decoder(self)
    }

    pub fn encoder(self) -> encoder::Encoder {
        encoder::Encoder(self)
    }

    fn open(self, codec: *const raw::AVCodec) -> Result<Self, Error> {
        self.open_with(codec, &[])
    }

    /// Open with codec options (`AVDictionary` entries such as `preset`); unknown keys are
    /// left unused by FFmpeg.
    fn open_with(
        self,
        codec: *const raw::AVCodec,
        options: &[(&str, &str)],
    ) -> Result<Self, Error> {
        let util = &loader::loaded().util;
        let mut dict: *mut raw::AVDictionary = ptr::null_mut();
        for (key, value) in options {
            let (key, value) = (cstring(key)?, cstring(value)?);
            // SAFETY: valid NUL-terminated strings; FFmpeg copies them into the dictionary.
            check(unsafe { (util.av_dict_set)(&mut dict, key.as_ptr(), value.as_ptr(), 0) })?;
        }
        // SAFETY: valid context; a null codec uses the one set on the context. FFmpeg leaves
        // the unused entries in `dict`, which is freed below.
        let opened =
            check(unsafe { (loader::loaded().codec.avcodec_open2)(self.ptr, codec, &mut dict) });
        // SAFETY: the dictionary (possibly null) is ours to free.
        unsafe { (util.av_dict_free)(&mut dict) };
        opened?;
        Ok(self)
    }

    fn width(&self) -> u32 {
        // SAFETY: valid context.
        unsafe { (*self.ptr).width.max(0) as u32 }
    }

    fn height(&self) -> u32 {
        // SAFETY: valid context.
        unsafe { (*self.ptr).height.max(0) as u32 }
    }

    fn format(&self) -> Pixel {
        // SAFETY: valid context.
        Pixel(unsafe { (*self.ptr).pix_fmt })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: owned pointer from avcodec_alloc_context3.
        unsafe { (loader::loaded().codec.avcodec_free_context)(&mut self.ptr) };
    }
}

pub mod decoder {
    use super::*;

    pub fn find(id: Id) -> Option<Codec> {
        let core = loader::core().ok()?;
        // SAFETY: plain lookup.
        Codec::from_ptr(unsafe { (core.codec.avcodec_find_decoder)(id.0) })
    }

    pub fn find_by_name(name: &str) -> Option<Codec> {
        let core = loader::core().ok()?;
        let name = cstring(name).ok()?;
        // SAFETY: NUL-terminated name.
        Codec::from_ptr(unsafe { (core.codec.avcodec_find_decoder_by_name)(name.as_ptr()) })
    }

    /// An unopened decoder context.
    pub struct Decoder(pub(super) Context);

    impl Decoder {
        /// Open as a video decoder (finding the codec from the context's id if unset).
        pub fn video(self) -> Result<Video, Error> {
            let ctx = self.0;
            // SAFETY: valid context.
            let mut codec = unsafe { (*ctx.ptr).codec };
            if codec.is_null() {
                // SAFETY: plain lookup by the context's codec id.
                codec =
                    unsafe { (loader::loaded().codec.avcodec_find_decoder)((*ctx.ptr).codec_id) };
                if codec.is_null() {
                    return Err(Error::Other {
                        errno: libc::ENOENT,
                    });
                }
            }
            Ok(Video(ctx.open(codec)?))
        }
    }

    /// An opened video decoder.
    pub struct Video(Context);

    impl Video {
        pub fn send_packet(
            &mut self,
            packet: &crate::ffmpeg::ff::packet::Packet,
        ) -> Result<(), Error> {
            // SAFETY: opened context and valid packet.
            check(unsafe {
                (loader::loaded().codec.avcodec_send_packet)(self.0.ptr, packet.as_ptr())
            })
            .map(|_| ())
        }

        pub fn send_eof(&mut self) -> Result<(), Error> {
            // SAFETY: a null packet enters draining mode.
            check(unsafe { (loader::loaded().codec.avcodec_send_packet)(self.0.ptr, ptr::null()) })
                .map(|_| ())
        }

        pub fn receive_frame(
            &mut self,
            frame: &mut crate::ffmpeg::ff::frame::Video,
        ) -> Result<(), Error> {
            // SAFETY: opened context and owned frame.
            check(unsafe {
                (loader::loaded().codec.avcodec_receive_frame)(self.0.ptr, frame.as_mut_ptr())
            })
            .map(|_| ())
        }

        pub fn width(&self) -> u32 {
            self.0.width()
        }

        pub fn height(&self) -> u32 {
            self.0.height()
        }

        pub fn format(&self) -> Pixel {
            self.0.format()
        }

        pub fn as_ptr(&self) -> *const raw::AVCodecContext {
            self.0.ptr
        }

        pub unsafe fn as_mut_ptr(&mut self) -> *mut raw::AVCodecContext {
            self.0.ptr
        }
    }
}

pub mod encoder {
    use super::*;

    pub fn find(id: Id) -> Option<Codec> {
        let core = loader::core().ok()?;
        // SAFETY: plain lookup.
        Codec::from_ptr(unsafe { (core.codec.avcodec_find_encoder)(id.0) })
    }

    pub fn find_by_name(name: &str) -> Option<Codec> {
        let core = loader::core().ok()?;
        let name = cstring(name).ok()?;
        // SAFETY: NUL-terminated name.
        Codec::from_ptr(unsafe { (core.codec.avcodec_find_encoder_by_name)(name.as_ptr()) })
    }

    /// An unopened encoder context.
    pub struct Encoder(pub(super) Context);

    impl Encoder {
        pub fn video(self) -> Result<Video, Error> {
            Ok(Video(self.0))
        }
    }

    /// Settings shared by unopened and opened video encoders.
    macro_rules! video_settings {
        ($ty:ty) => {
            impl $ty {
                fn raw(&mut self) -> *mut raw::AVCodecContext {
                    self.0.ptr
                }

                /// Encode from device surfaces of `frames` (an `AVHWFramesContext`); takes a new
                /// reference to it.
                ///
                /// # Safety
                /// `frames` must be a valid hardware frames context reference.
                pub unsafe fn set_hw_frames(&mut self, frames: *mut raw::AVBufferRef) {
                    // SAFETY: valid owned context; the context owns the new reference.
                    unsafe {
                        (*self.raw()).hw_frames_ctx = (loader::loaded().util.av_buffer_ref)(frames);
                    }
                }

                pub fn set_width(&mut self, width: u32) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).width = width as c_int };
                }

                pub fn set_height(&mut self, height: u32) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).height = height as c_int };
                }

                pub fn set_format(&mut self, format: Pixel) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).pix_fmt = format.0 };
                }

                pub fn set_time_base(&mut self, (num, den): (i32, i32)) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).time_base = raw::AVRational { num, den } };
                }

                pub fn set_frame_rate(&mut self, rate: Option<(i32, i32)>) {
                    let (num, den) = rate.unwrap_or((0, 1));
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).framerate = raw::AVRational { num, den } };
                }

                pub fn set_bit_rate(&mut self, bit_rate: usize) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).bit_rate = bit_rate.min(i64::MAX as usize) as i64 };
                }

                pub fn set_max_b_frames(&mut self, frames: usize) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).max_b_frames = frames as c_int };
                }

                pub fn set_gop(&mut self, gop: u32) {
                    // SAFETY: valid owned context.
                    unsafe { (*self.raw()).gop_size = gop as c_int };
                }

                pub fn set_threading(&mut self, config: threading::Config) {
                    self.0.set_threading(config);
                }
            }
        };
    }

    /// An unopened video encoder.
    pub struct Video(Context);
    video_settings!(Video);

    impl Video {
        pub fn open_as(self, codec: Codec) -> Result<video::Encoder, Error> {
            Ok(video::Encoder(self.0.open(codec.ptr)?))
        }

        /// Open with codec options such as `("preset", "veryfast")`.
        pub fn open_as_with(
            self,
            codec: Codec,
            options: &[(&str, &str)],
        ) -> Result<video::Encoder, Error> {
            Ok(video::Encoder(self.0.open_with(codec.ptr, options)?))
        }
    }

    pub mod video {
        use super::*;

        /// An opened video encoder.
        pub struct Encoder(pub(super) Context);
        video_settings!(Encoder);

        impl Encoder {
            pub fn send_frame(
                &mut self,
                frame: &crate::ffmpeg::ff::frame::Video,
            ) -> Result<(), Error> {
                // SAFETY: opened context and valid frame.
                check(unsafe {
                    (loader::loaded().codec.avcodec_send_frame)(self.0.ptr, frame.as_ptr())
                })
                .map(|_| ())
            }

            pub fn send_eof(&mut self) -> Result<(), Error> {
                // SAFETY: a null frame enters draining mode.
                check(unsafe {
                    (loader::loaded().codec.avcodec_send_frame)(self.0.ptr, ptr::null())
                })
                .map(|_| ())
            }

            pub fn receive_packet(
                &mut self,
                packet: &mut crate::ffmpeg::ff::packet::Packet,
            ) -> Result<(), Error> {
                // SAFETY: opened context and owned packet.
                check(unsafe {
                    (loader::loaded().codec.avcodec_receive_packet)(self.0.ptr, packet.as_mut_ptr())
                })
                .map(|_| ())
            }
        }
    }
}
