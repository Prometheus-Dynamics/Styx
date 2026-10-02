//! The sensor's embedded data, when the bridge has its embedded-data pad (`styx,embedded-data`)
//! and the receiver a node for it (`rp1-cfe-embedded`, CSI-2 channel 1): streamed alongside the
//! image, decoded with the description's layout, and reported to the control schedule, so the
//! values a frame carries are read back from the sensor instead of predicted.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use styx_kernel::media::{EntityFunction, Topology};
use styx_kernel::v4l2::{BufType, Format, Memory, MetaFormat, QueueBuffer, VideoDevice};
use styx_kernel::{FourCc, Mapping};

use crate::control::lock;
use crate::error::{KernelContext, Result};
use crate::stream::SensorSide;
use crate::topology::{LinkChange, RawRoute};

const META: BufType = BufType::MetaCapture;
/// `V4L2_META_FMT_SENSOR_DATA`.
const SENSOR_DATA: FourCc = FourCc::new(b"SENS");
/// Buffer size for one or a few embedded lines.
const BUFFER_SIZE: u32 = 16384;

/// The link that feeds the embedded node, when the sensor (bridge) has an embedded-data pad
/// feeding the receiver: the receiver's source → the embedded node.
pub(crate) fn embedded_link(topo: &Topology, route: &RawRoute) -> Option<LinkChange> {
    let node = route.embedded_node?;
    let bridge_has_pad = topo
        .links_from(route.sensor)
        .iter()
        .any(|l| l.sink.entity == route.receiver && l.source.index != route.sensor_pad);
    if !bridge_has_pad
        || topo
            .entity(node)
            .is_none_or(|e| e.function != EntityFunction::IO_V4L)
    {
        return None;
    }
    topo.links_to(node)
        .into_iter()
        .find(|l| l.source.entity == route.receiver)
        .map(|l| LinkChange {
            source: l.source,
            sink: l.sink,
            enable: true,
        })
}

/// The embedded data node, streaming alongside the image node.
///
/// Its buffers exist only while it streams: [`EmbeddedCapture::stop`] frees them, so a
/// handle kept alive by a stale frame stream does not keep the queue busy for the next open
/// (`S_FMT` fails with `EBUSY` on a queue with buffers).
pub(crate) struct EmbeddedCapture {
    video: VideoDevice,
    buffers: u32,
    maps: Mutex<Vec<Mapping>>,
    streaming: AtomicBool,
    sensor: Arc<dyn SensorSide>,
    pub(crate) reported: AtomicU64,
}

impl EmbeddedCapture {
    /// Opens the node and sets the sensor-data format; buffers come with [`Self::start`].
    pub(crate) fn open(path: &Path, buffers: u32, sensor: Arc<dyn SensorSide>) -> Result<Self> {
        let video = VideoDevice::open(path).step("open embedded node")?;
        video
            .set_format(
                META,
                &Format::Meta(MetaFormat {
                    fourcc: SENSOR_DATA,
                    buffer_size: BUFFER_SIZE,
                    ..Default::default()
                }),
            )
            .step("embedded format")?;
        Ok(Self {
            video,
            buffers: buffers.max(2),
            maps: Mutex::new(Vec::new()),
            streaming: AtomicBool::new(false),
            sensor,
            reported: AtomicU64::new(0),
        })
    }

    /// Allocates and queues the buffers and starts streaming (before the image node: the
    /// receiver starts once every node with an enabled link streams).
    pub(crate) fn start(&self) -> Result<()> {
        let mut maps = lock(&self.maps);
        let r = (|| {
            let got = self
                .video
                .request_buffers(META, Memory::Mmap, self.buffers)
                .step("embedded REQBUFS")?;
            for i in 0..got.count {
                let mut planes = self.video.map_buffer(META, i).step("map embedded buffer")?;
                if !planes.is_empty() {
                    maps.push(planes.remove(0));
                }
                self.video
                    .queue(&QueueBuffer::mmap(META, i))
                    .step("embedded QBUF")?;
            }
            self.video.stream_on(META).step("embedded STREAMON")
        })();
        if r.is_err() {
            maps.clear();
            let _ = self.video.free_buffers(META, Memory::Mmap);
            return r;
        }
        self.streaming.store(true, Ordering::Release);
        Ok(())
    }

    /// Dequeues every ready buffer, reports its values for its frame and queues it again.
    pub(crate) fn drain(&self) {
        if !self.streaming.load(Ordering::Acquire) {
            return;
        }
        let maps = lock(&self.maps);
        while let Ok(Some(buf)) = self.video.dequeue(META, Memory::Mmap) {
            if let Some(map) = maps.get(buf.index as usize) {
                let used = buf.bytes_used().min(map.len());
                if used > 0 {
                    self.sensor
                        .report_embedded(u64::from(buf.sequence), &map.as_slice()[..used]);
                    self.reported.fetch_add(1, Ordering::Relaxed);
                }
            }
            let _ = self.video.queue(&QueueBuffer::mmap(META, buf.index));
        }
    }

    /// Stops streaming and frees the buffers.
    pub(crate) fn stop(&self) {
        let mut maps = lock(&self.maps);
        if self.streaming.swap(false, Ordering::AcqRel) {
            let _ = self.video.stream_off(META);
        }
        maps.clear();
        let _ = self.video.free_buffers(META, Memory::Mmap);
    }
}

impl Drop for EmbeddedCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use styx_kernel::media::{Link, LinkFlags, Pad, PadFlags};

    use super::*;
    use crate::topology::{find_route, testing::cm5_topology};

    #[test]
    fn needs_the_bridge_embedded_pad() {
        let mut t = cm5_topology("ov9782 styx-sensor-bridge-cam0");
        let r = find_route(&t, 16).unwrap();
        // One pad on the bridge: no embedded data.
        assert!(embedded_link(&t, &r).is_none());
        // With the embedded pad (pad 1 -> csi2 sink 1), the csi2:5 -> embedded link is used.
        let src = 900;
        t.pads.push(Pad {
            id: src,
            entity_id: 16,
            flags: PadFlags::SOURCE,
            index: 1,
        });
        let csi_sink1 = t
            .pads
            .iter()
            .find(|p| p.entity_id == 1 && p.index == 1)
            .unwrap()
            .id;
        t.links.push(Link {
            id: 901,
            source_id: src,
            sink_id: csi_sink1,
            flags: LinkFlags::ENABLED | LinkFlags::IMMUTABLE,
        });
        let r = find_route(&t, 16).unwrap();
        let l = embedded_link(&t, &r).unwrap();
        assert_eq!((l.source.entity, l.source.index), (1, 5));
        assert_eq!((l.sink.entity, l.enable), (22, true));
    }
}
