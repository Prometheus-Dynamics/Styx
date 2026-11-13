//! Runtime loading of FFmpeg's libraries.
//!
//! Nothing in Styx links FFmpeg. The libraries are opened with `dlopen` the first time an FFmpeg
//! codec is built, and the functions Styx calls are looked up by name. Libraries are opened by the
//! major version the field offsets in [`crate::sys`] were written against (libavutil 60,
//! libavcodec 62: FFmpeg 8.x). A machine without them reports "not present", and the registry
//! skips the FFmpeg candidates with that reason.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// libavutil's major version (FFmpeg 8.x).
pub(crate) const AVUTIL_MAJOR: c_int = 60;
/// libavcodec's major version (FFmpeg 8.x).
pub(crate) const AVCODEC_MAJOR: c_int = 62;

const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;

// SAFETY (the block): these are the C library's dynamic loader entry points, with the signatures
// dlfcn.h declares. glibc (2.34 and later) provides them in libc itself, so nothing is linked.
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

/// A loaded shared library. Never closed: once FFmpeg is in the process it stays.
struct Library(*mut c_void);

// SAFETY: a dlopen handle is process-global and used only with dlsym.
unsafe impl Send for Library {}
// SAFETY: as above; dlsym is thread-safe.
unsafe impl Sync for Library {}

impl Library {
    /// Opens `lib{stem}.so.{major}` from each directory in `dirs`, or from the system search path
    /// when `dirs` is empty.
    fn open(dirs: &[PathBuf], stem: &str, major: c_int) -> Result<Self, String> {
        let file = format!("lib{stem}.so.{major}");
        let candidates: Vec<String> = if dirs.is_empty() {
            vec![file.clone()]
        } else {
            dirs.iter()
                .map(|dir| dir.join(&file).to_string_lossy().into_owned())
                .collect()
        };
        let mut errors = Vec::new();
        for name in &candidates {
            let cname = CString::new(name.as_str()).map_err(|e| e.to_string())?;
            // SAFETY: `cname` is a valid NUL-terminated string that outlives the call.
            let handle = unsafe { dlopen(cname.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
            if !handle.is_null() {
                return Ok(Self(handle));
            }
            errors.push(last_dl_error());
        }
        Err(format!(
            "FFmpeg not present: lib{stem}.so.{major} could not be opened ({})",
            errors.join("; ")
        ))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: the handle came from a successful dlopen and is never closed; `cname` is valid.
        let sym = unsafe { dlsym(self.0, cname.as_ptr()) };
        if sym.is_null() {
            Err(format!(
                "FFmpeg not present: symbol {name} missing ({})",
                last_dl_error()
            ))
        } else {
            Ok(sym)
        }
    }
}

fn last_dl_error() -> String {
    // SAFETY: dlerror returns a thread-local message, or null when there is none.
    let message = unsafe { dlerror() };
    if message.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: a non-null dlerror result is a NUL-terminated string owned by libc until the
        // next dl call; it is copied out here.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Declares a table of FFmpeg functions resolved by name from one library.
macro_rules! function_table {
    ($table:ident from $lib:ident { $( $name:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )* }) => {
        #[allow(non_snake_case)]
        pub(crate) struct $table {
            $( pub(crate) $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl $table {
            fn load(lib: &Library) -> Result<Self, String> {
                Ok(Self {
                    $(
                        $name: {
                            let sym = lib.symbol(stringify!($name))?;
                            // SAFETY: the symbol has this C signature in the FFmpeg headers the
                            // offsets in `sys` come from. Function pointers and data pointers have
                            // the same size on every target Styx builds for.
                            unsafe {
                                std::mem::transmute::<
                                    *mut c_void,
                                    unsafe extern "C" fn($($arg),*) $(-> $ret)?,
                                >(sym)
                            }
                        },
                    )*
                })
            }
        }
    };
}

function_table!(AvUtil from util {
    av_frame_alloc: fn() -> *mut c_void;
    av_frame_free: fn(*mut *mut c_void);
    av_frame_get_buffer: fn(*mut c_void, c_int) -> c_int;
    av_frame_make_writable: fn(*mut c_void) -> c_int;
    av_frame_unref: fn(*mut c_void);
    av_opt_set: fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int;
    av_strerror: fn(c_int, *mut c_char, usize) -> c_int;
});

function_table!(AvCodec from codec {
    av_packet_alloc: fn() -> *mut c_void;
    av_packet_free: fn(*mut *mut c_void);
    av_packet_unref: fn(*mut c_void);
    av_new_packet: fn(*mut c_void, c_int) -> c_int;
    avcodec_find_decoder_by_name: fn(*const c_char) -> *const c_void;
    avcodec_find_encoder_by_name: fn(*const c_char) -> *const c_void;
    avcodec_alloc_context3: fn(*const c_void) -> *mut c_void;
    avcodec_free_context: fn(*mut *mut c_void);
    avcodec_open2: fn(*mut c_void, *const c_void, *mut *mut c_void) -> c_int;
    avcodec_send_packet: fn(*mut c_void, *const c_void) -> c_int;
    avcodec_receive_frame: fn(*mut c_void, *mut c_void) -> c_int;
    avcodec_send_frame: fn(*mut c_void, *const c_void) -> c_int;
    avcodec_receive_packet: fn(*mut c_void, *mut c_void) -> c_int;
});

/// The resolved functions of libavutil and libavcodec.
pub(crate) struct Api {
    pub(crate) util: AvUtil,
    pub(crate) codec: AvCodec,
}

impl Api {
    fn load(dirs: &[PathBuf]) -> Result<Self, String> {
        let util = Library::open(dirs, "avutil", AVUTIL_MAJOR)?;
        let codec = Library::open(dirs, "avcodec", AVCODEC_MAJOR)?;
        Ok(Self {
            util: AvUtil::load(&util)?,
            codec: AvCodec::load(&codec)?,
        })
    }
}

/// The libraries from the system search path, opened once per process on first use.
pub(crate) fn system() -> Result<Arc<Api>, String> {
    static API: OnceLock<Result<Arc<Api>, String>> = OnceLock::new();
    API.get_or_init(|| Api::load(&[]).map(Arc::new)).clone()
}

/// The libraries from `dirs` (an explicit search, used by tests and by hosts with a private
/// FFmpeg), or from the system search path when `dirs` is empty.
pub(crate) fn load(dirs: &[PathBuf]) -> Result<Arc<Api>, String> {
    if dirs.is_empty() {
        system()
    } else {
        Api::load(dirs).map(Arc::new)
    }
}

/// Text for an FFmpeg error code.
pub(crate) fn error_text(api: &Api, code: c_int) -> String {
    let mut buffer = [0 as c_char; 256];
    // SAFETY: the buffer is 256 bytes and av_strerror writes at most that many, NUL-terminated.
    unsafe { (api.util.av_strerror)(code, buffer.as_mut_ptr(), buffer.len()) };
    // SAFETY: av_strerror NUL-terminates the buffer on success; on failure the buffer was zeroed
    // by the initialiser above, so it is still NUL-terminated.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
