//! USB configuration and UVC class descriptors: the camera's units and terminals (with the
//! controls they offer), its streaming interfaces, formats, frame sizes and intervals, and the
//! endpoints of every alternate setting.

use crate::{Result, UvcError};

const DT_DEVICE: u8 = 0x01;
const DT_CONFIG: u8 = 0x02;
const DT_INTERFACE: u8 = 0x04;
const DT_ENDPOINT: u8 = 0x05;
const DT_SS_EP_COMPANION: u8 = 0x30;
const CS_INTERFACE: u8 = 0x24;

const CC_VIDEO: u8 = 0x0e;
const SC_VIDEOCONTROL: u8 = 0x01;
const SC_VIDEOSTREAMING: u8 = 0x02;

// VideoControl descriptor subtypes.
const VC_HEADER: u8 = 0x01;
const VC_INPUT_TERMINAL: u8 = 0x02;
const VC_OUTPUT_TERMINAL: u8 = 0x03;
const VC_SELECTOR_UNIT: u8 = 0x04;
const VC_PROCESSING_UNIT: u8 = 0x05;
const VC_EXTENSION_UNIT: u8 = 0x06;
const VC_ENCODING_UNIT: u8 = 0x07;

// VideoStreaming descriptor subtypes.
const VS_INPUT_HEADER: u8 = 0x01;
const VS_FORMAT_UNCOMPRESSED: u8 = 0x04;
const VS_FRAME_UNCOMPRESSED: u8 = 0x05;
const VS_FORMAT_MJPEG: u8 = 0x06;
const VS_FRAME_MJPEG: u8 = 0x07;
const VS_COLORFORMAT: u8 = 0x0d;
const VS_FORMAT_FRAME_BASED: u8 = 0x10;
const VS_FRAME_FRAME_BASED: u8 = 0x11;

/// `ITT_CAMERA`: an input terminal that is a camera sensor.
pub const ITT_CAMERA: u16 = 0x0201;

fn u16le(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32le(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A little-endian control bitmap of `size` bytes at `at`.
fn bitmap(b: &[u8], at: usize, size: usize) -> u64 {
    b.iter()
        .skip(at)
        .take(size.min(8))
        .enumerate()
        .fold(0, |acc, (i, &v)| acc | (u64::from(v) << (8 * i)))
}

/// The USB device descriptor's identity fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub max_packet_size0: u8,
    pub num_configurations: u8,
}

/// USB transfer type of an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

/// An endpoint of an alternate setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// `bEndpointAddress` (bit 7: IN).
    pub address: u8,
    pub transfer: TransferType,
    /// `wMaxPacketSize` bits 0-10.
    pub max_packet: u16,
    /// Transactions per (micro)frame: 1 + `wMaxPacketSize` bits 11-12 (high speed), or
    /// (burst + 1) × (mult + 1) from the SuperSpeed companion.
    pub transactions: u16,
    /// `wBytesPerInterval` of a SuperSpeed companion descriptor, when present.
    pub bytes_per_interval: Option<u16>,
    /// `bInterval`.
    pub interval: u8,
}

impl Endpoint {
    /// Whether it is an IN endpoint.
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// Bytes it can move per (micro)frame.
    pub fn bytes_per_interval(&self) -> usize {
        self.bytes_per_interval
            .map(usize::from)
            .unwrap_or(usize::from(self.max_packet) * usize::from(self.transactions.max(1)))
    }
}

/// One alternate setting of an interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AltSetting {
    pub alt: u8,
    pub endpoints: Vec<Endpoint>,
}

/// A terminal or unit of the VideoControl interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entity {
    InputTerminal {
        id: u8,
        terminal_type: u16,
        /// Camera terminal control bitmap (`bmControls`), 0 for other input terminals.
        controls: u64,
    },
    OutputTerminal {
        id: u8,
        terminal_type: u16,
        source: u8,
    },
    SelectorUnit {
        id: u8,
        sources: Vec<u8>,
    },
    ProcessingUnit {
        id: u8,
        source: u8,
        controls: u64,
    },
    ExtensionUnit {
        id: u8,
        guid: [u8; 16],
        num_controls: u8,
        controls: u64,
    },
    EncodingUnit {
        id: u8,
        source: u8,
    },
}

impl Entity {
    /// The terminal or unit id.
    pub fn id(&self) -> u8 {
        match self {
            Entity::InputTerminal { id, .. }
            | Entity::OutputTerminal { id, .. }
            | Entity::SelectorUnit { id, .. }
            | Entity::ProcessingUnit { id, .. }
            | Entity::ExtensionUnit { id, .. }
            | Entity::EncodingUnit { id, .. } => *id,
        }
    }
}

