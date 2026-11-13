//! The shape of one producer's ring and buffers: the one place its numbers live.
//!
//! The producer is built from a [`Geometry`]; the handshake carries it to readers; a reader
//! derives its buffer layout and its frame pool from it. Nothing else holds a copy of these
//! numbers.

use shm_ring::Spec;
use shm_ring_buffers::{BufferDesc, MAX_BUFFERS};

use crate::SocketError;
use crate::wire::META_MAX;

/// The most entries a ring keeps (the ring's own limit).
const MAX_HISTORY: u32 = shm_ring::MAX_HISTORY;

/// The shape of a producer's ring and its buffers.
///
/// Every buffer sits at a fixed offset in the pool's one memfd: buffer `i` starts at
/// `i * buffer_bytes()`, and its first `payload()` bytes are the frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    history: u32,
    meta_max: usize,
    buffers: usize,
    payload: usize,
    slack: usize,
}

impl Geometry {
    /// The geometry for `payload` bytes in each of `buffers` buffers, with `history` ring
    /// entries. The ring entries carry at most the crate's metadata size, and there is no GPU
    /// slack.
    ///
    /// # Errors
    ///
    /// [`SocketError::Config`] when the shape is not one a ring and pool can hold.
    pub fn new(payload: usize, buffers: usize, history: u32) -> Result<Self, SocketError> {
        Self::checked(history, META_MAX, buffers, payload, 0)
    }

    /// The geometry a handshake names, checked the same way [`Geometry::new`] checks it.
    pub(crate) fn checked(
        history: u32,
        meta_max: usize,
        buffers: usize,
        payload: usize,
        slack: usize,
    ) -> Result<Self, SocketError> {
        if payload == 0 {
            return Err(SocketError::Config("a buffer holds at least one byte"));
        }
        if buffers == 0 || buffers > MAX_BUFFERS as usize {
            return Err(SocketError::Config("a pool holds 1 to 1024 buffers"));
        }
        if history == 0 || history > MAX_HISTORY {
            return Err(SocketError::Config("a ring keeps 1 to 2^24 entries"));
        }
        if meta_max > META_MAX {
            return Err(SocketError::Config(
                "a frame's metadata is at most META_MAX bytes",
            ));
        }
        let geometry = Self {
            history,
            meta_max,
            buffers,
            payload,
            slack,
        };
        geometry
            .buffer_bytes_checked()
            .and_then(|bytes| bytes.checked_mul(buffers))
            .ok_or(SocketError::Config("the pool is too large"))?;
        Ok(geometry)
    }

    /// Ring entries kept: how far a reader may fall behind before it loses frames.
    pub const fn history(&self) -> u32 {
        self.history
    }

    /// The most metadata bytes one ring entry carries (the entry's size, less its header).
    pub const fn meta_max(&self) -> usize {
        self.meta_max
    }

    /// Buffers in the pool.
    pub const fn buffers(&self) -> usize {
        self.buffers
    }

    /// Bytes a frame may use in each buffer, from the buffer's start.
    pub const fn payload(&self) -> usize {
        self.payload
    }

    /// Bytes past the payload in each buffer, left for a GPU import.
    pub const fn slack(&self) -> usize {
        self.slack
    }

    /// Bytes each buffer takes in the pool: payload and slack.
    pub const fn buffer_bytes(&self) -> usize {
        self.payload + self.slack
    }

    /// Where buffer `index` starts in the pool's memfd.
    pub const fn offset(&self, index: usize) -> usize {
        index * self.buffer_bytes()
    }

    /// The size of the pool's memfd: every buffer.
    pub fn pool_bytes(&self) -> usize {
        self.offset(self.buffers)
    }

    fn buffer_bytes_checked(&self) -> Option<usize> {
        self.payload.checked_add(self.slack)
    }

    /// The ring's shape: one field of entries of this geometry's size.
    pub(crate) fn ring_spec(&self) -> Spec {
        Spec {
            fields: 1,
            history: self.history,
            max_payload: shm_ring_buffers::entry::len(1, self.meta_max) as u32,
            expiry_ms: 0,
        }
    }

    /// Where each buffer is, as the pool and the ring describe it: all in descriptor 0.
    pub(crate) fn buffer_descs(&self) -> Vec<BufferDesc> {
        (0..self.buffers)
            .map(|index| BufferDesc {
                descriptor: 0,
                offset: self.offset(index) as u64,
                size: self.buffer_bytes() as u64,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_sit_at_fixed_offsets_in_one_pool() {
        let geometry = Geometry::new(2048, 300, 8).expect("geometry");
        assert_eq!(geometry.offset(0), 0);
        assert_eq!(geometry.offset(299), 299 * 2048);
        assert_eq!(geometry.pool_bytes(), 300 * 2048);
        let descs = geometry.buffer_descs();
        assert_eq!(descs.len(), 300);
        assert!(descs.iter().all(|desc| desc.descriptor == 0));
        assert_eq!(descs[1].offset, 2048);
    }

    #[test]
    fn shapes_a_ring_and_pool_cannot_hold_are_refused() {
        assert!(Geometry::new(0, 4, 8).is_err());
        assert!(Geometry::new(64, 0, 8).is_err());
        assert!(Geometry::new(64, MAX_BUFFERS as usize + 1, 8).is_err());
        assert!(Geometry::new(64, 4, 0).is_err());
        assert!(Geometry::new(usize::MAX / 2, 4, 8).is_err());
        assert!(Geometry::checked(8, META_MAX + 1, 4, 64, 0).is_err());
    }
}
