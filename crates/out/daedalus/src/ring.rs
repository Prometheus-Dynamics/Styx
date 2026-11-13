//! A ring reader's frames as `styx:frame` values.

use styx_socket::{ReadError, Reader};

use crate::value::StyxFrame;

/// The next frame the reader has, as a value. The value holds the frame's buffer and the ring's
/// hold on it until it drops, so the producer keeps the buffer while the graph has the value.
pub fn next_value(reader: &mut Reader) -> Result<Option<StyxFrame>, ReadError> {
    Ok(reader.next_frame()?.map(StyxFrame::from_ring))
}

/// The newest frame the reader has, as a value; older ones are passed over and counted as
/// superseded, as [`Reader::latest`] does.
pub fn latest_value(reader: &mut Reader) -> Result<Option<StyxFrame>, ReadError> {
    Ok(reader.latest()?.map(StyxFrame::from_ring))
}
