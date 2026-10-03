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
//!
//! With [`NativeCamera::start_external_driven`] the caller's thread does the per-frame work of
//! the event thread (which then serves only the bridge and does not wake per frame): it calls
//! [`SensorStream::service`] whenever it wakes, and adds [`SensorStream::frame_start_fd`] to
//! what it waits on while that is `Some` (writes wait for a frame start). A pipeline thread
//! that waits on its own nodes anyway then wakes once per frame for the frame instead of the
//! event thread waking for the frame start too.

use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::Arc;

use styx_kernel::event::{EventType, Events, SubscribeFlags};
use styx_kernel::media::{LinkFlags, MediaDevice};
use styx_kernel::v4l2::VideoDevice;

use crate::camera::{Configured, NativeCamera, StreamSettings};
use crate::control::FrameControls;
use crate::device::CaptureDevice;
use crate::discover::find_media;
use crate::embedded::{EmbeddedCapture, embedded_link};
use crate::error::{KernelContext, Result};
use crate::events::drain_frame_starts;
use crate::health::Health;
use crate::stream::SensorSide;
use crate::topology::find_route;

/// The sensor side of a camera whose frames another device captures.
pub struct SensorStream {
    sensor: Arc<dyn SensorSide>,
    embedded: Option<Arc<EmbeddedCapture>>,
    frame_sync: bool,
    /// The frame-start node and the stream's health, when the caller drives frame starts.
    driven: Option<(Arc<VideoDevice>, Arc<Health>)>,
}

impl std::fmt::Debug for SensorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SensorStream")
            .field("embedded", &self.embedded.is_some())
            .field("frame_sync", &self.frame_sync)
            .field("driven", &self.driven.is_some())
            .finish()
    }
}

impl SensorStream {
    /// Frame `seq` was delivered. Without frame-start events this drives the control schedule
    /// (frame `seq + 1` is starting).
    pub fn frame_done(&self, seq: u64) {
        self.service_frame_starts();
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

    /// Driven streams ([`NativeCamera::start_external_driven`]): feeds the pending frame-start
    /// events to the control schedule (writing what is due) and reads the embedded data that
    /// arrived. Cheap when nothing is pending (one `DQEVENT` and one `DQBUF` that find
    /// nothing); call it whenever the thread wakes, and before making a sensor request (so
    /// the schedule knows how much of the current frame is left). No-op otherwise.
    pub fn service(&self) {
        if self.driven.is_none() {
            return;
        }
        self.service_frame_starts();
        if let Some(e) = &self.embedded {
            e.drain();
        }
    }

    fn service_frame_starts(&self) {
        if let Some((video, health)) = &self.driven
            && self.frame_sync
            && let Err(f) = drain_frame_starts(video.as_ref(), self.sensor.as_ref(), health)
        {
            health.fail(f);
        }
    }

    /// Driven streams: the descriptor to wait on for priority readiness (`POLLPRI`, a
    /// frame-start event) while requested values wait for a frame start to be written; then
    /// call [`Self::service`]. `None` when nothing waits for one (then frame starts are only
    /// read when the thread wakes anyway) or when the stream is not driven.
    pub fn frame_start_fd(&self) -> Option<BorrowedFd<'_>> {
        let (video, _) = self.driven.as_ref()?;
        (self.frame_sync && self.sensor.writes_pending()).then(|| video.event_fd())
    }

    /// Whether the caller drives frame starts ([`NativeCamera::start_external_driven`]).
    pub fn is_driven(&self) -> bool {
        self.driven.is_some()
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
        self.start_external_with(frame_sync, false)
    }

    /// [`Self::start_external`], with the caller doing the event thread's per-frame work (see
    /// the [module docs](self)): the event thread only serves the bridge's start and stop
    /// requests, and the caller calls [`SensorStream::service`] when it wakes and waits on
    /// [`SensorStream::frame_start_fd`] when that is `Some`.
    pub fn start_external_driven(&mut self, frame_sync: &Path) -> Result<SensorStream> {
        self.start_external_with(frame_sync, true)
    }

    fn start_external_with(&mut self, frame_sync: &Path, driven: bool) -> Result<SensorStream> {
        if self.configured.is_none() {
            return Err(crate::NativeError::State("configure before starting"));
        }
        let sensor: Arc<dyn SensorSide> = self.control.clone();
        self.session.set_embedded(None);
        let format = self.configured.as_ref().map(|c| c.mode.format.as_str());
        if self.options.embedded_data
            && format.is_some_and(|f| self.info.description.embedded_data_in(f))
        {
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
        let health = self.session.start_external(
            Arc::clone(&video) as Arc<dyn CaptureDevice>,
            sync,
            driven,
        )?;
        Ok(SensorStream {
            sensor,
            embedded: self.session.embedded().cloned(),
            frame_sync: sync,
            driven: driven.then_some((video, health)),
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
