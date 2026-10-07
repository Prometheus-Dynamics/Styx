//! Styx frames in Daedalus graphs (feature `daedalus`).
//!
//! The one place that tells Daedalus about [`FrameLease`]: every crate that turns on
//! `styx-core-rs/daedalus` gets the same type key, descriptor and registration.
//!
//! - [`FRAME_TYPE_KEY`] (`styx:framelease`): the key of every frame payload and of the frame type.
//! - [`FrameDescriptor`]: the frame as graphs, editors and host inspection see it (format, size,
//!   planes, timestamp, residency, CPU access, companions), never the pixels.
//! - [`StyxFramesPlugin`]: registers the frame type, the descriptor (with its nested plane and
//!   companion types), a metadata-only adapter from frame to descriptor, and a value serializer
//!   so host inspection shows frames as their descriptor. Install it once per registry.
//! - [`frame_payload`] / [`shared_frame_payload`]: a frame as a Daedalus [`Payload`] without
//!   copying it, with its residency mapped ([`payload_residency`]).
//! - [`push_group`] / [`group_payloads`]: a multi-camera [`FrameGroup`](crate::multicam::FrameGroup)
//!   pushed as one atomic host batch, so the graph ticks once per synchronized set, with its
//!   [`FrameGroupInfo`] (`styx:frame_group`: spread, offsets, cameras present).
//! - `FrameLease: FrameSource`: Daedalus's generic `daedalus:frame` v2 view ([`FrameView`]),
//!   registered as a provider by the plugin, so nodes that know nothing of Styx (separately
//!   built plugins included) read a Styx frame in place: DRM format, modifier and format kind
//!   ([`crate::format::drm`]), the planes' dma-bufs, offsets, strides and lengths without
//!   touching the pixels, and the lease's own bytes through bracketed CPU reads
//!   ([`FrameLease::begin_cpu_read`]: mapped on first use, dma-bufs synced while read), never
//!   a host pointer the CPU cannot read. [`frame_view`] views a lease directly.
//!
//! ```ignore
//! let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
//! registry.install(&styx_core::daedalus::StyxFramesPlugin::new())?;
//! // Nodes take `&FrameLease`, `FrameView<'_>`, or `&FrameDescriptor` through the adapters.
//! host.push_payload("frame", styx_core::daedalus::frame_payload(frame));
//! ```

use std::sync::Arc;

use ::daedalus::data::to_value::ToValue;
use ::daedalus::runtime::plugins::{PluginRegistry, PluginResult};
use ::daedalus::transport::{ForeignBorrow, Payload, Residency, TransportError};
pub use ::daedalus::transport::{FrameFormatKind, FrameInterface, FrameView, PlaneMapping};
use ::daedalus::{DaedalusToValue, DaedalusTypeExpr, adapt, plugin};

use crate::buffer::{CompanionKind, CpuAccess, FrameLease, FrameResidency, PlaneLayout};

/// Type key of Styx frames in Daedalus: every frame payload carries it. A public contract.
pub const FRAME_TYPE_KEY: &str = "styx:framelease";

/// Type key of [`FrameDescriptor`].
pub const DESCRIPTOR_TYPE_KEY: &str = "styx:frame_descriptor";

impl ::daedalus::data::daedalus_type::DaedalusTypeExpr for FrameLease {
    const TYPE_KEY: &'static str = FRAME_TYPE_KEY;

    fn type_expr() -> ::daedalus::data::model::TypeExpr {
        ::daedalus::data::model::TypeExpr::Opaque(FRAME_TYPE_KEY.to_string())
    }
}

