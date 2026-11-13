//! The producer's ring and pool, adopted as an Orion buffer topic.
//!
//! [`publish`] makes the producer's own ring the topic's ring and its own pool the topic's pool.
//! Orion's subscribers are handed the same objects: the same ring, the same buffers, and their hold
//! marks go into the producer's hold table. The producer keeps writing with its own objects; the
//! caller wakes Orion's subscribers after each publish with [`Published::wake`].

use orion::Orion;
use orion::buffers::{AdoptedBuffers, HoldTable};
use shm_ring_buffers::entry;
use styx_socket::Producer;

use crate::topic::OrionError;

/// Orion's own overhead per buffer-topic entry, in this rev (its header for up to eight buffers).
/// A topic's `max_payload` is this plus the publisher's metadata. A ring the producer made has
/// `max_payload` of `entry::len(1, meta)`, so the metadata Orion is told is that less this.
const ENTRY_OVERHEAD: u32 = 72;

/// A producer's ring and pool, published as an Orion buffer topic.
///
/// Orion's subscribers are rung by [`Published::wake`], which Orion itself does for its own
/// subscribers' doorbells: one eventfd write for each that is asleep. Orion gives no doorbell
/// events, so the producer's [`styx_socket::Producer::add_bell`] is not used for them.
///
/// Dropping it leaves the topic until the session ends; [`Published::withdraw`] ends it now.
#[derive(Debug)]
pub struct Published {
    adopted: AdoptedBuffers,
}

/// Adopts `producer`'s ring and pool as the buffer topic `name` on `orion`.
///
/// The history and the metadata size come from the producer's geometry, so the declaration is
/// the ring's shape. `reader_slots` are the hold-table slots Orion may give its subscribers, a bit
/// for each of the 64. Zero is refused: at this Orion rev an adopted ring has readers Orion did
/// not place, so Orion must be told which slots are its own. Direct socket readers take the
/// lowest free slot, so a caller that also has direct readers should give Orion the high bits.
///
/// Adopting again under the same name from the same session resumes the same memory. Another
/// ring is checked against the declaration, and refused as malformed when its shape differs.
///
/// # Errors
///
/// [`OrionError::Shape`] when the ring's entries are too small for a topic header, or
/// [`OrionError::Orion`] when `reader_slots` is zero, the session is not on the board, or the comm
/// core refuses (a ring of another shape than the declaration is `malformed`).
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
    /// Rings Orion's subscribers that are asleep, after the producer published. Call it after each
    /// [`styx_socket::Producer::publish`]. No system call for one that is awake.
    pub fn wake(&mut self) {
        self.adopted.wake();
    }

    /// How many of Orion's subscribers the topic has now.
    #[must_use]
    pub fn subscribers(&self) -> u32 {
        self.adopted.subscribers()
    }

    /// Looks at what the comm core has sent: Orion's subscribers that came, with their marks,
    /// added to the producer's hold table, or went, taken out. A subscriber gets no frames until
    /// this has run. It does not wait; call it when a frame is published or the producer is idle.
    ///
    /// The producer's table is taken out of the producer for the call, so the pool and the table
    /// can be borrowed together, and put back after it, whatever the result.
    ///
    /// # Errors
    ///
    /// [`OrionError::Orion`] when the connection broke or a subscriber's marks cannot be added.
    pub fn poll(&mut self, producer: &mut Producer) -> Result<(), OrionError> {
        let mut holds = std::mem::replace(producer.holds_mut(), HoldTable::new(0));
        let result = self.adopted.poll(producer.pool(), &mut holds);
        *producer.holds_mut() = holds;
        Ok(result?)
    }

    /// Withdraws the topic (a camera that leaves): Orion's subscribers are told the publisher is
    /// gone, and what they hold they keep until they let it go. The producer's own readers go on
    /// reading.
    ///
    /// # Errors
    ///
    /// [`OrionError::Orion`] when the comm core refuses or the connection broke.
    pub fn withdraw(self) -> Result<(), OrionError> {
        Ok(self.adopted.withdraw()?)
    }
}
