//! The Orion side: a topic that names a frame ring, and the lookup that finds it.
//!
//! The topic is a latest-value topic with one field of bytes. Its sample is an [`Advert`]. Frames
//! are not in it, and nothing here reads or copies one.

use core::fmt;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use orion::Orion;
use orion::topics::{FieldId, FrameTime, OnSubscribe, Publisher, Subscriber, TopicDecl};
use shm_sample_time::SampleTime;
use styx_socket::{Reader, SocketError};

use crate::advert::{Advert, AdvertError, MAX_LEN};

/// The topic's one field: the discovery record.
const RECORD: FieldId = FieldId(0);
/// Two deep: a reader that looks between two adverts sees the newer one.
const HISTORY: u32 = 2;

/// Why a topic could not be declared, found or connected to.
#[derive(Debug)]
pub enum OrionError {
    /// Orion refused, or the connection to the comm core broke.
    Orion(orion::Error),
    /// The record in the topic is not one this build reads, or would not be made.
    Advert(AdvertError),
    /// The socket the record names could not be reached.
    Io(io::Error),
    /// The producer's handshake failed.
    Socket(SocketError),
}

impl fmt::Display for OrionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Orion(err) => write!(f, "orion: {err}"),
            Self::Advert(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "frame socket: {err}"),
            Self::Socket(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for OrionError {}

impl From<orion::Error> for OrionError {
    fn from(err: orion::Error) -> Self {
        Self::Orion(err)
    }
}

impl From<orion::topics::TopicError> for OrionError {
    fn from(err: orion::topics::TopicError) -> Self {
        Self::Orion(orion::Error::from(err))
    }
}

impl From<AdvertError> for OrionError {
    fn from(err: AdvertError) -> Self {
        Self::Advert(err)
    }
}

impl From<io::Error> for OrionError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<SocketError> for OrionError {
    fn from(err: SocketError) -> Self {
        Self::Socket(err)
    }
}

/// The publisher side of a topic that names a frame ring. Dropping it leaves the topic to Orion.
pub struct Advertisement {
    publisher: Publisher,
}

impl fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Advertisement")
    }
}

impl Advertisement {
    /// Publishes `advert` as the topic's newest record. Call again when the ring's shape or
    /// socket changes.
    ///
    /// # Errors
    ///
    /// [`OrionError::Advert`] when the record cannot be made, [`OrionError::Orion`] when the
    /// connection broke.
    pub fn update(&mut self, advert: &Advert) -> Result<(), OrionError> {
        let mut buffer = [0_u8; MAX_LEN];
        let len = advert.encode(&mut buffer)?;
        let time = FrameTime::from_nanos(SampleTime::now().as_nanos());
        self.publisher.publish(RECORD, time, &buffer[..len])?;
        Ok(())
    }
}

/// Declares the topic `name` on `orion` and publishes `advert` as its first record.
///
/// # Errors
///
/// As [`Advertisement::update`], and [`OrionError::Orion`] when the comm core refuses the topic
/// (another program declared it).
pub fn advertise(orion: &Orion, name: &str, advert: &Advert) -> Result<Advertisement, OrionError> {
    let publisher = orion.declare(&TopicDecl {
        name,
        fields: &orion::fields![record: Bytes],
        history: HISTORY,
        expiry_ms: 0,
        max_payload: MAX_LEN as u32,
        // A subscriber that connects gets the newest record at once: the frame ring's advert is
        // the one the reader needs, so it is not made to wait for the next publish.
        on_subscribe: OnSubscribe::Latest,
    })?;
    let mut advertisement = Advertisement { publisher };
    advertisement.update(advert)?;
    Ok(advertisement)
}

/// A frame ring found by name. Holding it keeps the subscription to the topic.
pub struct Discovered {
    /// The record the topic held when it was found.
    pub advert: Advert,
    subscriber: Subscriber,
}

impl fmt::Debug for Discovered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovered")
            .field("advert", &self.advert)
            .finish_non_exhaustive()
    }
}

impl Discovered {
    /// Connects to the ring's producer and takes its handshake. The frames then arrive through
    /// the ring, read in place; Orion is not involved.
    ///
    /// # Errors
    ///
    /// [`OrionError::Io`] when the socket is not there, [`OrionError::Socket`] when the handshake
    /// fails.
    pub fn connect(&self) -> Result<Reader, OrionError> {
        let stream = UnixStream::connect(&self.advert.socket)?;
        Ok(Reader::connect(stream)?)
    }

    /// The subscription to the topic, for records published after this one.
    pub const fn subscriber(&mut self) -> &mut Subscriber {
        &mut self.subscriber
    }
}

/// Finds the topic `name` on `orion`, waiting up to `wait` for a producer to publish it.
///
/// `Ok(None)` when nothing is published under the name in time.
///
/// # Errors
///
/// [`OrionError::Orion`] when the comm core refuses or the connection broke,
/// [`OrionError::Advert`] when the topic's record is not one this build reads.
pub fn discover(
    orion: &Orion,
    name: &str,
    wait: Duration,
) -> Result<Option<Discovered>, OrionError> {
    let mut subscriber = orion.subscribe(name)?;
    let deadline = Instant::now() + wait;
    loop {
        let record = subscriber
            .latest(RECORD)
            .map(|sample| Advert::decode(sample.payload));
        if let Some(record) = record {
            return Ok(Some(Discovered {
                advert: record?,
                subscriber,
            }));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        subscriber.wait(left)?;
    }
}
