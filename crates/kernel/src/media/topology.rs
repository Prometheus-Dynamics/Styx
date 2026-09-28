//! The media graph (`MEDIA_IOC_G_TOPOLOGY`): entities, interfaces, pads and links, with
//! helpers to follow links and find device nodes.

use std::fmt;
use std::path::PathBuf;

use super::raw;
use crate::flags::flags;
use crate::ioctl::cstr_field;

/// An entity's main function (`MEDIA_ENT_F_*`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EntityFunction(pub u32);

impl EntityFunction {
    /// Unknown.
    pub const UNKNOWN: Self = Self(0);
    /// A V4L2 subdevice of unknown function.
    pub const V4L2_SUBDEV_UNKNOWN: Self = Self(0x0002_0000);
    /// A V4L2 video/metadata device node (DMA engine).
    pub const IO_V4L: Self = Self(0x0001_0001);
    /// Camera sensor.
    pub const CAM_SENSOR: Self = Self(0x0002_0001);
    /// Flash.
    pub const FLASH: Self = Self(0x0002_0002);
    /// Lens.
    pub const LENS: Self = Self(0x0002_0003);
    /// Video composer.
    pub const PROC_VIDEO_COMPOSER: Self = Self(0x4001);
    /// Pixel formatter.
    pub const PROC_VIDEO_PIXEL_FORMATTER: Self = Self(0x4002);
    /// Pixel encoding converter.
    pub const PROC_VIDEO_PIXEL_ENC_CONV: Self = Self(0x4003);
    /// Look-up table.
    pub const PROC_VIDEO_LUT: Self = Self(0x4004);
    /// Scaler.
    pub const PROC_VIDEO_SCALER: Self = Self(0x4005);
    /// Statistics.
    pub const PROC_VIDEO_STATISTICS: Self = Self(0x4006);
    /// Encoder.
    pub const PROC_VIDEO_ENCODER: Self = Self(0x4007);
    /// Decoder.
    pub const PROC_VIDEO_DECODER: Self = Self(0x4008);
    /// ISP.
    pub const PROC_VIDEO_ISP: Self = Self(0x4009);
    /// Video multiplexer.
    pub const VID_MUX: Self = Self(0x5001);
    /// Video interface bridge (e.g. a CSI-2 receiver).
    pub const VID_IF_BRIDGE: Self = Self(0x5002);

    /// A short name for the function (the `MEDIA_ENT_F_` suffix).
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0 => "UNKNOWN",
            0x0002_0000 => "V4L2_SUBDEV_UNKNOWN",
            0x0000_0001 => "DTV_DEMOD",
            0x0000_0002 => "TS_DEMUX",
            0x0000_0003 => "DTV_CA",
            0x0000_0004 => "DTV_NET_DECAP",
            0x0001_0001 => "IO_V4L",
            0x0000_1001 => "IO_DTV",
            0x0000_1002 => "IO_VBI",
            0x0000_1003 => "IO_SWRADIO",
            0x0002_0001 => "CAM_SENSOR",
            0x0002_0002 => "FLASH",
            0x0002_0003 => "LENS",
            0x0002_0004 => "ATV_DECODER",
            0x0002_0005 => "TUNER",
            0x0000_2001 => "IF_VID_DECODER",
            0x0000_2002 => "IF_AUD_DECODER",
            0x0000_3001 => "AUDIO_CAPTURE",
            0x0000_3002 => "AUDIO_PLAYBACK",
            0x0000_3003 => "AUDIO_MIXER",
            0x4001 => "PROC_VIDEO_COMPOSER",
            0x4002 => "PROC_VIDEO_PIXEL_FORMATTER",
            0x4003 => "PROC_VIDEO_PIXEL_ENC_CONV",
            0x4004 => "PROC_VIDEO_LUT",
            0x4005 => "PROC_VIDEO_SCALER",
            0x4006 => "PROC_VIDEO_STATISTICS",
            0x4007 => "PROC_VIDEO_ENCODER",
            0x4008 => "PROC_VIDEO_DECODER",
            0x4009 => "PROC_VIDEO_ISP",
            0x5001 => "VID_MUX",
            0x5002 => "VID_IF_BRIDGE",
            0x6001 => "DV_DECODER",
            0x6002 => "DV_ENCODER",
            _ => return None,
        })
    }
}

impl fmt::Debug for EntityFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "{:#x}", self.0),
        }
    }
}

impl fmt::Display for EntityFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

flags! {
    /// Entity flags (`MEDIA_ENT_FL_*`).
    pub struct EntityFlags: u32 {
        const DEFAULT = 1 << 0;
        const CONNECTOR = 1 << 1;
    }
}

