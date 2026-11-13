//! The producer's ring and pool, adopted as an Orion buffer topic.
//!
//! [`publish`] makes the producer's own ring the topic's ring and its own pool the topic's pool.
//! Orion's subscribers are handed the same objects: the same ring, the same buffers, and their hold
//! marks go into the producer's hold table. The producer keeps writing with its own objects; the
//! caller wakes Orion's subscribers after each publish with [`Published::wake`].

use orion::Orion;
use orion::buffers::{AdoptedBuffers, HoldTable, Pool};
use shm_ring_buffers::entry;
use styx_socket::Producer;

use crate::topic::OrionError;

/// Orion's own overhead per buffer-topic entry, in this rev (its header for up to eight buffers).
/// A topic's `max_payload` is this plus the publisher's metadata. A ring the producer made has
/// `max_payload` of `entry::len(1, meta)`, so the metadata Orion is told is that less this.
const ENTRY_OVERHEAD: u32 = 72;

/// A producer's ring and pool, published as an Orion buffer topic. Dropping it leaves the topic
/// until the session ends.
///
/// # Not yet wired (draft, see the PR)
///
/// - [`Published::poll`] takes the producer's hold table, which Orion needs mutably to add a
///   subscriber's marks. `styx_socket::Producer` only lends it out immutably, so a caller cannot
///   run this against a live producer until the socket crate gives a `&mut` to it.
/// - Orion's subscribers' doorbells are rung by [`Published::wake`] (Orion's own bells). Rev
///   181e139 has no doorbell add/remove events, so [`styx_socket::Producer::add_bell`] is not
///   used.
/// - The withdraw hook for a camera that leaves is not here: rev 181e139 has no `withdraw`.
#[derive(Debug)]
pub struct Published {
    adopted: AdoptedBuffers,
}

/// Adopts `producer`'s ring and pool as the buffer topic `name` on `orion`.
///
/// The history and the metadata size come from the producer's geometry, so the declaration is
/// the ring's shape. `reader_slots` are the hold-table slots Orion may give its subscribers, a bit
/// for each of the 64; zero lets it use any. Direct socket readers take the lowest free slot, so
/// a caller that also has direct readers should give Orion the high bits only.
///
/// # Errors
///
/// [`OrionError::Shape`] when the ring's entries are too small for a topic header, or
/// [`OrionError::Orion`] when the session is not on the board or the comm core refuses (a ring of
/// another shape than the topic is refused as malformed).
pub fn publish(
    orion: &Orion,
    name: &str,
    producer: &Producer,
    reader_slots: u64,
) -> Result<Published, OrionError> {
    let geometry = producer.geometry();
    let ring_payload = entry::len(1, geometry.meta_max()) as u32;
    let max_meta = ring_payload
        .checked_sub(ENTRY_OVERHEAD)
        .ok_or(OrionError::Shape)?;
    let adopted = orion.adopt_buffers(
        name,
        geometry.history(),
        max_meta,
        producer.ring(),
        producer.pool(),
        reader_slots,
    )?;
    Ok(Published { adopted })
}

impl Published {
    /// Wakes Orion's subscribers that are asleep, after the producer published. No system call for
    /// one that is awake.
    pub fn wake(&mut self) {
        self.adopted.wake();
    }

    /// How many of Orion's subscribers the topic has now.
    #[must_use]
    pub fn subscribers(&self) -> u32 {
        self.adopted.subscribers()
    }

    /// Looks at what the comm core has sent: Orion's subscribers that came, with their marks, put
    /// in `holds`, or went, taken out. `holds` must be the producer's hold table and `pool` its
    /// pool; a subscriber gets no frames until this has run. It does not wait.
    ///
    /// # Errors
    ///
    /// [`OrionError::Orion`] when the connection broke or a subscriber's marks cannot be added.
    pub fn poll(&mut self, pool: &Pool, holds: &mut HoldTable) -> Result<(), OrionError> {
        Ok(self.adopted.poll(pool, holds)?)
    }
}
