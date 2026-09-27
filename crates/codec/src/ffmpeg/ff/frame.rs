use std::any::Any;
use std::ffi::c_void;
use std::sync::Arc;

use super::*;

/// An owned video frame (`AVFrame`).
pub struct Video {
    ptr: *mut raw::AVFrame,
}

// SAFETY: an AVFrame is owned data; FFmpeg's buffers are reference counted and thread-safe.
unsafe impl Send for Video {}
// SAFETY: shared access only reads.
unsafe impl Sync for Video {}

impl Video {
    pub fn empty() -> Self {
        // SAFETY: plain allocation.
        let ptr = unsafe { (loader::loaded().util.av_frame_alloc)() };
        assert!(!ptr.is_null(), "av_frame_alloc failed");
        Self { ptr }
    }

    /// A frame with buffers for `format` at `width`x`height`.
    pub fn new(format: Pixel, width: u32, height: u32) -> Self {
        let mut frame = Self::empty();
        // SAFETY: fresh frame.
        unsafe { frame.alloc(format, width, height) };
        frame
    }

    /// A frame over memory FFmpeg does not own: `planes` are read in place, and `owner` (which
    /// keeps that memory valid) is released when FFmpeg drops its last reference to the frame,
    /// which may be after an encoder has held it for a while.
    ///
    /// # Safety
    /// Each plane's `(pointer, stride, len)` must describe `len` readable bytes that stay valid
    /// and unchanged while `owner` is alive, and cover the rows the pixel format needs.
    pub unsafe fn borrowing(
        format: Pixel,
        width: u32,
        height: u32,
        planes: &[(*const u8, usize, usize)],
        owner: Arc<dyn Any + Send + Sync>,
    ) -> Option<Self> {
        /// `AV_BUFFER_FLAG_READONLY`: FFmpeg copies before writing.
        const READONLY: c_int = 1;
        unsafe extern "C" fn release(opaque: *mut c_void, _data: *mut u8) {
            // SAFETY: `opaque` is the boxed owner created below, released exactly once.
            drop(unsafe { Box::from_raw(opaque.cast::<Arc<dyn Any + Send + Sync>>()) });
        }
        let mut frame = Self::empty();
        frame.set_format(format);
        frame.set_width(width);
        frame.set_height(height);
        for (index, &(data, stride, len)) in planes.iter().enumerate().take(8) {
            let opaque = Box::into_raw(Box::new(owner.clone())).cast::<c_void>();
            // SAFETY: `data` is valid for `len` bytes while `owner` lives; FFmpeg calls
            // `release` once when the buffer's last reference goes.
            let buf = unsafe {
                (loader::loaded().util.av_buffer_create)(
                    data.cast_mut(),
                    len,
                    Some(release),
                    opaque,
                    READONLY,
                )
            };
            if buf.is_null() {
                // SAFETY: FFmpeg did not take `opaque`.
                unsafe { release(opaque, std::ptr::null_mut()) };
                return None;
            }
            // SAFETY: valid owned frame; the frame now owns `buf` and frees it on drop.
            unsafe {
                (*frame.ptr).buf[index] = buf;
                (*frame.ptr).data[index] = data.cast_mut();
                (*frame.ptr).linesize[index] = c_int::try_from(stride).ok()?;
            }
        }
        Some(frame)
    }

    /// Allocate buffers for `format` at `width`x`height`.
    pub unsafe fn alloc(&mut self, format: Pixel, width: u32, height: u32) {
        self.set_format(format);
        self.set_width(width);
        self.set_height(height);
        // SAFETY: format and size are set; 32-byte aligned planes like ffmpeg-next.
        unsafe { (loader::loaded().util.av_frame_get_buffer)(self.ptr, 32) };
    }

    pub unsafe fn as_ptr(&self) -> *const raw::AVFrame {
        self.ptr
    }

    pub unsafe fn as_mut_ptr(&mut self) -> *mut raw::AVFrame {
        self.ptr
    }

    fn raw(&self) -> &raw::AVFrame {
        // SAFETY: valid owned frame.
        unsafe { &*self.ptr }
    }

