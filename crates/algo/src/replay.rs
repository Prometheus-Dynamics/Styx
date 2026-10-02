//! Record and replay statistics sequences.
//!
//! Format: JSON Lines. The first line is a header
//! `{"styx_algo_replay": 1, "config": <CameraConfig>, "warm": <WarmStart>?}`; every further line is one frame
//! `{"stats": <Statistics>, "meta": <FrameMetadata>, "params": <Params>}` where `params` (what
//! the pipeline produced when recording) is optional. Floats round-trip exactly, so replaying a
//! recording through the same algorithms and tuning reproduces `params` bit for bit.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::config::CameraConfig;
use crate::error::{AlgoError, Result};
use crate::frame::FrameMetadata;
use crate::params::Params;
use crate::pipeline::Pipeline;
use crate::stats::Statistics;
use crate::warm::WarmStart;

/// Format version written in the header.
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Header {
    styx_algo_replay: u32,
    config: CameraConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    warm: Option<WarmStart>,
}

/// One recorded frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Statistics.
    pub stats: Statistics,
    /// Frame metadata (including controls).
    pub meta: FrameMetadata,
    /// Output produced when recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Params>,
}

/// Writes a recording.
#[derive(Debug)]
pub struct Recorder<W: Write> {
    out: W,
}

impl<W: Write> Recorder<W> {
    /// Start a recording for a camera configuration.
    pub fn new(out: W, config: &CameraConfig) -> Result<Self> {
        Self::with_warm_start(out, config, None)
    }

    /// Start a recording for a camera configuration whose algorithms were prepared with a
    /// warm start ([`Pipeline::prepare_warm`]).
    pub fn with_warm_start(
        mut out: W,
        config: &CameraConfig,
        warm: Option<&WarmStart>,
    ) -> Result<Self> {
        let header = Header {
            styx_algo_replay: VERSION,
            config: config.clone(),
            warm: warm.copied(),
        };
        writeln!(out, "{}", to_json(&header)?)?;
        Ok(Self { out })
    }

    /// Record one frame.
    pub fn record(
        &mut self,
        stats: &Statistics,
        meta: &FrameMetadata,
        params: Option<&Params>,
    ) -> Result<()> {
        let rec = Record {
            stats: stats.clone(),
            meta: meta.clone(),
            params: params.cloned(),
        };
        writeln!(self.out, "{}", to_json(&rec)?)?;
        Ok(())
    }

    /// The writer.
    pub fn into_inner(self) -> W {
        self.out
    }
}

fn to_json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| AlgoError::Replay {
        line: 0,
        message: e.to_string(),
    })
}

/// A loaded recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// The camera configuration.
    pub config: CameraConfig,
    /// The warm start the algorithms were prepared with, if any.
    pub warm: Option<WarmStart>,
    /// The frames.
    pub records: Vec<Record>,
}

impl Recording {
    /// Read a recording.
    pub fn read(input: impl BufRead) -> Result<Self> {
        let mut lines = input.lines().enumerate();
        let bad = |line: usize, message: String| AlgoError::Replay {
            line: line + 1,
            message,
        };
        let (_, first) = lines.next().ok_or_else(|| bad(0, "empty file".into()))?;
        let header: Header = serde_json::from_str(&first?).map_err(|e| bad(0, e.to_string()))?;
        if header.styx_algo_replay != VERSION {
            return Err(bad(
                0,
                format!("unsupported version {}", header.styx_algo_replay),
            ));
        }
        let mut records = Vec::new();
        for (i, line) in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            records.push(serde_json::from_str(&line).map_err(|e| bad(i, e.to_string()))?);
        }
        Ok(Self {
            config: header.config,
            warm: header.warm,
            records,
        })
    }
}

/// What a replay produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayReport {
    /// Output per frame.
    pub outputs: Vec<Params>,
    /// Indices of frames whose output differs from the recorded one.
    pub mismatches: Vec<usize>,
}

/// Prepare the pipeline with the recording's configuration (and warm start) and run every
/// frame through it.
pub fn replay(pipeline: &mut Pipeline, recording: &Recording) -> Result<ReplayReport> {
    pipeline.prepare_warm(&recording.config, recording.warm.as_ref())?;
    let mut outputs = Vec::with_capacity(recording.records.len());
    let mut mismatches = Vec::new();
    for (i, r) in recording.records.iter().enumerate() {
        let p = pipeline.process(&r.stats, &r.meta).clone();
        if r.params.as_ref().is_some_and(|expected| *expected != p) {
            mismatches.push(i);
        }
        outputs.push(p);
    }
    Ok(ReplayReport {
        outputs,
        mismatches,
    })
}