/// A frame as graphs see it: everything but the pixels.
#[derive(Clone, Debug, PartialEq, DaedalusTypeExpr, DaedalusToValue)]
#[daedalus(type_key = "styx:frame_descriptor")]
pub struct FrameDescriptor {
    /// Pixel format as a FourCC (`NV12`, `GREY`, `RG24`, `H264`, ...).
    pub format: String,
    pub width: u32,
    pub height: u32,
    /// Colour space (`srgb`, `bt709`, `bt2020`, `unknown`).
    pub color: String,
    /// Capture timestamp in nanoseconds, in `clock`.
    pub timestamp_ns: u64,
    /// Clock of `timestamp_ns` (`monotonic`, `boottime`, `realtime`, `stream_relative`), when
    /// known.
    pub clock: Option<String>,
    /// An inter-coded packet (H.264/H.265): it needs the packets before it.
    pub delta: bool,
    /// When the frame is a region-of-interest view: the region in full-frame pixels.
    pub crop: Option<RegionDescriptor>,
    pub planes: Vec<PlaneDescriptor>,
    /// Where the memory lives (`host_owned`, `host_external`, `dmabuf`, `gpu_texture`,
    /// `compressed_packet`).
    pub residency: String,
    /// Whether and how fast the CPU reads the planes (`cached`, `uncached`, `none`).
    pub cpu_access: String,
    /// Companion frames attached to this one (pyramid levels, a second ISP output).
    pub companions: Vec<CompanionDescriptor>,
}

/// One plane of a frame.
#[derive(Clone, Debug, PartialEq, DaedalusTypeExpr, DaedalusToValue)]
#[daedalus(type_key = "styx:plane_descriptor")]
pub struct PlaneDescriptor {
    /// Bytes from the start of the plane's buffer to its first row.
    pub offset: u64,
    /// Bytes of the plane.
    pub len: u64,
    /// Bytes from one row to the next.
    pub stride: u64,
}

/// A rectangle in full-frame pixels.
#[derive(Clone, Debug, PartialEq, DaedalusTypeExpr, DaedalusToValue)]
#[daedalus(type_key = "styx:region")]
pub struct RegionDescriptor {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A companion frame: the same capture at another size.
#[derive(Clone, Debug, PartialEq, DaedalusTypeExpr, DaedalusToValue)]
#[daedalus(type_key = "styx:companion_descriptor")]
pub struct CompanionDescriptor {
    /// `pyramid` (downscaled by 2^`level`), `scaled` (another output's size), `overview`
    /// (the whole frame, when this one is a region of it) or `region` (another region of
    /// interest, its index in `level`).
    pub kind: String,
    /// Pyramid level (1 = ½, 2 = ¼, ...), or the region's index; 0 otherwise.
    pub level: u32,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub planes: Vec<PlaneDescriptor>,
    pub residency: String,
    pub cpu_access: String,
}

fn planes(layouts: &[PlaneLayout]) -> Vec<PlaneDescriptor> {
    layouts
        .iter()
        .map(|l| PlaneDescriptor {
            offset: l.offset as u64,
            len: l.len as u64,
            stride: l.stride as u64,
        })
        .collect()
}

fn snake(debug: impl std::fmt::Debug) -> String {
    let name = format!("{debug:?}");
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

impl FrameDescriptor {
    /// The descriptor of `frame`, from its metadata only (the pixels are not touched).
    pub fn of(frame: &FrameLease) -> Self {
        let meta = frame.meta();
        let format = meta.format;
        FrameDescriptor {
            format: format.code.to_string(),
            width: format.resolution.width.get(),
            height: format.resolution.height.get(),
            color: snake(format.color),
            timestamp_ns: meta.timestamp,
            clock: meta.clock.map(snake),
            delta: meta.delta,
            crop: meta.crop.map(|c| RegionDescriptor {
                x: c.x,
                y: c.y,
                width: c.width,
                height: c.height,
            }),
            planes: planes(frame.layout_slice()),
            residency: frame.residency().to_string(),
            cpu_access: frame.cpu_access().to_string(),
            companions: frame
                .companions()
                .map(|(kind, companion)| {
                    let format = companion.meta().format;
                    let (kind, level) = match kind {
                        CompanionKind::Pyramid { level } => ("pyramid", u32::from(level)),
                        CompanionKind::Scaled => ("scaled", 0),
                        CompanionKind::Overview => ("overview", 0),
                        CompanionKind::Region { index } => ("region", u32::from(index)),
                    };
                    CompanionDescriptor {
                        kind: kind.into(),
                        level,
                        format: format.code.to_string(),
                        width: format.resolution.width.get(),
                        height: format.resolution.height.get(),
                        planes: planes(companion.layout_slice()),
                        residency: companion.residency().to_string(),
                        cpu_access: companion.cpu_access().to_string(),
                    }
                })
                .collect(),
        }
    }
}

/// The Daedalus residency of `frame`'s memory: frames owning host memory (and compressed packets)
/// are `Cpu`; memory owned elsewhere, such as dma-bufs or driver and memfd buffers, `External`;
/// GPU textures `Gpu`.
pub fn payload_residency(frame: &FrameLease) -> Residency {
    match frame.residency() {
        FrameResidency::HostOwned | FrameResidency::CompressedPacket => Residency::Cpu,
        FrameResidency::HostExternal | FrameResidency::Dmabuf => Residency::External,
        FrameResidency::GpuTexture => Residency::Gpu,
    }
}

/// `frame` as a Daedalus payload, without copying it: the payload owns the frame (and with it
/// its buffer) until the graph drops it.
pub fn frame_payload(frame: FrameLease) -> Payload {
    shared_frame_payload(Arc::new(frame))
}

/// [`frame_payload`] for a frame already shared (e.g. one also kept by the caller). CPU writes
/// still open on the frame's memory are finished first, so devices reading it by descriptor
/// see them.
pub fn shared_frame_payload(frame: Arc<FrameLease>) -> Payload {
    frame.finish_cpu_write();
    let residency = payload_residency(&frame);
    let bytes = frame.payload_bytes() as u64;
    Payload::shared_with(FRAME_TYPE_KEY, frame, residency, None, Some(bytes))
}

/// The planner inserts this on an edge from a frame port to a descriptor port: it reads the frame's
/// metadata, never its pixels.
#[adapt(
    id = "styx.frame_descriptor",
    kind = ::daedalus::transport::AdapterKind::MetadataOnly
)]
fn frame_descriptor(frame: &FrameLease) -> Result<FrameDescriptor, TransportError> {
    Ok(FrameDescriptor::of(frame))
}