/// What a format carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatKind {
    /// Uncompressed pixels; the GUID's first four bytes (`YUY2`, `NV12`, ...).
    Uncompressed {
        guid: [u8; 16],
        bits_per_pixel: u8,
    },
    Mjpeg,
    /// Frame-based (H.264, ...): the GUID's first four bytes name it.
    FrameBased {
        guid: [u8; 16],
        variable_size: bool,
    },
}

/// Frame intervals of a frame descriptor, in 100 ns units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intervals {
    Discrete(Vec<u32>),
    Continuous { min: u32, max: u32, step: u32 },
}

impl Intervals {
    /// Every interval to advertise: the discrete list, or min, max and the default.
    pub fn listed(&self, default: u32) -> Vec<u32> {
        match self {
            Intervals::Discrete(v) => v.clone(),
            Intervals::Continuous { min, max, .. } => {
                let mut v = vec![*min, default, *max];
                v.sort_unstable();
                v.dedup();
                v
            }
        }
    }

    /// The supported interval closest to `wanted` (100 ns units).
    pub fn closest(&self, wanted: u32) -> Option<u32> {
        match self {
            Intervals::Discrete(v) => v.iter().copied().min_by_key(|&i| i.abs_diff(wanted)),
            Intervals::Continuous { min, max, step } => {
                // A device may describe the range backwards or with a step past its end.
                let (lo, hi) = (u64::from(*min.min(max)), u64::from(*min.max(max)));
                let w = u64::from(wanted).clamp(lo, hi);
                let step = u64::from(*step).max(1);
                let n = (w - lo + step / 2) / step;
                Some((lo + n * step).min(hi) as u32)
            }
        }
    }
}

/// One frame size of a format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// `bFrameIndex` (1-based).
    pub index: u8,
    pub width: u16,
    pub height: u16,
    /// `dwMaxVideoFrameBufferSize` (0 for frame-based formats, which do not have one).
    pub max_frame_size: u32,
    /// `dwDefaultFrameInterval`, 100 ns units.
    pub default_interval: u32,
    pub intervals: Intervals,
    /// `dwBytesPerLine` of a frame-based frame.
    pub bytes_per_line: u32,
}

/// The colour description of a format (`VS_COLORFORMAT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorFormat {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

/// One format of a streaming interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    /// `bFormatIndex` (1-based).
    pub index: u8,
    pub kind: FormatKind,
    /// `bDefaultFrameIndex`.
    pub default_frame: u8,
    pub frames: Vec<Frame>,
    pub color: Option<ColorFormat>,
}

impl Format {
    /// The V4L2 FourCC Styx uses for it (`YUYV`, `NV12`, `MJPG`, `H264`, ...), as bytes.
    pub fn fourcc(&self) -> Option<[u8; 4]> {
        match self.kind {
            FormatKind::Mjpeg => Some(*b"MJPG"),
            FormatKind::Uncompressed { guid, .. } | FormatKind::FrameBased { guid, .. } => {
                let g = [guid[0], guid[1], guid[2], guid[3]];
                Some(match &g {
                    b"YUY2" => *b"YUYV",
                    b"Y800" => *b"GREY",
                    b"I420" => *b"YU12",
                    b"Y16 " => *b"Y16 ",
                    b"RGBP" => *b"RGBP",
                    b"MJPG" => *b"MJPG",
                    _ if g.iter().all(|b| b.is_ascii_graphic() || *b == b' ') => g,
                    _ => return None,
                })
            }
        }
    }

    /// A frame by index.
    pub fn frame(&self, index: u8) -> Option<&Frame> {
        self.frames.iter().find(|f| f.index == index)
    }

    /// Bytes of a complete uncompressed frame of `frame`, when fixed.
    pub fn frame_bytes(&self, frame: &Frame) -> Option<usize> {
        match self.kind {
            FormatKind::Uncompressed { bits_per_pixel, .. } => Some(
                usize::from(frame.width) * usize::from(frame.height) * usize::from(bits_per_pixel)
                    / 8,
            ),
            _ => None,
        }
    }
}

/// A VideoStreaming interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingInterface {
    pub number: u8,
    /// `bEndpointAddress` from the input header: the video data endpoint.
    pub endpoint: u8,
    /// `bTerminalLink`: the output terminal it streams from.
    pub terminal_link: u8,
    pub formats: Vec<Format>,
    /// Alternate settings with their endpoints (alt 0 first).
    pub alts: Vec<AltSetting>,
}

