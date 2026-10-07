//! Serving previews over HTTP without an HTTP framework: an MJPEG
//! (`multipart/x-mixed-replace`) body as a `Stream` of `Bytes`, and a binary message per frame
//! for WebSockets.
//!
//! ## WebSocket message (`SPV1`)
//!
//! One binary message per frame: a 32-byte little-endian header, then the JPEG file.
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `SPV1` (`53 50 56 31`) |
//! | 4 | 2 | header length (32; skip to it: later versions may add fields) |
//! | 6 | 1 | timestamp clock: 0 unknown, 1 monotonic, 2 boottime, 3 realtime, 4 stream-relative |
//! | 7 | 1 | flags: bit 0 grey, bit 1 camera JPEG passed through |
//! | 8 | 8 | sequence (u64): 1, 2, ... per preview; gaps are frames this viewer skipped |
//! | 16 | 8 | capture timestamp, nanoseconds (u64) on that clock |
//! | 24 | 2 | width (u16) |
//! | 26 | 2 | height (u16) |
//! | 28 | 4 | JPEG length (u32) |
//! | 32 | n | the JPEG |

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{BufMut, Bytes, BytesMut};
use styx_core::prelude::TimestampClock;

use super::PreviewFrame;
use super::output::PreviewSubscriber;

/// The multipart boundary of [`MjpegStream`] bodies.
pub const MJPEG_BOUNDARY: &str = "styxpreview";
/// The `Content-Type` header for an [`MjpegStream`] body.
pub const MJPEG_CONTENT_TYPE: &str = "multipart/x-mixed-replace; boundary=styxpreview";

/// An MJPEG HTTP response body: `multipart/x-mixed-replace` parts, one JPEG each, as a
/// `futures_core::Stream` of `Bytes` chunks (two per frame: the part's headers, then the
/// JPEG, shared without copying). Serve it with [`MJPEG_CONTENT_TYPE`] from any HTTP server
/// (hyper/axum: `Body::from_stream(stream.fallible())`); browsers show it in an `<img>`.
///
/// Latest-frame semantics: a slow connection gets the newest frame each time it can take
/// one, skipping the rest; the stream never queues. It ends when the preview stops.
///
/// Each part carries `X-Timestamp-Ns` (the capture timestamp), `X-Sequence` and
/// `Content-Length`.
pub struct MjpegStream {
    subscriber: PreviewSubscriber,
    /// The JPEG of the part whose headers were just sent.
    pending: Option<Bytes>,
    /// Headers are written here, then split off (its room reused).
    buf: BytesMut,
    /// A part was sent (the next one ends it first).
    started: bool,
}

impl std::fmt::Debug for MjpegStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MjpegStream")
            .field("subscriber", &self.subscriber)
            .finish()
    }
}

impl MjpegStream {
    pub fn new(subscriber: PreviewSubscriber) -> Self {
        Self {
            subscriber,
            pending: None,
            buf: BytesMut::with_capacity(256),
            started: false,
        }
    }

    /// The same body as `Result<Bytes, Infallible>` items, for servers that take a fallible
    /// stream (hyper's and axum's `Body::from_stream`).
    pub fn fallible(self) -> TryMjpegStream {
        TryMjpegStream(self)
    }

    /// The part headers for `frame`: the end of the previous part (after the first), the
    /// boundary and the headers.
    fn headers(&mut self, frame: &PreviewFrame) -> Bytes {
        use std::fmt::Write;
        let mut text = String::with_capacity(160);
        if self.started {
            text.push_str("\r\n");
        }
        self.started = true;
        let _ = write!(
            text,
            "--{MJPEG_BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\
             X-Timestamp-Ns: {}\r\nX-Sequence: {}\r\n\r\n",
            frame.jpeg.len(),
            frame.timestamp_ns,
            frame.sequence
        );
        self.buf.put_slice(text.as_bytes());
        self.buf.split().freeze()
    }