/// Host payload inspection shows frames as their descriptor instead of an opaque summary.
fn install(registry: &mut PluginRegistry) -> PluginResult<()> {
    registry
        .register_value_serializer::<FrameLease, _>(|frame| FrameDescriptor::of(frame).to_value());
    Ok(())
}

/// Registers Styx frames with a Daedalus registry: the frame type ([`FRAME_TYPE_KEY`]), the
/// descriptor types, the metadata adapter, the `daedalus:frame` provider (a `View` adapter,
/// `daedalus.foreign:styx:framelease->daedalus:frame`, which the planner inserts before ports
/// taking [`FrameView`]) and the inspection serializer. Install it once:
/// `registry.install(&StyxFramesPlugin::new())`.
#[plugin(
    id = "styx.frames",
    install = install,
    crate_build,
    types(FrameLease),
    values(FrameDescriptor, FrameGroupInfo),
    adapters(frame_descriptor),
    foreign_providers(FrameLease => FrameInterface)
)]
pub struct StyxFramesPlugin;

/// Whether the CPU can read `frame` ([`CpuAccess`]), for nodes deciding to read in place.
pub fn cpu_readable(frame: &FrameLease) -> bool {
    frame.cpu_access() != CpuAccess::None
}

/// `frame` through `daedalus:frame` v2, borrowed (no handle, no allocation), as a node taking
/// [`FrameView`] sees it: for hosts and tests calling such code directly.
pub fn frame_view(frame: &FrameLease) -> FrameView<'_> {
    match ForeignBorrow::of::<FrameLease, FrameInterface>(frame).view() {
        Ok(view) => view,
        // The provider and the view are this build's one copy of the interface.
        Err(_) => unreachable!("daedalus:frame differs from itself"),
    }
}

mod frame_view;
pub use frame_view::{format_kind, plane_mapping, view_residency};

mod group;
pub use group::{FrameGroupInfo, GROUP_INFO_TYPE_KEY, GroupPorts, group_payloads, push_group};

#[cfg(test)]
mod group_tests;
#[cfg(test)]
mod tests;
