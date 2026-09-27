//! Runtime loading of the FFmpeg libraries.
//!
//! Nothing in Styx links FFmpeg: the libraries are opened with `dlopen` the first time an FFmpeg
//! codec, scaler or stream is used, so processes that never decode or encode through FFmpeg never
//! map it (on a Raspberry Pi CM5 loading it costs about 2.2 MB of system memory, 5.5 MB PSS with
//! the first decoder).
//! Libraries are opened by the major version Styx was compiled against (e.g.
//! `libavcodec.so.62`), because the struct layouts come from those headers.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use ffmpeg_sys_next as raw;

/// A loaded shared library; never closed (the process keeps FFmpeg once it has used it).
struct Library(*mut c_void);

// SAFETY: a dlopen handle is process-global and only used with dlsym.
unsafe impl Send for Library {}
// SAFETY: as above; dlsym is thread-safe.
unsafe impl Sync for Library {}

impl Library {
    fn open(stem: &str, major: c_int) -> Result<Self, String> {
        // Only the major version the bindings were generated for: another major has a different
        // ABI.
        let names = [
            format!("lib{stem}.so.{major}"),
            format!("lib{stem}.{major}.dylib"),
        ];
        let mut errors = Vec::new();
        for name in &names {
            let cname = CString::new(name.as_str()).expect("library name");
            // SAFETY: dlopen with a valid NUL-terminated name.
            let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                return Ok(Self(handle));
            }
            errors.push(dlerror());
        }
        Err(format!(
            "FFmpeg library lib{stem} (major {major}) not found: {}",
            errors.join("; ")
        ))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void, String> {
        let cname = CString::new(name).expect("symbol name");
        // SAFETY: valid handle from dlopen and NUL-terminated name.
        let sym = unsafe { libc::dlsym(self.0, cname.as_ptr()) };
        if sym.is_null() {
            Err(format!("FFmpeg symbol {name} not found: {}", dlerror()))
        } else {
            Ok(sym)
        }
    }

