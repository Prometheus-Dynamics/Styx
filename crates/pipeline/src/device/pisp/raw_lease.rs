//! A frame's raw front end buffer for a consumer of the raw frame ([`PispOptions::raw_lease`]):
//! it stays dequeued after the back end read it, until the consumer gives it back.

use std::os::fd::{AsFd, BorrowedFd};

use styx_softisp::CfaPattern;

use super::{PispOptions, PispPipeline};
use crate::error::Result;

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
    /// Exports the front end's raw buffers when they are leased.
    pub(super) fn export_raw(&mut self) -> Result<()> {
        self.raw_fds.clear();
        if let (true, Some(fe)) = (self.options.raw_lease, self.fe_dev.as_ref()) {
            self.raw_fds = fe.image_dmabufs()?;
        }
        Ok(())
    }

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
pub const RAW_LEASE_BUFFERS: u32 = 2;
