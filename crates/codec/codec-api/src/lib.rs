//! Codecs: one trait family for encoders, decoders and converters, over [`styx_frame`] leases.
//!
//! - [`Encoder`], [`Decoder`] and [`Converter`] share the [`Codec`] supertrait (a
//!   [`CodecDescriptor`]: kind, input and output [`FourCc`], backend, name). Each call takes the
//!   input lease and an output lease the caller acquired from its own pool. The codec writes into
//!   the output and sets its entry; it allocates nothing and copies nothing it does not have to.
//! - [`Registry`] holds [`Candidate`]s, each a descriptor plus a constructor. [`Registry::select`]
//!   tries the native candidates first (in registration order), then FFmpeg, and returns the first
//!   that constructs, with a [`Decision`] for every candidate: chosen, skipped (and why), or not
//!   tried. Nothing is logged here; the caller logs the decisions it wants.
//! - A request nothing satisfies fails with [`SelectError`], which names the input and output
//!   formats and carries the same decisions.
//!
//! Construction allocates (it runs at setup, once per stream); the per-frame calls do not.

mod codec;
mod registry;

pub use codec::{
    Backend, Codec, CodecDescriptor, CodecError, CodecKind, Converter, Decoder, Encoder,
    Implementation,
};
pub use registry::{Candidate, Decision, Registry, Request, SelectError, Selection, Verdict};
pub use styx_format::FourCc;
pub use styx_frame::FrameLease;
