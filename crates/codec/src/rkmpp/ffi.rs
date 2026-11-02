//! The parts of Rockchip MPP's C API (`librockchip_mpp`, Apache-2.0 headers `rk_mpi.h`,
//! `mpp_buffer.h`, `mpp_frame.h`, `mpp_packet.h`, `mpp_meta.h`, `rk_mpi_cmd.h`,
//! `rk_venc_cfg.h`) the encoder uses, opened with `dlopen` on first use: nothing links MPP.
//!
//! Constants are from the headers on the `develop` branch; confirm them against the BSP's
//! headers on a board (written, untested on board).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

pub type MppRet = c_int;
pub type Handle = *mut c_void;

/// `MPP_CTX_ENC`.
pub const CTX_ENC: c_int = 1;
/// `MPP_VIDEO_CodingAVC`.
pub const CODING_AVC: c_int = 7;
/// `MPP_VIDEO_CodingMJPEG`.
pub const CODING_MJPEG: c_int = 8;
/// `MPP_VIDEO_CodingHEVC`.
pub const CODING_HEVC: c_int = 0x0100_0004;
/// `MPP_FMT_YUV420SP` (NV12).
pub const FMT_YUV420SP: c_int = 0;
/// `MPP_FMT_YUV422_YUYV`.
pub const FMT_YUV422_YUYV: c_int = 8;
/// `MPP_BUFFER_TYPE_EXT_DMA`: a dma-buf from outside MPP.
pub const BUFFER_TYPE_EXT_DMA: c_int = 2;
/// `MPP_SET_OUTPUT_TIMEOUT` (parameter: `RK_S64` milliseconds, -1 blocks).
pub const SET_OUTPUT_TIMEOUT: c_int = 0x0020_0007;
/// `MPP_ENC_SET_CFG`.
pub const ENC_SET_CFG: c_int = 0x0032_0001;
/// `MPP_ENC_SET_IDR_FRAME`.
pub const ENC_SET_IDR_FRAME: c_int = 0x0032_0009;
/// `MPP_ENC_SET_HEADER_MODE` (`MPP_ENC_CFG_MISC + 1`).
pub const ENC_SET_HEADER_MODE: c_int = 0x0032_8001;
/// `MPP_ENC_HEADER_MODE_EACH_IDR`: SPS/PPS before every IDR frame.
pub const HEADER_MODE_EACH_IDR: c_int = 1;
/// `MPP_ENC_RC_MODE_VBR`, `_CBR`, `_FIXQP`.
pub const RC_MODE_VBR: i32 = 0;
pub const RC_MODE_CBR: i32 = 1;
pub const RC_MODE_FIXQP: i32 = 2;

/// `FOURCC_META(a, b, c, d)`.
pub const fn fourcc_meta(k: &[u8; 4]) -> u32 {
    (k[0] as u32) << 24 | (k[1] as u32) << 16 | (k[2] as u32) << 8 | k[3] as u32
}

/// `KEY_OUTPUT_INTRA`: the packet is an intra frame.
pub const KEY_OUTPUT_INTRA: u32 = fourcc_meta(b"oidr");

/// `MppBufferInfo`.
#[repr(C)]
pub struct BufferInfo {
    pub kind: c_int,
    pub size: usize,
    pub ptr: *mut c_void,
    pub hnd: *mut c_void,
    pub fd: c_int,
    pub index: c_int,
}

type Fp = Option<unsafe extern "C" fn()>;

/// `MppApi` (`struct MppApi_t`): the context's function table.
#[repr(C)]
pub struct MppApi {
    pub size: u32,
    pub version: u32,
    pub decode: Fp,
    pub decode_put_packet: Fp,
    pub decode_get_frame: Fp,
    pub encode: Fp,
    pub encode_put_frame: Option<unsafe extern "C" fn(Handle, Handle) -> MppRet>,
    pub encode_get_packet: Option<unsafe extern "C" fn(Handle, *mut Handle) -> MppRet>,
    pub isp: Fp,
    pub isp_put_frame: Fp,
    pub isp_get_frame: Fp,
    pub poll: Fp,
    pub dequeue: Fp,
    pub enqueue: Fp,
    pub reset: Option<unsafe extern "C" fn(Handle) -> MppRet>,
    pub control: Option<unsafe extern "C" fn(Handle, c_int, *mut c_void) -> MppRet>,
    pub reserv: [u32; 16],
}