    /// Poll for the next chunk (`None`: the preview stopped).
    pub fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        if let Some(jpeg) = self.pending.take() {
            return Poll::Ready(Some(jpeg));
        }
        match self.subscriber.poll_recv(cx) {
            Poll::Ready(Some(frame)) => {
                let head = self.headers(&frame);
                self.pending = Some(frame.jpeg);
                Poll::Ready(Some(head))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl futures_core::Stream for MjpegStream {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        self.get_mut().poll_chunk(cx)
    }
}

/// [`MjpegStream`] with `Result<Bytes, Infallible>` items ([`MjpegStream::fallible`]).
#[derive(Debug)]
pub struct TryMjpegStream(MjpegStream);

impl futures_core::Stream for TryMjpegStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_chunk(cx).map(|c| c.map(Ok))
    }
}

/// The magic at the start of a [`ws_message`].
pub const WS_MAGIC: [u8; 4] = *b"SPV1";
/// Length of a [`ws_message`]'s header.
pub const WS_HEADER_LEN: usize = 32;

const FLAG_GRAY: u8 = 1;
const FLAG_PASSTHROUGH: u8 = 2;

fn clock_tag(clock: Option<TimestampClock>) -> u8 {
    match clock {
        None => 0,
        Some(TimestampClock::Monotonic) => 1,
        Some(TimestampClock::Boottime) => 2,
        Some(TimestampClock::Realtime) => 3,
        Some(TimestampClock::StreamRelative) => 4,
    }
}

fn clock_from(tag: u8) -> Option<TimestampClock> {
    match tag {
        1 => Some(TimestampClock::Monotonic),
        2 => Some(TimestampClock::Boottime),
        3 => Some(TimestampClock::Realtime),
        4 => Some(TimestampClock::StreamRelative),
        _ => None,
    }
}

/// A frame as one binary WebSocket message: the 32-byte header (see the [module](self)), then
/// the JPEG. One allocation and one copy of the JPEG; send it as a binary message.
pub fn ws_message(frame: &PreviewFrame) -> Bytes {
    let mut out = BytesMut::with_capacity(WS_HEADER_LEN + frame.jpeg.len());
    out.put_slice(&WS_MAGIC);
    out.put_u16_le(WS_HEADER_LEN as u16);
    out.put_u8(clock_tag(frame.clock));
    out.put_u8(
        if frame.gray { FLAG_GRAY } else { 0 }
            | if frame.passthrough {
                FLAG_PASSTHROUGH
            } else {
                0
            },
    );
    out.put_u64_le(frame.sequence);
    out.put_u64_le(frame.timestamp_ns);
    out.put_u16_le(u16::try_from(frame.width).unwrap_or(u16::MAX));
    out.put_u16_le(u16::try_from(frame.height).unwrap_or(u16::MAX));
    out.put_u32_le(u32::try_from(frame.jpeg.len()).unwrap_or(u32::MAX));
    out.put_slice(&frame.jpeg);
    out.freeze()
}

/// A [`ws_message`]'s header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WsHeader {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub clock: Option<TimestampClock>,
    pub width: u16,
    pub height: u16,
    pub gray: bool,
    pub passthrough: bool,
}

/// Read a [`ws_message`]: its header and the JPEG. `None` when it is not one (wrong magic,
/// too short, a length past the end).
pub fn parse_ws_message(message: &[u8]) -> Option<(WsHeader, &[u8])> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(message.get(i..i + 2)?.try_into().ok()?));
    let u64_at = |i: usize| Some(u64::from_le_bytes(message.get(i..i + 8)?.try_into().ok()?));
    if message.get(..4)? != WS_MAGIC {
        return None;
    }
    let header_len = usize::from(u16_at(4)?);
    if header_len < WS_HEADER_LEN {
        return None;
    }
    let flags = *message.get(7)?;
    let len = u32::from_le_bytes(message.get(28..32)?.try_into().ok()?) as usize;
    let jpeg = message.get(header_len..header_len.checked_add(len)?)?;
    Some((
        WsHeader {
            sequence: u64_at(8)?,
            timestamp_ns: u64_at(16)?,
            clock: clock_from(*message.get(6)?),
            width: u16_at(24)?,
            height: u16_at(26)?,
            gray: flags & FLAG_GRAY != 0,
            passthrough: flags & FLAG_PASSTHROUGH != 0,
        },
        jpeg,
    ))
}
