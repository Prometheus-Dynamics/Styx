//! Runtime loading of FFmpeg's libraries.
//!
//! Nothing in Styx links FFmpeg. The libraries are opened with `dlopen` the first time an FFmpeg
//! codec is built, and every function Styx calls is looked up by name. Only the majors in
//! [`SUPPORTED`] are loaded: the struct fields Styx touches ([`crate::sys`]) are laid out for
//! those. A library of any other major is refused with `FFmpeg not present: unsupported ...`, and
//! the registry skips the FFmpeg candidates with that reason. The version each loaded library
//! reports (`avutil_version()`, `avcodec_version()`) is checked against the soname's major.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The (libavutil major, libavcodec major) pairs Styx has laid out. FFmpeg 8.x: libavutil 60 and
/// libavcodec 62. Nothing else is loaded.
pub const SUPPORTED: &[(u32, u32)] = &[(60, 62)];

const UTIL_MAJORS: &[c_int] = &[60];
const CODEC_MAJORS: &[c_int] = &[62];
/// Majors probed only to name a library Styx refuses, in the refusal message.
const PROBE_RANGE: std::ops::RangeInclusive<c_int> = 54..=66;

const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;

// SAFETY (the block): the C library's dynamic loader entry points, with the signatures dlfcn.h
// declares. glibc (2.34 and later) provides them in libc itself, so nothing is linked.
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *mut c_char;
}

/// A loaded shared library. Never closed once the libraries are in use (the functions Styx holds
/// point into it); [`Library::close`] is for libraries that are refused.
struct Library(*mut c_void);

// SAFETY: a dlopen handle is process-global and used only with dlsym and dlclose.
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
        Err(errors.join("; "))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: the handle came from a successful dlopen; `cname` is valid.
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

    /// Unloads a library that is refused. Not used for libraries Styx keeps.
    fn close(self) {
        // SAFETY: the handle came from dlopen and nothing else holds it.
        unsafe { dlclose(self.0) };
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

/// Opens `lib{stem}` at one of the `supported` majors. When none opens, a library of another
/// major that is present is named, so the refusal says which one it is.
fn open_supported(
    dirs: &[PathBuf],
    stem: &str,
    supported: &[c_int],
) -> Result<(Library, c_int), String> {
    let mut errors = Vec::new();
    for &major in supported {
        match Library::open(dirs, stem, major) {
            Ok(library) => return Ok((library, major)),
            Err(error) => errors.push(error),
        }
    }
    for major in PROBE_RANGE {
        if supported.contains(&major) {
            continue;
        }
        if let Ok(library) = Library::open(dirs, stem, major) {
            library.close();
            return Err(format!(
                "FFmpeg not present: unsupported major {major} of lib{stem} (supported: {supported:?})"
            ));
        }
    }
    Err(format!(
        "FFmpeg not present: lib{stem} (supported majors {supported:?}) could not be opened: {}",
        errors.join("; ")
    ))
}

/// The major of a libavutil/libavcodec version number (`LIBAVCODEC_VERSION_INT` layout: major in
/// bits 16 and up).
pub(crate) fn major_of(version: c_uint) -> u32 {
    version >> 16
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
    avutil_version: fn() -> c_uint;
    av_frame_alloc: fn() -> *mut c_void;
    av_frame_free: fn(*mut *mut c_void);
    av_frame_get_buffer: fn(*mut c_void, c_int) -> c_int;
    av_frame_make_writable: fn(*mut c_void) -> c_int;
    av_frame_move_ref: fn(*mut c_void, *mut c_void);
    av_frame_unref: fn(*mut c_void);
    av_get_pix_fmt: fn(*const c_char) -> c_int;
    av_opt_set: fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int;
    av_strerror: fn(c_int, *mut c_char, usize) -> c_int;
});

function_table!(AvCodec from codec {
    avcodec_version: fn() -> c_uint;
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

/// The resolved functions of libavutil and libavcodec, and the majors they report.
pub(crate) struct Api {
    pub(crate) util: AvUtil,
    pub(crate) codec: AvCodec,
    pub(crate) util_major: u32,
    pub(crate) codec_major: u32,
}

impl Api {
    fn load(dirs: &[PathBuf]) -> Result<Self, String> {
        let (util, util_major) = open_supported(dirs, "avutil", UTIL_MAJORS)?;
        let (codec, codec_major) = open_supported(dirs, "avcodec", CODEC_MAJORS)?;
        let util_table = AvUtil::load(&util)?;
        let codec_table = AvCodec::load(&codec)?;
        // SAFETY: both version functions take no arguments and return an unsigned int.
        let (util_reported, codec_reported) = unsafe {
            (
                major_of((util_table.avutil_version)()),
                major_of((codec_table.avcodec_version)()),
            )
        };
        if util_reported != util_major as u32 || codec_reported != codec_major as u32 {
            return Err(format!(
                "FFmpeg not present: the libraries report libavutil {util_reported} and libavcodec {codec_reported}, not the majors they were opened as ({util_major}, {codec_major})"
            ));
        }
        if !SUPPORTED.contains(&(util_reported, codec_reported)) {
            return Err(format!(
                "FFmpeg not present: unsupported majors libavutil {util_reported}, libavcodec {codec_reported} (supported: {SUPPORTED:?})"
            ));
        }
        // The libraries stay loaded: the function pointers above point into them.
        Ok(Self {
            util: util_table,
            codec: codec_table,
            util_major: util_reported,
            codec_major: codec_reported,
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
