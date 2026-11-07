//! A frame's raw front end buffer for a consumer of the raw frame ([`PispOptions::raw_lease`]):
//! it stays dequeued after the back end read it, until the consumer gives it back.

use std::os::fd::{AsFd, BorrowedFd};

use styx_softisp::CfaPattern;

use super::{PispOptions, PispPipeline};
use crate::error::{PipelineError, Result};

/// How the front end stores a raw frame: 16-bit samples, the sensor's value in the top bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    pub cfa: CfaPattern,
    /// The sensor's bits per sample (in the top bits of each 16-bit sample).
    pub bits: u8,
}

impl PispPipeline {
    /// Raw buffer `index` as a dma-buf (with [`PispOptions::raw_lease`]).
    pub fn raw_dmabuf(&self, index: u32) -> Option<BorrowedFd<'_>> {
        Some(self.raw_fds.get(index as usize)?.as_fd())
    }

    /// The raw frames' layout.
    pub fn raw_layout(&self) -> Option<RawLayout> {
        let f = self.fe_dev.as_ref()?.image_format();
        Some(RawLayout {
            width: u32::from(f.width),
            height: u32::from(f.height),
            stride: f.stride.max(0) as u32,
            cfa: self.info.cfa,
            bits: self.info.bits,
        })
    }

    /// Gives raw buffer `index` ([`super::PispFrame::raw_image`]) back to the front end.
    pub fn release_raw(&mut self, index: u32) -> Result<()> {
        match self.fe_dev.as_ref() {
            Some(fe) => Ok(fe.release_image(index)?),
            None => Ok(()),
        }
    }

    /// Raw frames for consumers from now on, without a restart: the front end gets
    /// [`RAW_LEASE_BUFFERS`] more buffers, the back end takes them as inputs, and they are
    /// queued. They stay until the pipeline is opened again (the driver frees buffers only with
    /// the stream off). Not with compressed raw. Written, untested on board.
    pub fn enable_raw_lease(&mut self) -> Result<()> {
        if self.options.raw_lease {
            return Ok(());
        }
        if self.options.compressed_raw() {
            return Err(PipelineError::Config(
                "raw frames for consumers: the raw frames are compressed".into(),
            ));
        }
        let (Some(fe), Some(be)) = (self.fe_dev.as_mut(), self.be_dev.as_mut()) else {
            return Err(PipelineError::Config(
                "raw frames for consumers: not streaming".into(),
            ));
        };
        let first = u32::try_from(self.raw_fds.len()).unwrap_or(u32::MAX);
        let added = fe.add_image_buffers(RAW_LEASE_BUFFERS)?;
        let clones = added
            .iter()
            .map(|f| f.try_clone())
            .collect::<std::io::Result<Vec<_>>>()?;
        be.add_inputs(clones)?;
        for k in 0..added.len() as u32 {
            fe.release_image(first + k)?;
        }
        self.raw_fds.extend(added);
        self.options.raw_lease = true;
        // A reopen (a mode change) keeps them.
        self.options.fe_buffers += RAW_LEASE_BUFFERS;
        Ok(())
    }

    /// Whether frames keep their raw buffer for a consumer.
    pub fn leases_raw(&self) -> bool {
        self.options.raw_lease
    }
}

impl PispOptions {
    /// Raw front end buffers held by consumers ([`Self::raw_lease`]) besides the ones the
    /// stream needs: consumers get at most this many raw frames at once.
    pub const RAW_LEASE_BUFFERS: u32 = RAW_LEASE_BUFFERS;
}

/// See [`PispOptions::RAW_LEASE_BUFFERS`].
pub const RAW_LEASE_BUFFERS: u32 = 4;
