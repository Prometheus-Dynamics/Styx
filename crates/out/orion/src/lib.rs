//! Orion as the discovery and reach for a Styx frame ring.
//!
//! The frames never go through Orion. A camera service runs a `styx_socket::Producer` on a Unix
//! socket; its frames are the ring's entries. Orion's part is the name: with the `orion` feature,
//! [`advertise`] declares an Orion topic whose one sample is an [`Advert`] (the socket path, the
//! format, the pool's shape), and [`discover`] finds that topic by name and returns it, so a
//! reader connects to the producer's socket and reads the same ring with no copy.
//!
//! - Orion carries the record as opaque bytes and reads none of it. It keeps no frame.
//! - A reader that discovers a ring connects to the producer directly. Orion is not in the
//!   path of any frame.
//! - Reach: a socket is on one board. A ring on another board needs a carrier Orion does not
//!   have yet; see the PR's notes.
//!
//! The record codec is always built. The Orion client is behind the `orion` feature, which is
//! off by default. Linux only, like the ring it names.

#![cfg(target_os = "linux")]

mod advert;

pub use advert::{Advert, AdvertError, MAX_LEN, MAX_SOCKET, VERSION};

#[cfg(feature = "orion")]
mod topic;

#[cfg(feature = "orion")]
pub use topic::{Advertisement, Discovered, OrionError, advertise, discover};