impl StreamingInterface {
    /// A format by index.
    pub fn format(&self, index: u8) -> Option<&Format> {
        self.formats.iter().find(|f| f.index == index)
    }

    /// The transfer type of the video endpoint (bulk if any setting has it as bulk).
    pub fn transfer(&self) -> TransferType {
        self.alts
            .iter()
            .flat_map(|a| &a.endpoints)
            .find(|e| e.address == self.endpoint)
            .map_or(TransferType::Isochronous, |e| e.transfer)
    }

    /// The alternate setting with the least bandwidth that still carries `payload` bytes per
    /// (micro)frame, or the largest one if none does. `None` without isochronous settings.
    pub fn alt_for_bandwidth(&self, payload: usize) -> Option<(u8, Endpoint)> {
        let mut candidates: Vec<(u8, Endpoint)> = self
            .alts
            .iter()
            .filter_map(|a| {
                let ep = a.endpoints.iter().find(|e| {
                    e.address == self.endpoint && e.transfer == TransferType::Isochronous
                })?;
                Some((a.alt, *ep))
            })
            .collect();
        candidates.sort_by_key(|(_, e)| e.bytes_per_interval());
        candidates
            .iter()
            .find(|(_, e)| e.bytes_per_interval() >= payload)
            .or(candidates.last())
            .copied()
    }
}

/// The VideoControl interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlInterface {
    pub number: u8,
    /// `bcdUVC` (0x0100, 0x0110, 0x0150).
    pub uvc_version: u16,
    /// `dwClockFrequency`: the device clock of PTS and SCR, in Hz.
    pub clock_frequency: u32,
    pub entities: Vec<Entity>,
    /// The interrupt endpoint for status events, if any.
    pub status_endpoint: Option<u8>,
}

impl ControlInterface {
    /// The camera terminal: its id and control bitmap.
    pub fn camera_terminal(&self) -> Option<(u8, u64)> {
        self.entities.iter().find_map(|e| match e {
            Entity::InputTerminal {
                id,
                terminal_type: ITT_CAMERA,
                controls,
            } => Some((*id, *controls)),
            _ => None,
        })
    }

    /// The first processing unit: its id and control bitmap.
    pub fn processing_unit(&self) -> Option<(u8, u64)> {
        self.entities.iter().find_map(|e| match e {
            Entity::ProcessingUnit { id, controls, .. } => Some((*id, *controls)),
            _ => None,
        })
    }
}

/// One UVC function of a device: its control interface and streaming interfaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UvcFunction {
    pub device: DeviceDescriptor,
    /// `bConfigurationValue` of the configuration it was found in.
    pub configuration: u8,
    pub control: ControlInterface,
    pub streaming: Vec<StreamingInterface>,
}