macro_rules! mpp_functions {
    ($( $name:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )*) => {
        /// The MPP functions, resolved once.
        #[allow(non_snake_case)]
        pub struct Mpp {
            $( pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl Mpp {
            fn resolve(handle: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $( $name: {
                        let name = concat!(stringify!($name), "\0");
                        // SAFETY: a handle from dlopen and a NUL-terminated name.
                        let sym = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
                        if sym.is_null() {
                            return Err(format!("MPP symbol {} not found", stringify!($name)));
                        }
                        // SAFETY: the symbol has this signature in MPP's headers.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(sym) }
                    }, )*
                })
            }
        }
    };
}

mpp_functions! {
    mpp_create: fn(*mut Handle, *mut *mut MppApi) -> MppRet;
    mpp_init: fn(Handle, c_int, c_int) -> MppRet;
    mpp_destroy: fn(Handle) -> MppRet;
    mpp_enc_cfg_init: fn(*mut Handle) -> MppRet;
    mpp_enc_cfg_deinit: fn(Handle) -> MppRet;
    mpp_enc_cfg_set_s32: fn(Handle, *const c_char, i32) -> MppRet;
    mpp_buffer_import_with_tag: fn(Handle, *mut BufferInfo, *mut Handle, *const c_char, *const c_char) -> MppRet;
    mpp_buffer_put_with_caller: fn(Handle, *const c_char) -> MppRet;
    mpp_frame_init: fn(*mut Handle) -> MppRet;
    mpp_frame_deinit: fn(*mut Handle) -> MppRet;
    mpp_frame_set_width: fn(Handle, u32);
    mpp_frame_set_height: fn(Handle, u32);
    mpp_frame_set_hor_stride: fn(Handle, u32);
    mpp_frame_set_ver_stride: fn(Handle, u32);
    mpp_frame_set_fmt: fn(Handle, c_int);
    mpp_frame_set_buffer: fn(Handle, Handle);
    mpp_frame_set_pts: fn(Handle, i64);
    mpp_packet_deinit: fn(*mut Handle) -> MppRet;
    mpp_packet_get_pos: fn(Handle) -> *mut c_void;
    mpp_packet_get_length: fn(Handle) -> usize;
    mpp_packet_get_meta: fn(Handle) -> Handle;
    mpp_meta_get_s32: fn(Handle, u32, *mut i32) -> MppRet;
}

/// The tag MPP logs buffers under.
pub const TAG: &CStr = c"styx";

/// MPP, loaded on first use; the error says why it is not here.
pub fn mpp() -> Result<&'static Mpp, String> {
    static MPP: OnceLock<Result<Mpp, String>> = OnceLock::new();
    MPP.get_or_init(|| {
        let mut errors = Vec::new();
        for name in [
            c"librockchip_mpp.so.1",
            c"librockchip_mpp.so.0",
            c"librockchip_mpp.so",
        ] {
            // SAFETY: dlopen with a NUL-terminated name; the handle is never closed.
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                return Mpp::resolve(handle);
            }
            errors.push(name.to_string_lossy().into_owned());
        }
        Err(format!("librockchip_mpp not found ({})", errors.join(", ")))
    })
    .as_ref()
    .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_keys_are_big_endian_fourccs() {
        assert_eq!(KEY_OUTPUT_INTRA, 0x6f69_6472);
    }

    #[test]
    fn the_api_table_has_the_headers_layout() {
        // Two u32s, fourteen function pointers, sixteen u32s.
        let p = size_of::<usize>();
        assert_eq!(size_of::<MppApi>(), (8 + 14 * p + 64).next_multiple_of(p));
        assert_eq!(std::mem::offset_of!(MppApi, control), 8 + 13 * p);
    }
}
