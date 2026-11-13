//! libturbojpeg (2.x API), used when build.rs finds the library. Without it, `TurboDecoder` is
//! an uninhabited type and `new` says why.

use styx_codec_api::CodecError;

#[cfg(styx_turbojpeg)]
mod ffi {
    use std::os::raw::{c_char, c_int, c_ulong, c_void};

    pub type Handle = *mut c_void;

    /// `TJPF_RGB`: packed RGB, three bytes a pixel.
    pub const TJPF_RGB: c_int = 0;

    // Contract (libturbojpeg's documented API): each handle comes from `tjInitDecompress` and is
    // released once by `tjDestroy`; buffers are read or written only for the sizes passed; the
    // decode calls return non-zero on failure.
    unsafe extern "C" {
        pub fn tjInitDecompress() -> Handle;
        pub fn tjDestroy(handle: Handle) -> c_int;
        pub fn tjGetErrorStr2(handle: Handle) -> *mut c_char;
        pub fn tjDecompressHeader3(
            handle: Handle,
            jpeg: *const u8,
            size: c_ulong,
            width: *mut c_int,
            height: *mut c_int,
            subsamp: *mut c_int,
            colorspace: *mut c_int,
        ) -> c_int;
        pub fn tjDecompress2(
            handle: Handle,
            jpeg: *const u8,
            size: c_ulong,
            dst: *mut u8,
            width: c_int,
            pitch: c_int,
            height: c_int,
            pixel_format: c_int,
            flags: c_int,
        ) -> c_int;
    }
}

/// A libturbojpeg decompressor. `new` is the only way to get one.
#[cfg(styx_turbojpeg)]
pub struct TurboDecoder {
    handle: ffi::Handle,
}

#[cfg(styx_turbojpeg)]
impl TurboDecoder {
    pub fn new() -> Result<Self, CodecError> {
        // SAFETY: takes no arguments; a null return is checked below.
        let handle = unsafe { ffi::tjInitDecompress() };
        if handle.is_null() {
            return Err(CodecError::Unavailable("tjInitDecompress failed".into()));
        }
        Ok(Self { handle })
    }

    fn error(&self) -> String {
        // SAFETY: `handle` is live (owned by self); the returned string is copied out at once.
        unsafe {
            let p = ffi::tjGetErrorStr2(self.handle);
            if p.is_null() {
                return "unknown error".into();
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    /// Width and height from the JPEG header.
    pub fn header(&mut self, jpeg: &[u8]) -> Result<(usize, usize), CodecError> {
        let (mut w, mut h, mut subsamp, mut cs) = (0i32, 0i32, 0i32, 0i32);
        // SAFETY: `jpeg` is valid for `jpeg.len()` bytes; the four out-pointers are to locals.
        let rc = unsafe {
            ffi::tjDecompressHeader3(
                self.handle,
                jpeg.as_ptr(),
                jpeg.len() as std::os::raw::c_ulong,
                &mut w,
                &mut h,
                &mut subsamp,
                &mut cs,
            )
        };
        if rc != 0 {
            return Err(CodecError::Rejected(format!(
                "turbojpeg header: {}",
                self.error()
            )));
        }
        Ok((w.max(0) as usize, h.max(0) as usize))
    }

    /// Decompresses to packed RGB24 rows (pitch `width * 3`) into `dst`, which must hold
    /// `width * 3 * height` bytes.
    pub fn decompress(
        &mut self,
        jpeg: &[u8],
        dst: &mut [u8],
        width: usize,
        height: usize,
    ) -> Result<(), CodecError> {
        if dst.len() < width * 3 * height {
            return Err(CodecError::Failed("turbojpeg destination too short".into()));
        }
        // SAFETY: `jpeg` is valid for its length; `dst` holds `width * 3 * height` bytes, which is
        // exactly the pitch and row count given, so libturbojpeg writes only inside it.
        let rc = unsafe {
            ffi::tjDecompress2(
                self.handle,
                jpeg.as_ptr(),
                jpeg.len() as std::os::raw::c_ulong,
                dst.as_mut_ptr(),
                width as i32,
                (width * 3) as i32,
                height as i32,
                ffi::TJPF_RGB,
                0,
            )
        };
        if rc != 0 {
            return Err(CodecError::Rejected(format!(
                "turbojpeg decode: {}",
                self.error()
            )));
        }
        Ok(())
    }
}

#[cfg(styx_turbojpeg)]
impl Drop for TurboDecoder {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `tjInitDecompress`, is destroyed once here, and is not used
        // after `drop`.
        unsafe {
            ffi::tjDestroy(self.handle);
        }
    }
}

// SAFETY: a libturbojpeg handle is not tied to a thread; every use goes through `&mut self`, so
// the handle is never used from two threads at once.
#[cfg(styx_turbojpeg)]
unsafe impl Send for TurboDecoder {}

#[cfg(styx_turbojpeg)]
impl std::fmt::Debug for TurboDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurboDecoder").finish_non_exhaustive()
    }
}

/// Without libturbojpeg there is no decompressor to hold.
#[cfg(not(styx_turbojpeg))]
#[derive(Debug)]
pub enum TurboDecoder {}

#[cfg(not(styx_turbojpeg))]
impl TurboDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Err(CodecError::Unavailable(if cfg!(feature = "turbojpeg") {
            "libturbojpeg was not found at build time".into()
        } else {
            "built without the turbojpeg feature".into()
        }))
    }

    pub fn header(&mut self, _jpeg: &[u8]) -> Result<(usize, usize), CodecError> {
        match *self {}
    }

    pub fn decompress(
        &mut self,
        _jpeg: &[u8],
        _dst: &mut [u8],
        _width: usize,
        _height: usize,
    ) -> Result<(), CodecError> {
        match *self {}
    }
}
