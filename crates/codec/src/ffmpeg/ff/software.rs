pub mod scaling {
    pub mod flag {
        /// swscale algorithm flags.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Flags(pub std::ffi::c_int);

        impl Flags {
            // SWS_FAST_BILINEAR and SWS_BILINEAR (libswscale/swscale.h); stable values, while
            // their binding names change between FFmpeg versions.
            pub const FAST_BILINEAR: Self = Self(1);
            pub const BILINEAR: Self = Self(2);
        }
    }

    pub mod context {
        use super::super::super::*;
        use super::flag::Flags;
        use crate::ffmpeg::ff::frame::Video as Frame;

        /// A swscale context converting one fixed format and size to another.
        pub struct Context {
            ptr: *mut raw::SwsContext,
            src: (Pixel, u32, u32),
            dst: (Pixel, u32, u32),
        }

        // SAFETY: used by one thread at a time (callers hold it in a mutex).
        unsafe impl Send for Context {}

        impl Context {
            #[allow(clippy::too_many_arguments)]
            pub fn get(
                src_format: Pixel,
                src_width: u32,
                src_height: u32,
                dst_format: Pixel,
                dst_width: u32,
                dst_height: u32,
                flags: Flags,
            ) -> Result<Self, Error> {
                loader::core().map_err(Error::Unavailable)?;
                // SAFETY: plain context creation.
                let ptr = unsafe {
                    (loader::loaded().scale.sws_getContext)(
                        src_width as c_int,
                        src_height as c_int,
                        src_format.0,
                        dst_width as c_int,
                        dst_height as c_int,
                        dst_format.0,
                        flags.0,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null(),
                    )
                };
                if ptr.is_null() {
                    return Err(Error::InvalidData);
                }
                Ok(Self {
                    ptr,
                    src: (src_format, src_width, src_height),
                    dst: (dst_format, dst_width, dst_height),
                })
            }

            /// Convert `input` into `output`, allocating `output` if it has no buffers.
            pub fn run(&mut self, input: &Frame, output: &mut Frame) -> Result<(), Error> {
                if (input.format(), input.width(), input.height()) != self.src {
                    return Err(Error::InvalidData);
                }
                if output.is_empty() {
                    // SAFETY: fresh buffers for the destination format and size.
                    unsafe { output.alloc(self.dst.0, self.dst.1, self.dst.2) };
                }
                // SAFETY: frames match the formats/sizes the context was created for.
                unsafe {
                    let src = &*input.as_ptr();
                    let dst = &*output.as_mut_ptr();
                    (loader::loaded().scale.sws_scale)(
                        self.ptr,
                        src.data.as_ptr() as *const *const u8,
                        src.linesize.as_ptr(),
                        0,
                        self.src.2 as c_int,
                        dst.data.as_ptr(),
                        dst.linesize.as_ptr(),
                    );
                }
                Ok(())
            }
        }

        impl Drop for Context {
            fn drop(&mut self) {
                // SAFETY: owned context from sws_getContext.
                unsafe { (loader::loaded().scale.sws_freeContext)(self.ptr) };
            }
        }
    }
}
