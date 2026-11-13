//! The discovery record: what a reader needs to find a Styx frame ring and connect to it.
//!
//! The record is opaque to Orion, which carries it as bytes and reads none of it. Its layout is
//! fixed and little-endian:
//!
//! | Bytes | Holds |
//! | --- | --- |
//! | 0 | Version: 1 |
//! | 1 to 4 | The format's four-character code |
//! | 5 to 8 | Width, 9 to 12 height, in pixels |
//! | 13 to 16 | Payload bytes per buffer |
//! | 17 to 20 | Buffers in the pool |
//! | 21 to 22 | Length of the socket path, then the path (UTF-8) |

use core::fmt;
use std::num::NonZeroU32;

use styx_format::FourCc;

/// The record's version. A reader refuses any other.
pub const VERSION: u8 = 1;
/// The most bytes a record takes: the path is at most this long less the fixed fields.
pub const MAX_LEN: usize = 512;
/// The longest socket path a record can name.
pub const MAX_SOCKET: usize = MAX_LEN - FIXED;

const FIXED: usize = 23;

/// What a frame ring is: where its producer accepts readers, and the shape of its frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// The format every frame in the ring has.
    pub code: FourCc,
    /// The frame's width in pixels.
    pub width: NonZeroU32,
    /// The frame's height in pixels.
    pub height: NonZeroU32,
    /// Bytes in each buffer of the producer's pool.
    pub payload: u32,
    /// Buffers in the producer's pool.
    pub buffers: u32,
    /// The Unix socket the producer accepts readers on.
    pub socket: String,
}

/// Why a record could not be made or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvertError {
    /// The socket path is empty or longer than [`MAX_SOCKET`].
    Socket,
    /// The record is shorter than its fields, or its path is not UTF-8.
    Malformed,
    /// The record's version is not [`VERSION`].
    Version(u8),
    /// A field is zero where a frame needs a size.
    Zero,
}

impl fmt::Display for AdvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket => write!(f, "a socket path must be 1 to {MAX_SOCKET} bytes"),
            Self::Malformed => f.write_str("a discovery record that does not parse"),
            Self::Version(version) => {
                write!(f, "discovery record version {version}, not {VERSION}")
            }
            Self::Zero => f.write_str("a discovery record with a zero size"),
        }
    }
}

impl std::error::Error for AdvertError {}

impl Advert {
    /// Writes the record into `out`, returning its length.
    ///
    /// # Errors
    ///
    /// [`AdvertError::Socket`] when the path is empty or too long.
    pub fn encode(&self, out: &mut [u8; MAX_LEN]) -> Result<usize, AdvertError> {
        let path = self.socket.as_bytes();
        if path.is_empty() || path.len() > MAX_SOCKET {
            return Err(AdvertError::Socket);
        }
        out[0] = VERSION;
        out[1..5].copy_from_slice(&self.code.bytes());
        out[5..9].copy_from_slice(&self.width.get().to_le_bytes());
        out[9..13].copy_from_slice(&self.height.get().to_le_bytes());
        out[13..17].copy_from_slice(&self.payload.to_le_bytes());
        out[17..21].copy_from_slice(&self.buffers.to_le_bytes());
        out[21..23].copy_from_slice(&(path.len() as u16).to_le_bytes());
        out[FIXED..FIXED + path.len()].copy_from_slice(path);
        Ok(FIXED + path.len())
    }

    /// Reads a record another process wrote.
    ///
    /// # Errors
    ///
    /// [`AdvertError`] for a record that is not this version, is short, or names no socket.
    pub fn decode(bytes: &[u8]) -> Result<Self, AdvertError> {
        let version = *bytes.first().ok_or(AdvertError::Malformed)?;
        if version != VERSION {
            return Err(AdvertError::Version(version));
        }
        if bytes.len() < FIXED {
            return Err(AdvertError::Malformed);
        }
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let len = usize::from(u16::from_le_bytes([bytes[21], bytes[22]]));
        if len == 0 || len > MAX_SOCKET {
            return Err(AdvertError::Socket);
        }
        let path = bytes
            .get(FIXED..FIXED + len)
            .ok_or(AdvertError::Malformed)?;
        let socket = core::str::from_utf8(path)
            .map_err(|_| AdvertError::Malformed)?
            .to_owned();
        let width = NonZeroU32::new(u32_at(5)).ok_or(AdvertError::Zero)?;
        let height = NonZeroU32::new(u32_at(9)).ok_or(AdvertError::Zero)?;
        let payload = u32_at(13);
        let buffers = u32_at(17);
        if payload == 0 || buffers == 0 {
            return Err(AdvertError::Zero);
        }
        Ok(Self {
            code: FourCc::new([bytes[1], bytes[2], bytes[3], bytes[4]]),
            width,
            height,
            payload,
            buffers,
            socket,
        })
    }
}
