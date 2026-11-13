//! Styx's own H.264 encoder, pure Rust, on the native backend of `styx-codec-api`.
//!
//! [`StyxH264`] is the encoder: Constrained Baseline, CAVLC, one slice per picture and one
//! reference frame, the deblocking filter off. I pictures are Intra16x16 with DC chroma; P
//! pictures add P_Skip and P_L0_16x16 with a whole-pel diamond search refined to quarter pels.
//! Each picture's QP is fixed, or chosen to hold a [`RateTarget`]. The arithmetic (SAD, the
//! 4x4 transform and quantiser, the reconstruction, the half-pel filter) is `styx-simd`'s
//! kernels, each with a scalar oracle.
//!
//! [`H264Encoder`] is the same encoder as a `styx-codec-api` [`Encoder`](styx_codec_api::Encoder):
//! a raw frame in, one H.264 packet out, in the caller's output lease. [`candidates`] gives the
//! registry its candidates.
//!
//! Not yet: a motion search on the GPU (it waits on the GPU crate), deblocking, and anything past
//! Constrained Baseline.

mod bits;
mod cavlc;
mod encoder;
mod headers;
mod mb;
mod picture;
mod rate;
mod stream;
mod subpel;
mod tables;
mod transform;

pub use encoder::{DEFAULT_KEYFRAME_INTERVAL, DEFAULT_QP, H264Encoder, INPUTS, NAME, candidates};
pub use rate::RateTarget;
pub use stream::{StyxH264, StyxH264Input};
