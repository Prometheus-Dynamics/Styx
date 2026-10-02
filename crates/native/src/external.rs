//! Running the sensor side of a camera while another device owns the receiver's capture
//! nodes, e.g. an ISP front end (`pisp-fe` in `rp1-cfe`) that streams raw frames and
//! statistics itself.
//!
//! ```text
//! configure_external   power, mode, bridge format and timing (no links, no video node)
//! (the other device)   links, receiver pads, its nodes and buffers
//! start_external       embedded data (link, node, STREAMON), event thread (quiesced)
//! (the other device)   STREAMON: the receiver starts, the bridge asks, the sensor starts
//! resume_external      fails if the sensor did not start; frame starts flow
//! (per frame)          SensorStream::applied(seq): what produced the frame
//! quiesce_external     before the other device's STREAMOFF (the receiver holds the node's
//!                      lock while the bridge waits for the stop acknowledgement)
//! (the other device)   STREAMOFF: the bridge asks to stop
//! stop / close         as for the raw route
//! ```

use std::path::Path;
use std::sync::Arc;

use styx_kernel::event::{EventType, Events, SubscribeFlags};
use styx_kernel::media::{LinkFlags, MediaDevice};
use styx_kernel::v4l2::VideoDevice;

use crate::camera::{Configured, NativeCamera, StreamSettings};
use crate::control::FrameControls;
use crate::discover::find_media;
use crate::embedded::{EmbeddedCapture, embedded_link};
use crate::error::{KernelContext, Result};
use crate::stream::SensorSide;
use crate::topology::find_route;

/// The sensor side of a camera whose frames another device captures.
pub struct SensorStream {
    sensor: Arc<dyn SensorSide>,
    embedded: Option<Arc<EmbeddedCapture>>,
    frame_sync: bool,
}

impl std::fmt::Debug for SensorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SensorStream")
            .field("embedded", &self.embedded.is_some())
            .field("frame_sync", &self.frame_sync)
            .finish()
    }
}

impl SensorStream {
    /// Frame `seq` was delivered. Without frame-start events this drives the control schedule
    /// (frame `seq + 1` is starting).
    pub fn frame_done(&self, seq: u64) {
        if !self.frame_sync {
            let _ = self.sensor.frame_start(seq + 1, None);
        }
        if let Some(e) = &self.embedded {
            e.drain();
        }
    }

    /// The values that produced frame `seq` (read back from its embedded data when that has
    /// arrived, predicted otherwise).
    pub fn applied(&self, seq: u64) -> Option<FrameControls> {
        if let Some(e) = &self.embedded {
            e.drain();
        }
        self.sensor.applied(seq)
    }

    /// Whether frame starts come from the receiver's `FRAME_SYNC` events.
    pub fn uses_frame_sync(&self) -> bool {
        self.frame_sync
    }

    /// Whether the sensor's embedded data is captured.
    pub fn has_embedded_data(&self) -> bool {
        self.embedded.is_some()
    }
}

impl NativeCamera {
    /// Powers and configures the sensor and the bridge for `settings`, leaving the receiver's
    /// links, pads and capture nodes to another device (see the [module docs](self)). The
    /// configuration's pixel format is `settings.fourcc` (or the zero fourcc), with no stride.
    pub fn configure_external(&mut self, settings: &StreamSettings) -> Result<Configured> {
        let (mode, frame_length) = self.configure_sensor(settings)?;
        self.session.set_embedded(None);
        let configured = Configured {
            interval: crate::modes::frame_interval(&mode.timing, frame_length),
            mode,
            fourcc: settings.fourcc.unwrap_or(styx_kernel::FourCc(0)),
            stride: 0,
            size_image: 0,
            frame_length,
        };
        self.configured = Some(configured.clone());
        Ok(configured)
    }

    /// Starts serving the sensor side for another device: captures the embedded data
    /// (enabling its link) when the bridge and description provide it, takes frame starts from
    /// `frame_sync` (the other device's capture node that sends `FRAME_SYNC` events, e.g.
    /// `rp1-cfe-fe_image0`, opened here separately) and answers the bridge. Call before the
    /// other device starts streaming, and [`Self::resume_external`] after.
    pub fn start_external(&mut self, frame_sync: &Path) -> Result<SensorStream> {
        if self.configured.is_none() {
            return Err(crate::NativeError::State("configure before starting"));
        }
        let sensor: Arc<dyn SensorSide> = self.control.clone();
        self.session.set_embedded(None);
        if self.options.embedded_data && self.info.description.embedded_data.is_some() {
            let media = MediaDevice::open(&self.info.media).step("open media device")?;
            let topo = media.topology().step("media topology")?;
            let (_, _, entity) = find_media(&self.info.location)?;
            let route = find_route(&topo, entity)?;
            if let Some(link) = embedded_link(&topo, &route)
                && let Some(path) = route.embedded_node.and_then(|n| topo.devnode_path(n))
            {
                media
                    .setup_link(link.source, link.sink, LinkFlags::ENABLED)
                    .step("enable the embedded data link")?;
                self.session
                    .set_embedded(Some(Arc::new(EmbeddedCapture::open(
                        &path,
                        4,
                        Arc::clone(&sensor),
                    )?)));
            }
        }
        let video = Arc::new(VideoDevice::open(frame_sync).step("open the frame-start node")?);
        let sync = self.options.frame_sync
            && video
                .subscribe(EventType::FrameSync, 0, SubscribeFlags::empty())
                .is_ok();
        self.session.start_external(video, sync)?;
        Ok(SensorStream {
            sensor,
            embedded: self.session.embedded().cloned(),
            frame_sync: sync,
        })
    }

    /// The other device started streaming: fails if the sensor did not start, else frame
    /// starts flow.
    pub fn resume_external(&self) -> Result<()> {
        self.session.resume_external()
    }

    /// The other device is about to stop streaming: the event thread leaves its node alone
    /// (the receiver holds the node's lock in `STREAMOFF` while the bridge waits for the
    /// stop acknowledgement). Then stop the other device, then [`Self::stop`].
    pub fn quiesce_external(&self) {
        self.session.quiesce_external();
    }
}