    pub fn is_empty(&self) -> bool {
        self.raw().data[0].is_null()
    }

    pub fn width(&self) -> u32 {
        self.raw().width.max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.raw().height.max(0) as u32
    }

    pub fn format(&self) -> Pixel {
        // SAFETY: `format` holds an AVPixelFormat for video frames.
        Pixel(unsafe { std::mem::transmute::<c_int, raw::AVPixelFormat>(self.raw().format) })
    }

    pub fn set_format(&mut self, format: Pixel) {
        // SAFETY: valid owned frame.
        unsafe { (*self.ptr).format = format.0 as c_int };
    }

    pub fn set_width(&mut self, width: u32) {
        // SAFETY: valid owned frame.
        unsafe { (*self.ptr).width = width as c_int };
    }

    pub fn set_height(&mut self, height: u32) {
        // SAFETY: valid owned frame.
        unsafe { (*self.ptr).height = height as c_int };
    }

    pub fn pts(&self) -> Option<i64> {
        let pts = self.raw().pts;
        (pts != raw::AV_NOPTS_VALUE).then_some(pts)
    }

    /// The best-effort presentation timestamp.
    pub fn timestamp(&self) -> Option<i64> {
        let ts = self.raw().best_effort_timestamp;
        (ts != raw::AV_NOPTS_VALUE).then_some(ts)
    }

    pub fn set_pts(&mut self, pts: Option<i64>) {
        // SAFETY: valid owned frame.
        unsafe { (*self.ptr).pts = pts.unwrap_or(raw::AV_NOPTS_VALUE) };
    }

    /// Number of planes with data.
    pub fn planes(&self) -> usize {
        let raw = self.raw();
        (0..raw.data.len())
            .take_while(|&i| !raw.data[i].is_null() && raw.linesize[i] != 0)
            .count()
    }

    pub fn stride(&self, index: usize) -> usize {
        self.raw()
            .linesize
            .get(index)
            .map_or(0, |l| (*l).max(0) as usize)
    }

    /// Rows in plane `index`, from the pixel format's chroma subsampling.
    fn plane_height(&self, index: usize) -> usize {
        let height = self.height() as usize;
        if index == 0 {
            return height;
        }
        // SAFETY: descriptor lookup for a valid pixel format; null for unknown formats.
        let desc = unsafe { (loader::loaded().util.av_pix_fmt_desc_get)(self.format().0) };
        if desc.is_null() {
            return height;
        }
        // SAFETY: non-null static descriptor.
        let shift = unsafe { (*desc).log2_chroma_h } as usize;
        (height + (1 << shift) - 1) >> shift
    }

    pub fn data(&self, index: usize) -> &[u8] {
        let raw = self.raw();
        match raw.data.get(index) {
            Some(ptr) if !ptr.is_null() => {
                let len = self.stride(index) * self.plane_height(index);
                // SAFETY: FFmpeg plane buffer of at least linesize * rows bytes.
                unsafe { std::slice::from_raw_parts(*ptr, len) }
            }
            _ => &[],
        }
    }

    pub fn data_mut(&mut self, index: usize) -> &mut [u8] {
        let len = self.stride(index) * self.plane_height(index);
        // SAFETY: valid owned frame.
        let ptr = unsafe { (*self.ptr).data.get(index).copied() };
        match ptr {
            // SAFETY: FFmpeg plane buffer of at least linesize * rows bytes, owned by us.
            Some(ptr) if !ptr.is_null() => unsafe { std::slice::from_raw_parts_mut(ptr, len) },
            _ => &mut [],
        }
    }
}

impl Clone for Video {
    fn clone(&self) -> Self {
        // SAFETY: new reference to the same buffers.
        let ptr = unsafe { (loader::loaded().util.av_frame_clone)(self.ptr) };
        assert!(!ptr.is_null(), "av_frame_clone failed");
        Self { ptr }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        // SAFETY: owned pointer from av_frame_alloc/av_frame_clone.
        unsafe { (loader::loaded().util.av_frame_free)(&mut self.ptr) };
    }
}
