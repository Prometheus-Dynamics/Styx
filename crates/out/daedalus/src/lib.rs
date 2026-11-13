//! Styx frames as Daedalus values.
//!
//! With the `daedalus` feature, a frame is the `styx:frame` value of a Daedalus graph. The value is
//! a [`StyxFrame`]: a Styx frame's buffer, held until the graph lets the last value go, and the
//! dma-buf that holds its bytes, which Daedalus moves without reading (the plain `dmabuf` memory
//! location of `daedalus-dmabuf`). Daedalus learns the frame's bytes and their length, never its
//! format: sampling a frame as a texture by format is Styx's own node, not this crate's.
//!
//! - **In process**: a [`StyxFrame`] is made from a [`FrameLease`](styx_frame::FrameLease) and the
//!   pool's [`BufferSize`](styx_frame::BufferSize), and handed to a graph as a host input.
//! - **Out of process**: [`next_value`] and [`latest_value`] take the next frame from a
//!   [`styx_socket::Reader`] as a `styx:frame` value, so a graph in another process holds the
//!   ring's buffer until the value drops.
//!
//! Without the feature this crate is empty, and no Daedalus crate is built.

#[cfg(all(feature = "daedalus", target_os = "linux"))]
mod value;

#[cfg(all(feature = "daedalus", target_os = "linux"))]
mod ring;

#[cfg(all(feature = "daedalus", target_os = "linux"))]
pub use ring::{latest_value, next_value};
#[cfg(all(feature = "daedalus", target_os = "linux"))]
pub use value::{FRAME_KEY, StyxFrame, dmabuf_memory};