flags! {
    /// Pad flags (`MEDIA_PAD_FL_*`).
    pub struct PadFlags: u32 {
        const SINK = 1 << 0;
        const SOURCE = 1 << 1;
        const MUST_CONNECT = 1 << 2;
    }
}

flags! {
    /// Link flags (`MEDIA_LNK_FL_*`); the top four bits are the link type.
    pub struct LinkFlags: u32 {
        const ENABLED = 1 << 0;
        const IMMUTABLE = 1 << 1;
        const DYNAMIC = 1 << 2;
        const INTERFACE_LINK = 1 << 28;
        const ANCILLARY_LINK = 2 << 28;
    }
}

/// The kind of a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkType {
    /// Pad to pad: data flows from source to sink.
    Data,
    /// Interface to entity: the interface (device node) controls the entity.
    Interface,
    /// Entity to entity (e.g. sensor to its lens or flash).
    Ancillary,
    /// Unknown type bits.
    Other(u32),
}

impl LinkFlags {
    /// The link type encoded in the flags.
    pub fn link_type(self) -> LinkType {
        match (self.0 >> 28) & 0xf {
            0 => LinkType::Data,
            1 => LinkType::Interface,
            2 => LinkType::Ancillary,
            t => LinkType::Other(t),
        }
    }
}

/// An interface type (`MEDIA_INTF_T_*`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct InterfaceType(pub u32);

impl InterfaceType {
    /// V4L2 video node (`/dev/videoN`).
    pub const V4L_VIDEO: Self = Self(0x200);
    /// V4L2 subdevice node (`/dev/v4l-subdevN`).
    pub const V4L_SUBDEV: Self = Self(0x203);

    /// A short name (the `MEDIA_INTF_T_` suffix).
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0x100 => "DVB_FE",
            0x101 => "DVB_DEMUX",
            0x102 => "DVB_DVR",
            0x103 => "DVB_CA",
            0x104 => "DVB_NET",
            0x200 => "V4L_VIDEO",
            0x201 => "V4L_VBI",
            0x202 => "V4L_RADIO",
            0x203 => "V4L_SUBDEV",
            0x204 => "V4L_SWRADIO",
            0x205 => "V4L_TOUCH",
            0x300 => "ALSA_PCM_CAPTURE",
            0x301 => "ALSA_PCM_PLAYBACK",
            0x302 => "ALSA_CONTROL",
            _ => return None,
        })
    }
}

impl fmt::Debug for InterfaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "{:#x}", self.0),
        }
    }
}

impl fmt::Display for InterfaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A device node's character device number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DevNode {
    /// Major number.
    pub major: u32,
    /// Minor number.
    pub minor: u32,
}

impl DevNode {
    /// The `/dev` path of the node, from `/sys/dev/char/<major>:<minor>/uevent` (`DEVNAME`).
    /// `None` when sysfs has no such device.
    pub fn path(self) -> Option<PathBuf> {
        let uevent = std::fs::read_to_string(format!(
            "/sys/dev/char/{}:{}/uevent",
            self.major, self.minor
        ))
        .ok()?;
        devname_from_uevent(&uevent).map(|name| PathBuf::from("/dev").join(name))
    }
}

pub(crate) fn devname_from_uevent(uevent: &str) -> Option<&str> {
    uevent
        .lines()
        .find_map(|l| l.strip_prefix("DEVNAME="))
        .map(str::trim)
}

/// An entity: a hardware block or device node in the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    /// Graph object id.
    pub id: u32,
    /// Name (e.g. `ov9782 10-0060`, `csi2`, `rp1-cfe-csi2-ch0`).
    pub name: String,
    /// Main function.
    pub function: EntityFunction,
    /// Flags.
    pub flags: EntityFlags,
}

/// An interface: how userspace reaches entities (a device node).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    /// Graph object id.
    pub id: u32,
    /// Type.
    pub intf_type: InterfaceType,
    /// Flags.
    pub flags: u32,
    /// The character device, for device-node interfaces.
    pub devnode: Option<DevNode>,
}

/// A pad: a data connection point of an entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pad {
    /// Graph object id.
    pub id: u32,
    /// The entity it belongs to.
    pub entity_id: u32,
    /// Flags (sink or source).
    pub flags: PadFlags,
    /// Index of the pad within its entity (the number the subdev API uses).
    pub index: u32,
}

