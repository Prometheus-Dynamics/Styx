use super::*;

/// An owned packet (`AVPacket`).
pub struct Packet {
    ptr: *mut raw::AVPacket,
}

// SAFETY: owned packet data.
unsafe impl Send for Packet {}

impl Packet {
    pub fn empty() -> Self {
        // SAFETY: plain allocation.
        let ptr = unsafe { (loader::loaded().codec.av_packet_alloc)() };
        assert!(!ptr.is_null(), "av_packet_alloc failed");
        Self { ptr }
    }

    /// A packet holding a copy of `data`.
    pub fn copy(data: &[u8]) -> Self {
        let packet = Self::empty();
        // SAFETY: allocates `data.len()` bytes plus padding, then copies into them.
        unsafe {
            if (loader::loaded().codec.av_new_packet)(packet.ptr, data.len() as c_int) == 0 {
                ptr::copy_nonoverlapping(data.as_ptr(), (*packet.ptr).data, data.len());
            }
        }
        packet
    }

    pub fn data(&self) -> Option<&[u8]> {
        // SAFETY: valid owned packet.
        let raw = unsafe { &*self.ptr };
        if raw.data.is_null() {
            None
        } else {
            // SAFETY: packet buffer of `size` bytes.
            Some(unsafe { std::slice::from_raw_parts(raw.data, raw.size.max(0) as usize) })
        }
    }

    pub fn size(&self) -> usize {
        // SAFETY: valid owned packet.
        unsafe { (*self.ptr).size.max(0) as usize }
    }

    pub fn pts(&self) -> Option<i64> {
        // SAFETY: valid owned packet.
        let pts = unsafe { (*self.ptr).pts };
        (pts != raw::AV_NOPTS_VALUE).then_some(pts)
    }

    pub fn set_pts(&mut self, pts: Option<i64>) {
        // SAFETY: valid owned packet.
        unsafe { (*self.ptr).pts = pts.unwrap_or(raw::AV_NOPTS_VALUE) };
    }

    pub fn stream(&self) -> usize {
        // SAFETY: valid owned packet.
        unsafe { (*self.ptr).stream_index.max(0) as usize }
    }

    pub fn as_ptr(&self) -> *const raw::AVPacket {
        self.ptr
    }

    pub unsafe fn as_mut_ptr(&mut self) -> *mut raw::AVPacket {
        self.ptr
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: owned pointer from av_packet_alloc.
        unsafe { (loader::loaded().codec.av_packet_free)(&mut self.ptr) };
    }
}
