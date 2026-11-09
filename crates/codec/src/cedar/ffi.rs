//! The parts of Allwinner CedarC's encoder API (`libvencoder`, `libMemAdapter`) the encoder
//! uses, opened with `dlopen` on first use: nothing links them, and Styx ships none of them.
//!
//! The vendor's structs are declared by the offsets of the fields used, on aarch64, from the
//! `libcedarc-dev` 2.0.0 package of a Radxa Cubie A7S image; the rest is left as filler and
//! stays zero or as the library filled it. Written, untested on board.

use std::ffi::{CStr, c_int, c_void};
use std::sync::OnceLock;

pub type Encoder = *mut c_void;

/// `VENC_CODEC_H264`.
pub const CODEC_H264: c_int = 0;
/// `VENC_PIXEL_YUV420SP` (NV12).
pub const PIXEL_YUV420SP: c_int = 0;
/// `VENC_H264ProfileBaseline`.
pub const PROFILE_BASELINE: c_int = 66;
/// `VENC_FRAME_CODING`.
pub const FRAME_CODING: c_int = 0;
/// `AW_CBR`.
pub const RC_CBR: c_int = 0;
/// `VENC_IndexParamIfilter`.
pub const PARAM_IFILTER: c_int = 3;
/// `VENC_IndexParamForceKeyFrame` (`int`, write only).
pub const PARAM_FORCE_KEY_FRAME: c_int = 6;
/// `VENC_IndexParamSetPSkip`.
pub const PARAM_PSKIP: c_int = 18;
/// `VENC_IndexParamH264Param`.
pub const PARAM_H264: c_int = 256;
/// `VENC_IndexParamH264SPSPPS`.
pub const PARAM_H264_HEADERS: c_int = 257;
/// `VENC_BUFFERFLAG_KEYFRAME`.
pub const FLAG_KEYFRAME: u32 = 1;

/// `VencBaseConfig`.
#[repr(C)]
pub struct BaseConfig {
    pub _before: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub dst_width: u32,
    pub dst_height: u32,
    pub stride: u32,
    pub input_format: c_int,
    pub memops: *mut MemOps,
    pub _after: [u64; 5],
}

/// `VencH264Param`.
#[repr(C, align(8))]
pub struct H264Param {
    pub profile: c_int,
    pub level: c_int,
    pub cabac: c_int,
    pub max_qp: c_int,
    pub min_qp: c_int,
    pub max_p_qp: c_int,
    pub min_p_qp: c_int,
    pub init_qp: c_int,
    pub _qp_more: c_int,
    pub framerate: c_int,
    pub src_framerate: c_int,
    pub bitrate: c_int,
    pub max_key_interval: c_int,
    pub coding_mode: c_int,
    pub _before_rc: [u8; 32],
    pub rc_mode: c_int,
    pub _after: [u8; 268],
}

/// `VencAllocateBufferParam`.
#[repr(C)]
pub struct AllocParam {
    pub count: u32,
    pub size_y: u32,
    pub size_c: u32,
}

/// `VencHeaderData`.
#[repr(C)]
pub struct HeaderData {
    pub data: *mut u8,
    pub len: c_int,
}

/// `VencInputBuffer`.
#[repr(C, align(8))]
pub struct InputBuffer {
    pub _before: u64,
    pub pts: i64,
    pub flag: u32,
    pub _before_addr: [u8; 20],
    pub y: *mut u8,
    pub c: *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub align: c_int,
    pub enable_crop: c_int,
    pub _after: [u8; 304],
}

/// `VencOutputBuffer`.
#[repr(C)]
pub struct OutputBuffer {
    pub _before: u64,
    pub pts: i64,
    pub flag: u32,
    pub size0: u32,
    pub size1: u32,
    pub data0: *mut u8,
    pub data1: *mut u8,
    pub _between: [u8; 24],
    pub size2: u32,
    pub data2: *mut u8,
}

/// `struct ScMemOpsS`: the two functions used, the first and third of its table.
#[repr(C)]
pub struct MemOps {
    pub open: Option<unsafe extern "C" fn() -> c_int>,
    pub _open2: *mut c_void,
    pub close: Option<unsafe extern "C" fn()>,
}