    fn optional_symbol(&self, name: &str) -> Option<*mut c_void> {
        self.symbol(name).ok()
    }
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a thread-local message or null.
    let msg = unsafe { libc::dlerror() };
    if msg.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null NUL-terminated string owned by libc.
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Declares a table of FFmpeg functions resolved from one library.
macro_rules! function_table {
    ($table:ident { $( $name:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )* }) => {
        #[allow(non_snake_case)]
        pub(crate) struct $table {
            $( pub(crate) $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl $table {
            fn load(lib: &Library) -> Result<Self, String> {
                Ok(Self {
                    $(
                        // SAFETY: the symbol has this C signature in the FFmpeg headers the
                        // bindings were generated from.
                        $name: unsafe {
                            std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(
                                lib.symbol(stringify!($name))?,
                            )
                        },
                    )*
                })
            }
        }
    };
}

function_table!(AvUtil {
    av_frame_alloc: fn() -> *mut raw::AVFrame;
    av_frame_free: fn(*mut *mut raw::AVFrame);
    av_frame_get_buffer: fn(*mut raw::AVFrame, c_int) -> c_int;
    av_frame_copy_props: fn(*mut raw::AVFrame, *const raw::AVFrame) -> c_int;
    av_frame_clone: fn(*const raw::AVFrame) -> *mut raw::AVFrame;
    av_buffer_ref: fn(*const raw::AVBufferRef) -> *mut raw::AVBufferRef;
    av_buffer_unref: fn(*mut *mut raw::AVBufferRef);
    av_hwdevice_ctx_create: fn(*mut *mut raw::AVBufferRef, raw::AVHWDeviceType, *const c_char, *mut raw::AVDictionary, c_int) -> c_int;
    av_hwframe_transfer_data: fn(*mut raw::AVFrame, *const raw::AVFrame, c_int) -> c_int;
    av_pix_fmt_desc_get: fn(raw::AVPixelFormat) -> *const raw::AVPixFmtDescriptor;
    av_strerror: fn(c_int, *mut c_char, usize) -> c_int;
});

function_table!(AvCodec {
    avcodec_find_decoder: fn(raw::AVCodecID) -> *const raw::AVCodec;
    avcodec_find_decoder_by_name: fn(*const c_char) -> *const raw::AVCodec;
    avcodec_find_encoder: fn(raw::AVCodecID) -> *const raw::AVCodec;
    avcodec_find_encoder_by_name: fn(*const c_char) -> *const raw::AVCodec;
    avcodec_alloc_context3: fn(*const raw::AVCodec) -> *mut raw::AVCodecContext;
    avcodec_free_context: fn(*mut *mut raw::AVCodecContext);
    avcodec_open2: fn(*mut raw::AVCodecContext, *const raw::AVCodec, *mut *mut raw::AVDictionary) -> c_int;
    avcodec_parameters_to_context: fn(*mut raw::AVCodecContext, *const raw::AVCodecParameters) -> c_int;
    avcodec_parameters_alloc: fn() -> *mut raw::AVCodecParameters;
    avcodec_parameters_free: fn(*mut *mut raw::AVCodecParameters);
    avcodec_parameters_copy: fn(*mut raw::AVCodecParameters, *const raw::AVCodecParameters) -> c_int;
    avcodec_send_packet: fn(*mut raw::AVCodecContext, *const raw::AVPacket) -> c_int;
    avcodec_receive_frame: fn(*mut raw::AVCodecContext, *mut raw::AVFrame) -> c_int;
    avcodec_send_frame: fn(*mut raw::AVCodecContext, *const raw::AVFrame) -> c_int;
    avcodec_receive_packet: fn(*mut raw::AVCodecContext, *mut raw::AVPacket) -> c_int;
    avcodec_get_hw_config: fn(*const raw::AVCodec, c_int) -> *const raw::AVCodecHWConfig;
    av_codec_is_decoder: fn(*const raw::AVCodec) -> c_int;
    av_codec_is_encoder: fn(*const raw::AVCodec) -> c_int;
    av_packet_alloc: fn() -> *mut raw::AVPacket;
    av_packet_free: fn(*mut *mut raw::AVPacket);
    av_new_packet: fn(*mut raw::AVPacket, c_int) -> c_int;
});

function_table!(SwScale {
    sws_getContext: fn(c_int, c_int, raw::AVPixelFormat, c_int, c_int, raw::AVPixelFormat, c_int, *mut raw::SwsFilter, *mut raw::SwsFilter, *const f64) -> *mut raw::SwsContext;
    sws_scale: fn(*mut raw::SwsContext, *const *const u8, *const c_int, c_int, c_int, *const *mut u8, *const c_int) -> c_int;
    sws_freeContext: fn(*mut raw::SwsContext);
});

#[cfg(feature = "codec-ffmpeg-format")]
function_table!(AvFormat {
    avformat_alloc_context: fn() -> *mut raw::AVFormatContext;
    avformat_open_input: fn(*mut *mut raw::AVFormatContext, *const c_char, *const raw::AVInputFormat, *mut *mut raw::AVDictionary) -> c_int;
    avformat_find_stream_info: fn(*mut raw::AVFormatContext, *mut *mut raw::AVDictionary) -> c_int;
    av_find_best_stream: fn(*mut raw::AVFormatContext, raw::AVMediaType, c_int, c_int, *mut *const raw::AVCodec, c_int) -> c_int;
    av_read_frame: fn(*mut raw::AVFormatContext, *mut raw::AVPacket) -> c_int;
    avformat_close_input: fn(*mut *mut raw::AVFormatContext);
    avformat_network_init: fn() -> c_int;
});

/// The core libraries: libavutil, libavcodec and libswscale.
pub(crate) struct Core {
    pub(crate) util: AvUtil,
    pub(crate) codec: AvCodec,
    pub(crate) scale: SwScale,
    /// `avcodec_get_supported_config` (FFmpeg 7.1+), used to list encoder pixel formats.
    pub(crate) get_supported_config: Option<
        unsafe extern "C" fn(
            *const raw::AVCodecContext,
            *const raw::AVCodec,
            c_int,
            c_uint,
            *mut *const c_void,
            *mut c_int,
        ) -> c_int,
    >,
}

static CORE: OnceLock<Result<Core, String>> = OnceLock::new();
#[cfg(feature = "codec-ffmpeg-format")]
static FORMAT: OnceLock<Result<AvFormat, String>> = OnceLock::new();

/// The core FFmpeg libraries, loading them on first use.
pub(crate) fn core() -> Result<&'static Core, String> {
    CORE.get_or_init(|| {
        let util = Library::open("avutil", raw::LIBAVUTIL_VERSION_MAJOR)?;
        let codec = Library::open("avcodec", raw::LIBAVCODEC_VERSION_MAJOR)?;
        let scale = Library::open("swscale", raw::LIBSWSCALE_VERSION_MAJOR)?;
        let get_supported_config = codec
            .optional_symbol("avcodec_get_supported_config")
            // SAFETY: signature from libavcodec/avcodec.h (FFmpeg 7.1+).
            .map(|sym| unsafe { std::mem::transmute(sym) });
        let core = Core {
            util: AvUtil::load(&util)?,
            codec: AvCodec::load(&codec)?,
            scale: SwScale::load(&scale)?,
            get_supported_config,
        };
        // The handles are never closed (`Library` has no `Drop`): FFmpeg stays loaded once used.
        Ok(core)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// libavformat (containers and network streams), loading it on first use.
#[cfg(feature = "codec-ffmpeg-format")]
pub(crate) fn format() -> Result<&'static AvFormat, String> {
    FORMAT
        .get_or_init(|| {
            core()?;
            let lib = Library::open("avformat", raw::LIBAVFORMAT_VERSION_MAJOR)?;
            let format = AvFormat::load(&lib)?;
            // SAFETY: loaded above; enables RTSP/HTTP inputs.
            unsafe { (format.avformat_network_init)() };
            Ok(format)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The core libraries after a successful [`core`] call. FFmpeg objects only exist once it
/// succeeded, so their methods use this.
pub(crate) fn loaded() -> &'static Core {
    core().expect("FFmpeg used before it was loaded")
}
