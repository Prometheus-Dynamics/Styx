//! The shape of one producer's ring and buffers: the one place its numbers live.
//!
//! The producer is built from a [`Geometry`]; the handshake carries it to readers. Where each
//! buffer sits (its descriptor, offset and length) is the handshake's buffer table, not this
//! record: a pool built by [`Producer::new`](crate::Producer::new) has its buffers at fixed
//! offsets in one memfd, and a pool given to [`Producer::with_pool`](crate::Producer::with_pool)
//! has whatever layout its memory has.

use shm_ring::Spec;
use shm_ring_buffers::MAX_BUFFERS;

use crate::SocketError;
use crate::wire::{FRAME_META_MAX, META_MAX, USER_MAX};

/// The most entries a ring keeps (the ring's own limit).
const MAX_HISTORY: u32 = shm_ring::MAX_HISTORY;

/// The shape of a producer's ring and its buffers.
///
/// Each buffer's first `payload()` bytes are a frame's, and it holds `buffer_bytes()` in all.
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

    /// The most opaque bytes a frame may carry ([`crate::Producer::publish_with`]): the entry's
    /// metadata room less the frame's own part and its length, capped at [`crate::USER_MAX`].
    pub const fn user_max(&self) -> usize {
        let room = self.meta_max.saturating_sub(FRAME_META_MAX + 2);
        if room < USER_MAX { room } else { USER_MAX }
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

    /// Where buffer `index` starts in the pool's memfd, when the pool is one memfd of equal
    /// buffers ([`Producer::new`](crate::Producer::new)).
    pub const fn offset(&self, index: usize) -> usize {
        index * self.buffer_bytes()
    }

    /// The size of a [`Producer::new`](crate::Producer::new) pool's memfd: every buffer.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_of_a_new_pool_sit_at_fixed_offsets_in_one_memfd() {
        let geometry = Geometry::new(2048, 300, 8).expect("geometry");
        assert_eq!(geometry.offset(0), 0);
        assert_eq!(geometry.offset(299), 299 * 2048);
        assert_eq!(geometry.pool_bytes(), 300 * 2048);
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
