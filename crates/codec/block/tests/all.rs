//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod sim;

#[path = "decode.rs"]
mod decode;
#[path = "h264_decode.rs"]
mod h264_decode;
#[path = "h264_streams.rs"]
mod h264_streams;
#[path = "h264_streams_sim.rs"]
mod h264_streams_sim;
#[path = "live.rs"]
mod live;
#[path = "race.rs"]
mod race;