macro_rules! cedar_functions {
    ($( $name:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )*) => {
        /// The CedarC functions, resolved once.
        #[allow(non_snake_case)]
        pub struct Cedar {
            $( pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl Cedar {
            fn resolve(handles: &[*mut c_void]) -> Result<Self, String> {
                Ok(Self {
                    $( $name: {
                        let name = concat!(stringify!($name), "\0");
                        // SAFETY: handles from dlopen and a NUL-terminated name.
                        let sym = handles.iter().find_map(|&handle| {
                            let sym = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
                            (!sym.is_null()).then_some(sym)
                        });
                        let Some(sym) = sym else {
                            return Err(format!("CedarC symbol {} not found", stringify!($name)));
                        };
                        // SAFETY: the symbol has this signature in CedarC's headers.
                        unsafe {
                            std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(sym)
                        }
                    }, )*
                })
            }
        }
    };
}

cedar_functions! {
    MemAdapterGetOpsS: fn() -> *mut MemOps;
    VideoEncCreate: fn(c_int) -> Encoder;
    VideoEncDestroy: fn(Encoder);
    VideoEncInit: fn(Encoder, *mut BaseConfig) -> c_int;
    VideoEncSetParameter: fn(Encoder, c_int, *mut c_void) -> c_int;
    VideoEncGetParameter: fn(Encoder, c_int, *mut c_void) -> c_int;
    AllocInputBuffer: fn(Encoder, *mut AllocParam) -> c_int;
    GetOneAllocInputBuffer: fn(Encoder, *mut InputBuffer) -> c_int;
    FlushCacheAllocInputBuffer: fn(Encoder, *mut InputBuffer) -> c_int;
    ReturnOneAllocInputBuffer: fn(Encoder, *mut InputBuffer) -> c_int;
    ReleaseAllocInputBuffer: fn(Encoder) -> c_int;
    AddOneInputBuffer: fn(Encoder, *mut InputBuffer) -> c_int;
    VideoEncodeOneFrame: fn(Encoder) -> c_int;
    AlreadyUsedInputBuffer: fn(Encoder, *mut InputBuffer) -> c_int;
    ValidBitstreamFrameNum: fn(Encoder) -> c_int;
    GetOneBitstreamFrame: fn(Encoder, *mut OutputBuffer) -> c_int;
    FreeOneBitStreamFrame: fn(Encoder, *mut OutputBuffer) -> c_int;
}

const LIBRARIES: [&CStr; 2] = [c"libvencoder.so", c"libMemAdapter.so"];

/// CedarC, loaded on first use; the error says why it is not here.
pub fn cedar() -> Result<&'static Cedar, String> {
    static CEDAR: OnceLock<Result<Cedar, String>> = OnceLock::new();
    CEDAR
        .get_or_init(|| {
            let mut handles = [std::ptr::null_mut(); 2];
            for (handle, name) in handles.iter_mut().zip(LIBRARIES) {
                // SAFETY: dlopen with a NUL-terminated name; the handle is never closed.
                *handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
                if handle.is_null() {
                    return Err(format!("{} not found", name.to_string_lossy()));
                }
            }
            Cedar::resolve(&handles)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Offsets from compiling the vendor's headers for aarch64.
    #[test]
    fn structs_have_the_headers_layout() {
        assert_eq!(size_of::<BaseConfig>(), 80);
        assert_eq!(offset_of!(BaseConfig, input_width), 4);
        assert_eq!(offset_of!(BaseConfig, input_format), 24);
        assert_eq!(offset_of!(BaseConfig, memops), 32);

        assert_eq!(size_of::<H264Param>(), 360);
        assert_eq!(offset_of!(H264Param, cabac), 8);
        assert_eq!(offset_of!(H264Param, max_qp), 12);
        assert_eq!(offset_of!(H264Param, init_qp), 28);
        assert_eq!(offset_of!(H264Param, framerate), 36);
        assert_eq!(offset_of!(H264Param, coding_mode), 52);
        assert_eq!(offset_of!(H264Param, rc_mode), 88);

        assert_eq!(size_of::<AllocParam>(), 12);
        assert_eq!(size_of::<HeaderData>(), 16);

        assert_eq!(size_of::<InputBuffer>(), 376);
        assert_eq!(offset_of!(InputBuffer, pts), 8);
        assert_eq!(offset_of!(InputBuffer, flag), 16);
        assert_eq!(offset_of!(InputBuffer, y), 40);
        assert_eq!(offset_of!(InputBuffer, width), 56);
        assert_eq!(offset_of!(InputBuffer, enable_crop), 68);

        assert_eq!(size_of::<OutputBuffer>(), 88);
        assert_eq!(offset_of!(OutputBuffer, flag), 16);
        assert_eq!(offset_of!(OutputBuffer, size0), 20);
        assert_eq!(offset_of!(OutputBuffer, data0), 32);
        assert_eq!(offset_of!(OutputBuffer, size2), 72);
        assert_eq!(offset_of!(OutputBuffer, data2), 80);
    }
}
