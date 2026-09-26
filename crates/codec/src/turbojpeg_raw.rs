//! Thin owned wrapper over the TurboJPEG 3 decompression API.

use std::ffi::{CStr, c_int};

use turbojpeg::raw;

use crate::CodecError;
use crate::mjpeg_turbojpeg_luma::LumaCrop;

pub(crate) struct JpegHeader {
    pub width: u32,
    pub height: u32,
    pub mcu_width: u32,
}

/// Owned `tj3` decompression handle. Creating one costs well under a microsecond, so each call
/// uses its own handle and the decoder stays lock-free across threads.
pub(crate) struct TjDecompressor(raw::tjhandle);

impl TjDecompressor {
    pub(crate) fn new() -> Result<Self, CodecError> {
        // SAFETY: tj3Init has no preconditions; a null result is reported as an error.
        let handle = unsafe { raw::tj3Init(raw::TJINIT_TJINIT_DECOMPRESS as c_int) };
        if handle.is_null() {
            return Err(CodecError::Codec("tj3Init failed".into()));
        }
        Ok(Self(handle))
    }

    /// Map a tj3 return code to a result. Warnings (e.g. "extraneous bytes before marker",
    /// common in UVC camera MJPEG) still produce a complete image and are treated as success;
    /// the `turbojpeg` crate reports them as errors and drops such frames.
    fn check(&self, ret: c_int) -> Result<(), CodecError> {
        if ret == 0 {
            return Ok(());
        }
        // SAFETY: the handle is valid.
        if unsafe { raw::tj3GetErrorCode(self.0) } == raw::TJERR_TJERR_WARNING as c_int {
            return Ok(());
        }
        // SAFETY: the handle is valid; tj3GetErrorStr returns a NUL-terminated static or
        // handle-owned string.
        let msg = unsafe { CStr::from_ptr(raw::tj3GetErrorStr(self.0)) };
        Err(CodecError::Codec(msg.to_string_lossy().into_owned()))
    }

    pub(crate) fn set(&self, param: raw::TJPARAM, value: c_int) -> Result<(), CodecError> {
        // SAFETY: valid handle; libjpeg-turbo validates the parameter and value.
        self.check(unsafe { raw::tj3Set(self.0, param as c_int, value) })
    }

    fn get(&self, param: raw::TJPARAM) -> c_int {
        // SAFETY: valid handle; unknown parameters return -1.
        unsafe { raw::tj3Get(self.0, param as c_int) }
    }

    pub(crate) fn read_header(&self, jpeg: &[u8]) -> Result<JpegHeader, CodecError> {
        // SAFETY: `jpeg` is a live slice for the duration of the call.
        self.check(unsafe { raw::tj3DecompressHeader(self.0, jpeg.as_ptr(), jpeg.len() as _) })?;
        let width = self.get(raw::TJPARAM_TJPARAM_JPEGWIDTH);
        let height = self.get(raw::TJPARAM_TJPARAM_JPEGHEIGHT);
        if width <= 0 || height <= 0 {
            return Err(CodecError::Codec("invalid jpeg dimensions".into()));
        }
        // iMCU width: 16 for horizontally subsampled chroma (4:2:0, 4:2:2, 4:1:1 uses 32), else 8.
        let mcu_width = match self.get(raw::TJPARAM_TJPARAM_SUBSAMP) {
            s if s == raw::TJSAMP_TJSAMP_420 as c_int || s == raw::TJSAMP_TJSAMP_422 as c_int => 16,
            s if s == raw::TJSAMP_TJSAMP_411 as c_int => 32,
            _ => 8,
        };
        Ok(JpegHeader {
            width: width as u32,
            height: height as u32,
            mcu_width,
        })
    }

    pub(crate) fn set_scaling(&self, denom: c_int) -> Result<(), CodecError> {
        let factor = raw::tjscalingfactor { num: 1, denom };
        // SAFETY: valid handle; unsupported factors are rejected with an error.
        self.check(unsafe { raw::tj3SetScalingFactor(self.0, factor) })
    }

    pub(crate) fn set_crop(&self, crop: Option<LumaCrop>) -> Result<(), CodecError> {
        let region = crop.map_or(
            raw::tjregion {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            |c| raw::tjregion {
                x: c.x as c_int,
                y: c.y as c_int,
                w: c.width as c_int,
                h: c.height as c_int,
            },
        );
        // SAFETY: valid handle; the region was clamped to the scaled image and iMCU-aligned.
        self.check(unsafe { raw::tj3SetCroppingRegion(self.0, region) })
    }

    /// Decompress into `dst` (`stride * height` bytes for the planned output) as `pixel_format`.
    pub(crate) fn decompress(
        &self,
        jpeg: &[u8],
        dst: &mut [u8],
        stride: usize,
        pixel_format: raw::TJPF,
    ) -> Result<(), CodecError> {
        // SAFETY: callers size `dst` for the planned output at `stride`, `stride` is at least the
        // output row size, and both slices outlive the call.
        self.check(unsafe {
            raw::tj3Decompress8(
                self.0,
                jpeg.as_ptr(),
                jpeg.len() as _,
                dst.as_mut_ptr(),
                stride as c_int,
                pixel_format as c_int,
            )
        })
    }
}

impl Drop for TjDecompressor {
    fn drop(&mut self) {
        // SAFETY: the handle came from tj3Init and is destroyed exactly once.
        unsafe { raw::tj3Destroy(self.0) }
    }
}
