//! The codec traits and the descriptor every implementation carries.

use std::fmt;

use styx_format::FourCc;
use styx_frame::FrameLease;

/// What an implementation does with frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecKind {
    /// Raw frames in, compressed packets out.
    Encoder,
    /// Compressed packets in, raw frames out.
    Decoder,
    /// Raw frames in, raw frames of another format out (a colour converter, a scaler).
    Converter,
}

impl fmt::Display for CodecKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Encoder => "encoder",
            Self::Decoder => "decoder",
            Self::Converter => "converter",
        })
    }
}

/// Where an implementation runs. The registry tries native implementations before FFmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    /// Styx's own code, or Lemnos's hardware bindings behind it.
    Native,
    /// FFmpeg, as a coverage backend. Optional; the default build has none.
    Ffmpeg,
}

impl Backend {
    /// The order the registry tries backends in: lower first.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Native => 0,
            Self::Ffmpeg => 1,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Native => "native",
            Self::Ffmpeg => "ffmpeg",
        })
    }
}

/// What an implementation is: its kind, the formats it turns one into the other, where it runs,
/// and a name for logs and for choosing it by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodecDescriptor {
    pub kind: CodecKind,
    pub input: FourCc,
    pub output: FourCc,
    pub backend: Backend,
    pub name: &'static str,
}

/// Why an implementation could not be built or could not process a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The implementation cannot run on this machine (no device, no library, not compiled in).
    Unavailable(String),
    /// The input does not fit the implementation (size, plane layout, bitstream).
    Rejected(String),
    /// Processing failed after the implementation started.
    Failed(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
            Self::Rejected(why) => write!(f, "rejected input: {why}"),
            Self::Failed(why) => write!(f, "failed: {why}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Every codec: its descriptor. The per-frame calls are on [`Encoder`], [`Decoder`] and
/// [`Converter`].
///
/// `M` is the per-frame metadata of the pool the leases come from ([`styx_frame::FramePool`]).
pub trait Codec<M = ()>: Send {
    fn descriptor(&self) -> &CodecDescriptor;
}

/// Raw frames in, compressed packets out.
///
/// `output` is a lease the caller acquired from its own pool, sole owner, so it can be written.
/// The encoder fills its planes (or its packet bytes), sets its entry, and returns. Steady-state
/// calls allocate nothing: the output buffer comes from the caller, and the encoder keeps its own
/// scratch across calls.
pub trait Encoder<M = ()>: Codec<M> {
    fn encode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError>;
}

/// Compressed packets in, raw frames out. The same call shape as [`Encoder`].
pub trait Decoder<M = ()>: Codec<M> {
    fn decode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError>;
}

/// Raw frames in, raw frames of another format or size out. The same call shape as [`Encoder`].
pub trait Converter<M = ()>: Codec<M> {
    fn convert(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError>;
}

/// A constructed implementation, of whichever kind it is.
pub enum Implementation<M = ()> {
    Encoder(Box<dyn Encoder<M>>),
    Decoder(Box<dyn Decoder<M>>),
    Converter(Box<dyn Converter<M>>),
}

impl<M> Implementation<M> {
    pub fn descriptor(&self) -> &CodecDescriptor {
        match self {
            Self::Encoder(codec) => codec.descriptor(),
            Self::Decoder(codec) => codec.descriptor(),
            Self::Converter(codec) => codec.descriptor(),
        }
    }

    pub fn kind(&self) -> CodecKind {
        self.descriptor().kind
    }
}

impl<M> fmt::Debug for Implementation<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Implementation")
            .field(self.descriptor())
            .finish()
    }
}