impl UvcFunction {
    /// Parses the device and configuration descriptors (as read from usbfs or sysfs
    /// `descriptors`) and returns the first video function.
    pub fn parse(raw: &[u8]) -> Result<UvcFunction> {
        let mut device = DeviceDescriptor::default();
        let mut configuration = 0;
        let mut control: Option<ControlInterface> = None;
        let mut streaming: Vec<StreamingInterface> = Vec::new();
        // The interface (number, subclass, alt) the descriptors at hand belong to.
        let mut current: Option<(u8, u8, u8)> = None;
        let mut i = 0;
        while i + 2 <= raw.len() {
            let len = usize::from(raw[i]);
            if len < 2 || i + len > raw.len() {
                return Err(UvcError::Descriptor(format!(
                    "descriptor of length {len} at byte {i} of {}",
                    raw.len()
                )));
            }
            let d = &raw[i..i + len];
            i += len;
            match d[1] {
                DT_DEVICE if len >= 18 => {
                    device = DeviceDescriptor {
                        bcd_usb: u16le(d, 2),
                        max_packet_size0: d[7],
                        vendor_id: u16le(d, 8),
                        product_id: u16le(d, 10),
                        bcd_device: u16le(d, 12),
                        num_configurations: d[17],
                    };
                }
                DT_CONFIG if len >= 9 => {
                    // Only the first configuration's function.
                    if control.is_some() {
                        break;
                    }
                    configuration = d[5];
                    current = None;
                }
                DT_INTERFACE if len >= 9 => {
                    let (number, alt, class, subclass) = (d[2], d[3], d[5], d[6]);
                    current = (class == CC_VIDEO).then_some((number, subclass, alt));
                    if class == CC_VIDEO && subclass == SC_VIDEOCONTROL && control.is_none() {
                        control = Some(ControlInterface {
                            number,
                            uvc_version: 0,
                            clock_frequency: 0,
                            entities: Vec::new(),
                            status_endpoint: None,
                        });
                    }
                    if class == CC_VIDEO && subclass == SC_VIDEOSTREAMING {
                        let vs = match streaming.iter_mut().find(|s| s.number == number) {
                            Some(vs) => vs,
                            None => {
                                streaming.push(StreamingInterface {
                                    number,
                                    endpoint: 0,
                                    terminal_link: 0,
                                    formats: Vec::new(),
                                    alts: Vec::new(),
                                });
                                streaming.last_mut().expect("just pushed")
                            }
                        };
                        vs.alts.push(AltSetting {
                            alt,
                            endpoints: Vec::new(),
                        });
                    }
                }
                DT_ENDPOINT if len >= 7 => {
                    let ep = parse_endpoint(d);
                    match current {
                        Some((n, SC_VIDEOSTREAMING, _)) => {
                            if let Some(alt) = streaming
                                .iter_mut()
                                .find(|s| s.number == n)
                                .and_then(|s| s.alts.last_mut())
                            {
                                alt.endpoints.push(ep);
                            }
                        }
                        Some((_, SC_VIDEOCONTROL, _)) => {
                            if let Some(c) = control.as_mut() {
                                c.status_endpoint = Some(ep.address);
                            }
                        }
                        _ => {}
                    }
                }
                DT_SS_EP_COMPANION if len >= 6 => {
                    if let Some((n, SC_VIDEOSTREAMING, _)) = current
                        && let Some(ep) = streaming
                            .iter_mut()
                            .find(|s| s.number == n)
                            .and_then(|s| s.alts.last_mut())
                            .and_then(|a| a.endpoints.last_mut())
                    {
                        let burst = u16::from(d[2]) + 1;
                        let mult = if ep.transfer == TransferType::Isochronous {
                            u16::from(d[3] & 0x3) + 1
                        } else {
                            1
                        };
                        ep.transactions = burst * mult;
                        ep.bytes_per_interval = Some(u16le(d, 4));
                    }
                }
                CS_INTERFACE if len >= 3 => match current {
                    Some((_, SC_VIDEOCONTROL, _)) => {
                        if let Some(c) = control.as_mut() {
                            parse_vc(c, d);
                        }
                    }
                    Some((n, SC_VIDEOSTREAMING, _)) => {
                        if let Some(vs) = streaming.iter_mut().find(|s| s.number == n) {
                            parse_vs(vs, d);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        let control =
            control.ok_or_else(|| UvcError::Descriptor("no video control interface".into()))?;
        streaming.retain(|s| !s.formats.is_empty());
        if streaming.is_empty() {
            return Err(UvcError::Descriptor(
                "no video streaming interface with formats".into(),
            ));
        }
        Ok(UvcFunction {
            device,
            configuration,
            control,
            streaming,
        })
    }
}

fn parse_endpoint(d: &[u8]) -> Endpoint {
    let wmax = u16le(d, 4);
    let transfer = match d[3] & 0x3 {
        0 => TransferType::Control,
        1 => TransferType::Isochronous,
        2 => TransferType::Bulk,
        _ => TransferType::Interrupt,
    };
    let transactions = if matches!(
        transfer,
        TransferType::Isochronous | TransferType::Interrupt
    ) {
        ((wmax >> 11) & 0x3) + 1
    } else {
        1
    };
    Endpoint {
        address: d[2],
        transfer,
        max_packet: wmax & 0x7ff,
        transactions,
        bytes_per_interval: None,
        interval: d[6],
    }
}

fn parse_vc(c: &mut ControlInterface, d: &[u8]) {
    let len = d.len();
    match d[2] {
        VC_HEADER if len >= 12 => {
            c.uvc_version = u16le(d, 3);
            c.clock_frequency = u32le(d, 7);
        }
        VC_INPUT_TERMINAL if len >= 8 => {
            let terminal_type = u16le(d, 4);
            let controls = if terminal_type == ITT_CAMERA && len >= 15 {
                bitmap(d, 15, usize::from(d[14]))
            } else {
                0
            };
            c.entities.push(Entity::InputTerminal {
                id: d[3],
                terminal_type,
                controls,
            });
        }
        VC_OUTPUT_TERMINAL if len >= 9 => c.entities.push(Entity::OutputTerminal {
            id: d[3],
            terminal_type: u16le(d, 4),
            source: d[7],
        }),
        VC_SELECTOR_UNIT if len >= 5 => {
            let n = usize::from(d[4]);
            c.entities.push(Entity::SelectorUnit {
                id: d[3],
                sources: d.iter().skip(5).take(n).copied().collect(),
            });
        }
        VC_PROCESSING_UNIT if len >= 8 => c.entities.push(Entity::ProcessingUnit {
            id: d[3],
            source: d[4],
            controls: bitmap(d, 8, usize::from(d[7])),
        }),
        VC_EXTENSION_UNIT if len >= 22 => {
            let pins = usize::from(d[21]);
            let size_at = 22 + pins;
            let size = d.get(size_at).copied().map_or(0, usize::from);
            let mut guid = [0u8; 16];
            guid.copy_from_slice(&d[4..20]);
            c.entities.push(Entity::ExtensionUnit {
                id: d[3],
                guid,
                num_controls: d[20],
                controls: bitmap(d, size_at + 1, size),
            });
        }
        VC_ENCODING_UNIT if len >= 5 => c.entities.push(Entity::EncodingUnit {
            id: d[3],
            source: d[4],
        }),
        _ => {}
    }
}

/// `bFrameIntervalType` at `kind_at`, the intervals from `first`.
fn parse_intervals(d: &[u8], kind_at: usize, first: usize) -> Intervals {
    let n = d.get(kind_at).copied().unwrap_or(0);
    if n == 0 {
        Intervals::Continuous {
            min: u32le(d, first),
            max: u32le(d, first + 4),
            step: u32le(d, first + 8),
        }
    } else {
        Intervals::Discrete(
            (0..usize::from(n))
                .map(|k| first + 4 * k)
                .filter(|&at| at + 4 <= d.len())
                .map(|at| u32le(d, at))
                .filter(|&v| v > 0)
                .collect(),
        )
    }
}

fn parse_vs(vs: &mut StreamingInterface, d: &[u8]) {
    let len = d.len();
    match d[2] {
        VS_INPUT_HEADER if len >= 9 => {
            vs.endpoint = d[6];
            vs.terminal_link = d[8];
        }
        VS_FORMAT_UNCOMPRESSED | VS_FORMAT_FRAME_BASED if len >= 23 => {
            let mut guid = [0u8; 16];
            guid.copy_from_slice(&d[5..21]);
            let kind = if d[2] == VS_FORMAT_UNCOMPRESSED {
                FormatKind::Uncompressed {
                    guid,
                    bits_per_pixel: d[21],
                }
            } else {
                FormatKind::FrameBased {
                    guid,
                    variable_size: d.get(27).is_some_and(|&v| v != 0),
                }
            };
            vs.formats.push(Format {
                index: d[3],
                kind,
                default_frame: d[22],
                frames: Vec::new(),
                color: None,
            });
        }
        VS_FORMAT_MJPEG if len >= 7 => vs.formats.push(Format {
            index: d[3],
            kind: FormatKind::Mjpeg,
            default_frame: d[6],
            frames: Vec::new(),
            color: None,
        }),
        VS_FRAME_UNCOMPRESSED | VS_FRAME_MJPEG if len >= 26 => {
            if let Some(f) = vs.formats.last_mut() {
                f.frames.push(Frame {
                    index: d[3],
                    width: u16le(d, 5),
                    height: u16le(d, 7),
                    max_frame_size: u32le(d, 17),
                    default_interval: u32le(d, 21),
                    intervals: parse_intervals(d, 25, 26),
                    bytes_per_line: 0,
                });
            }
        }
        VS_FRAME_FRAME_BASED if len >= 26 => {
            if let Some(f) = vs.formats.last_mut() {
                f.frames.push(Frame {
                    index: d[3],
                    width: u16le(d, 5),
                    height: u16le(d, 7),
                    max_frame_size: 0,
                    default_interval: u32le(d, 17),
                    intervals: parse_intervals(d, 21, 26),
                    bytes_per_line: u32le(d, 22),
                });
            }
        }
        VS_COLORFORMAT if len >= 6 => {
            if let Some(f) = vs.formats.last_mut() {
                f.color = Some(ColorFormat {
                    primaries: d[3],
                    transfer: d[4],
                    matrix: d[5],
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "descriptors_tests.rs"]
mod tests;