/// A link between two pads (data), an interface and an entity, or two entities (ancillary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Graph object id.
    pub id: u32,
    /// Source pad id (data links), interface id (interface links) or entity id (ancillary).
    pub source_id: u32,
    /// Sink pad id (data links) or entity id (interface and ancillary links).
    pub sink_id: u32,
    /// Flags.
    pub flags: LinkFlags,
}

/// One end of a data link, resolved to entity and pad index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PadRef {
    /// Entity id.
    pub entity: u32,
    /// Pad index within the entity.
    pub index: u32,
}

/// A data link resolved to entities and pad indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataLink {
    /// Link id.
    pub id: u32,
    /// Source end.
    pub source: PadRef,
    /// Sink end.
    pub sink: PadRef,
    /// Flags.
    pub flags: LinkFlags,
}

/// A snapshot of a media graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    /// Version of the graph (changes whenever it changes).
    pub version: u64,
    /// Entities.
    pub entities: Vec<Entity>,
    /// Interfaces.
    pub interfaces: Vec<Interface>,
    /// Pads.
    pub pads: Vec<Pad>,
    /// Links.
    pub links: Vec<Link>,
}

impl Topology {
    /// Builds a topology from the arrays `MEDIA_IOC_G_TOPOLOGY` fills.
    pub(crate) fn from_raw(
        version: u64,
        entities: &[raw::media_v2_entity],
        interfaces: &[raw::media_v2_interface],
        pads: &[raw::media_v2_pad],
        links: &[raw::media_v2_link],
    ) -> Self {
        Self {
            version,
            entities: entities
                .iter()
                .map(|e| Entity {
                    id: e.id,
                    name: cstr_field(&{ e.name }),
                    function: EntityFunction(e.function),
                    flags: EntityFlags(e.flags),
                })
                .collect(),
            interfaces: interfaces
                .iter()
                .map(|i| {
                    let intf_type = InterfaceType(i.intf_type);
                    let raw = i.raw;
                    // All V4L, DVB and ALSA interfaces are device nodes.
                    let devnode = (0x100..0x400).contains(&intf_type.0).then_some(DevNode {
                        major: raw[0],
                        minor: raw[1],
                    });
                    Interface {
                        id: i.id,
                        intf_type,
                        flags: i.flags,
                        devnode,
                    }
                })
                .collect(),
            pads: pads
                .iter()
                .map(|p| Pad {
                    id: p.id,
                    entity_id: p.entity_id,
                    flags: PadFlags(p.flags),
                    index: p.index,
                })
                .collect(),
            links: links
                .iter()
                .map(|l| Link {
                    id: l.id,
                    source_id: l.source_id,
                    sink_id: l.sink_id,
                    flags: LinkFlags(l.flags),
                })
                .collect(),
        }
    }

    /// The entity with graph id `id`.
    pub fn entity(&self, id: u32) -> Option<&Entity> {
        self.entities.iter().find(|e| e.id == id)
    }

