//! The sensor's embedded data through `rp1-cfe-embedded` (CSI-2 channel 1), when the bridge
//! has its embedded data pad (`styx,embedded-data`, the `-emb` runtime overlay): the node, its
//! link from `csi2` source pad 5, and per-frame buffers matched to image frames by sequence.

use std::collections::VecDeque;
use std::path::Path;

use styx_kernel::media::{LinkFlags, MediaDevice, PadRef, Topology};
use styx_kernel::v4l2::{BufType, Format, Memory, MetaFormat, QueueBuffer, VideoDevice};
use styx_kernel::{FourCc, Mapping};

use crate::{Result, ResultExt, log};

const META: BufType = BufType::MetaCapture;
/// The embedded data node.
pub const NODE: &str = "rp1-cfe-embedded";
/// `V4L2_META_FMT_SENSOR_DATA`.
const SENSOR_DATA: FourCc = FourCc::new(b"SENS");
/// Bytes kept per frame (the start of the buffer, where the register values are).
pub const KEEP: usize = 256;

/// One embedded data buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedFrame {
    /// Frame sequence.
    pub sequence: u32,
    /// Bytes the receiver wrote.
    pub used: usize,
    /// The first [`KEEP`] bytes.
    pub head: Vec<u8>,
}

/// The embedded data node, streaming alongside the image node.
pub struct EmbeddedNode {
    video: VideoDevice,
    maps: Vec<Mapping>,
    streaming: bool,
    /// Buffers seen, oldest first (bounded).
    pub frames: VecDeque<EmbeddedFrame>,
}

/// Enables `csi2` source pad `source` → the embedded node, if the bridge feeds `csi2` sink pad
/// `source - 4` (only with the embedded data pad). Returns the node's device path.
pub fn enable_link(media: &MediaDevice, topo: &Topology, receiver: u32) -> Result<std::path::PathBuf> {
    let node = topo
        .entity_by_name(NODE)
        .ok_or_else(|| format!("no {NODE} entity"))?;
    let fed = topo
        .links_to(receiver)
        .iter()
        .any(|l| l.sink.index == 1);
    if !fed {
        return Err("nothing feeds csi2 pad 1: the bridge has no embedded data pad (use the -emb overlay)".into());
    }
    media
        .setup_link(
            PadRef {
                entity: receiver,
                index: 5,
            },
            PadRef {
                entity: node.id,
                index: 0,
            },
            LinkFlags::ENABLED,
        )
        .ctx("enable csi2:5 -> embedded")?;
    topo.devnode_path(node.id)
        .ok_or_else(|| format!("no device node for {NODE}"))
}

impl EmbeddedNode {
    /// Opens the node, sets the sensor-data format and queues `buffers` buffers.
    pub fn open(path: &Path, buffers: u32) -> Result<Self> {
        let video = VideoDevice::open(path).ctx("open embedded node")?;
        let set = video
            .set_format(
                META,
                &Format::Meta(MetaFormat {
                    fourcc: SENSOR_DATA,
                    buffer_size: 16384,
                    ..Default::default()
                }),
            )
            .ctx("embedded format")?;
        let got = video
            .request_buffers(META, Memory::Mmap, buffers)
            .ctx("embedded REQBUFS")?;
        let mut maps = Vec::new();
        for i in 0..got.count {
            maps.push(video.map_buffer(META, i).ctx("mmap")?.remove(0));
            video.queue(&QueueBuffer::mmap(META, i)).ctx("QBUF")?;
        }
        log!("embedded: {} {set:?}, {} buffers", path.display(), got.count);
        Ok(Self {
            video,
            maps,
            streaming: false,
            frames: VecDeque::new(),
        })
    }

    /// Starts streaming (before the image node: rp1-cfe starts once all enabled nodes stream).
    pub fn start(&mut self) -> Result<()> {
        self.video.stream_on(META).ctx("embedded STREAMON")?;
        self.streaming = true;
        Ok(())
    }

    /// Dequeues every ready buffer.
    pub fn poll(&mut self) -> Result<()> {
        while let Some(buf) = self.video.dequeue(META, Memory::Mmap).ctx("embedded DQBUF")? {
            let map = &self.maps[buf.index as usize];
            let used = buf.bytes_used().min(map.len());
            let head = map.as_slice()[..used.min(KEEP)].to_vec();
            self.video
                .queue(&QueueBuffer::mmap(META, buf.index))
                .ctx("embedded QBUF")?;
            if self.frames.len() >= 64 {
                self.frames.pop_front();
            }
            self.frames.push_back(EmbeddedFrame {
                sequence: buf.sequence,
                used,
                head,
            });
        }
        Ok(())
    }

    /// The buffer of frame `seq`, if seen.
    pub fn frame(&self, seq: u32) -> Option<&EmbeddedFrame> {
        self.frames.iter().find(|f| f.sequence == seq)
    }
}

impl Drop for EmbeddedNode {
    fn drop(&mut self) {
        if self.streaming && let Err(e) = self.video.stream_off(META) {
            log!("embedded STREAMOFF: {e}");
        }
        self.maps.clear();
        if let Err(e) = self.video.free_buffers(META, Memory::Mmap) {
            log!("embedded: freeing buffers: {e}");
        }
    }
}

/// Hex dump of the first bytes, 32 per line.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .chunks(32)
        .map(|c| c.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n    ")
}