    /// The first entity named `name`.
    pub fn entity_by_name(&self, name: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| e.name == name)
    }

    /// The pad with graph id `id`.
    pub fn pad(&self, id: u32) -> Option<&Pad> {
        self.pads.iter().find(|p| p.id == id)
    }

    /// The pads of an entity, by index.
    pub fn pads_of(&self, entity_id: u32) -> Vec<&Pad> {
        let mut pads: Vec<&Pad> = self
            .pads
            .iter()
            .filter(|p| p.entity_id == entity_id)
            .collect();
        pads.sort_by_key(|p| p.index);
        pads
    }

    /// All data links, resolved to entities and pad indices.
    pub fn data_links(&self) -> Vec<DataLink> {
        self.links
            .iter()
            .filter(|l| l.flags.link_type() == LinkType::Data)
            .filter_map(|l| {
                let src = self.pad(l.source_id)?;
                let sink = self.pad(l.sink_id)?;
                Some(DataLink {
                    id: l.id,
                    source: PadRef {
                        entity: src.entity_id,
                        index: src.index,
                    },
                    sink: PadRef {
                        entity: sink.entity_id,
                        index: sink.index,
                    },
                    flags: l.flags,
                })
            })
            .collect()
    }

    /// Data links that start from `entity_id`.
    pub fn links_from(&self, entity_id: u32) -> Vec<DataLink> {
        self.data_links()
            .into_iter()
            .filter(|l| l.source.entity == entity_id)
            .collect()
    }

    /// Data links that end at `entity_id`.
    pub fn links_to(&self, entity_id: u32) -> Vec<DataLink> {
        self.data_links()
            .into_iter()
            .filter(|l| l.sink.entity == entity_id)
            .collect()
    }

    /// The interface (device node) linked to an entity, if any.
    pub fn interface_of(&self, entity_id: u32) -> Option<&Interface> {
        self.links
            .iter()
            .filter(|l| l.flags.link_type() == LinkType::Interface && l.sink_id == entity_id)
            .find_map(|l| self.interfaces.iter().find(|i| i.id == l.source_id))
    }

    /// The entity an interface controls, if any.
    pub fn entity_of_interface(&self, interface_id: u32) -> Option<&Entity> {
        self.links
            .iter()
            .filter(|l| l.flags.link_type() == LinkType::Interface && l.source_id == interface_id)
            .find_map(|l| self.entity(l.sink_id))
    }

    /// The `/dev` path of the device node of an entity, via its interface and sysfs.
    pub fn devnode_path(&self, entity_id: u32) -> Option<PathBuf> {
        self.interface_of(entity_id)?.devnode?.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v4l2::raw::zeroed;

    fn entity(id: u32, name: &str, function: u32) -> raw::media_v2_entity {
        let mut e: raw::media_v2_entity = zeroed();
        e.id = id;
        let mut n = [0u8; 64];
        n[..name.len()].copy_from_slice(name.as_bytes());
        e.name = n;
        e.function = function;
        e
    }

    fn pad(id: u32, entity_id: u32, flags: u32, index: u32) -> raw::media_v2_pad {
        let mut p: raw::media_v2_pad = zeroed();
        p.id = id;
        p.entity_id = entity_id;
        p.flags = flags;
        p.index = index;
        p
    }

    fn link(id: u32, source_id: u32, sink_id: u32, flags: u32) -> raw::media_v2_link {
        let mut l: raw::media_v2_link = zeroed();
        l.id = id;
        l.source_id = source_id;
        l.sink_id = sink_id;
        l.flags = flags;
        l
    }

    /// sensor(1) pad0 --> csi2(2) pad0; csi2 pad1 --> video(3) pad0; interface 10 -> video.
    fn sample() -> Topology {
        let entities = [
            entity(1, "ov9782 10-0060", 0x0002_0001),
            entity(2, "csi2", 0x5002),
            entity(3, "rp1-cfe-csi2-ch0", 0x0001_0001),
        ];
        let mut intf: raw::media_v2_interface = zeroed();
        intf.id = 10;
        intf.intf_type = 0x200;
        let mut r = [0u32; 16];
        r[0] = 81;
        r[1] = 5;
        intf.raw = r;
        let pads = [
            pad(20, 1, 2, 0),
            pad(21, 2, 1, 0),
            pad(22, 2, 2, 1),
            pad(23, 3, 1, 0),
        ];
        let links = [
            link(30, 20, 21, 0x3),
            link(31, 22, 23, 0x1),
            link(32, 10, 3, (1 << 28) | 0x3),
        ];
        Topology::from_raw(7, &entities, &[intf], &pads, &links)
    }

    #[test]
    fn parses_and_resolves_links() {
        let t = sample();
        assert_eq!(t.version, 7);
        assert_eq!(
            t.entity_by_name("csi2").unwrap().function,
            EntityFunction::VID_IF_BRIDGE
        );
        assert_eq!(t.entity(1).unwrap().function.to_string(), "CAM_SENSOR");
        let data = t.data_links();
        assert_eq!(data.len(), 2);
        assert_eq!(
            data[0].source,
            PadRef {
                entity: 1,
                index: 0
            }
        );
        assert_eq!(
            data[0].sink,
            PadRef {
                entity: 2,
                index: 0
            }
        );
        assert!(data[0].flags.contains(LinkFlags::IMMUTABLE));
        assert_eq!(
            t.links_from(2)[0].sink,
            PadRef {
                entity: 3,
                index: 0
            }
        );
        assert_eq!(t.links_to(2).len(), 1);
        assert_eq!(t.pads_of(2).len(), 2);
        let intf = t.interface_of(3).unwrap();
        assert_eq!(intf.intf_type, InterfaceType::V4L_VIDEO);
        assert_eq!(
            intf.devnode,
            Some(DevNode {
                major: 81,
                minor: 5
            })
        );
        assert_eq!(t.entity_of_interface(10).unwrap().id, 3);
        assert!(t.interface_of(1).is_none());
        assert_eq!(LinkFlags(1 << 28).link_type(), LinkType::Interface);
    }

    #[test]
    fn uevent_devname() {
        let uevent = "MAJOR=81\nMINOR=5\nDEVNAME=video5\n";
        assert_eq!(devname_from_uevent(uevent), Some("video5"));
        assert_eq!(devname_from_uevent("MAJOR=1\n"), None);
    }
}
