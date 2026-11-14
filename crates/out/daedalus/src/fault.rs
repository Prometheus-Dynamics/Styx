//! Styx's own fault codes, for the faults of Styx's steps.
//!
//! A code is a number in the node's own code space: Daedalus carries it and does not read it, so a
//! caller matches on the kind of fault without parsing the text. Each value is published once and
//! never changes or is reused; a retired code keeps its number. [`TABLE`] lists every code with its
//! name and meaning, and the table in the docs is checked against it by the test suite
//! (`tests/fault_codes.rs`), so the enum, the table and the docs cannot drift apart.
//!
//! The reason a step gives is a headline of up to 120 bytes. The underlying error, from Lemnos,
//! Orion, Daedalus or the GPU, goes in the step's detail instead, with its own words complete and
//! its numeric kind named when it has one.

use core::fmt;

use daedalus_abi::{FixedError, StepCx, StepEnd};
use styx_format::{FourCc, MediaFormat, plane_sizes};
use styx_frame::BufferMemory;

use crate::value::StyxFrame;

/// A Styx fault's code. The table of codes, with what each means, is below; the numbers are
/// published and fixed.
#[doc = include_str!("fault_table.md")]
#[repr(u32)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaultCode {
    /// A fault Styx has no code for. Reserved for an error with no kind of its own.
    Other = 0,
    /// The shared-memory layout of the other side is not this build's.
    LayoutRefused = 1,
    /// Orion refused a revision or a feature.
    OrionRefused = 2,
    /// The buffer's bytes are needed on the CPU, and it is not mapped for the CPU.
    StreamNotMapped = 3,
    /// The buffer must be written, and its mapping is read-only.
    StreamReadOnly = 4,
    /// No GPU adapter, because this user is refused the GPU's device nodes.
    NoGpuAccess = 5,
    /// A device failed: it did not open, refused or failed a job, or is gone.
    DeviceFailed = 6,
    /// A frame with neither a dma-buf nor CPU bytes.
    NoDataPath = 7,
    /// A frame in a pixel format the step does not take.
    UnsupportedFormat = 8,
    /// A buffer too small for the planes its frame needs.
    PlaneLayoutMismatch = 9,
    /// Orion refused a revision: the node's revision is not one this build can use.
    OrionRevision = 10,
    /// Orion refused a feature level: the feature needs a level the node does not provide.
    OrionFeature = 11,
}

impl FaultCode {
    /// Every code, in the order of [`TABLE`].
    pub const ALL: &'static [FaultCode] = &[
        FaultCode::Other,
        FaultCode::LayoutRefused,
        FaultCode::OrionRefused,
        FaultCode::StreamNotMapped,
        FaultCode::StreamReadOnly,
        FaultCode::NoGpuAccess,
        FaultCode::DeviceFailed,
        FaultCode::NoDataPath,
        FaultCode::UnsupportedFormat,
        FaultCode::PlaneLayoutMismatch,
        FaultCode::OrionRevision,
        FaultCode::OrionFeature,
    ];
}

/// Every code: the code, its name (the variant's name), and its meaning. The docs' table is this
/// table; `tests/fault_codes.rs` checks both against the enum.
pub const TABLE: &[(FaultCode, &str, &str)] = &[
    (
        FaultCode::Other,
        "Other",
        "A fault Styx has no code for. The detail says what it was. Never given by a Styx check.",
    ),
    (
        FaultCode::LayoutRefused,
        "LayoutRefused",
        "The shared-memory layout of the other side is not this build's, so the attach is refused. The detail names the expected and found layout words.",
    ),
    (
        FaultCode::OrionRefused,
        "OrionRefused",
        "Orion refused a revision or a feature. The detail names the revision and the feature as Orion says them.",
    ),
    (
        FaultCode::StreamNotMapped,
        "StreamNotMapped",
        "A step needed the buffer's bytes on the CPU, and the buffer is not mapped for the CPU (GPU memory, or a dma-buf not mapped here).",
    ),
    (
        FaultCode::StreamReadOnly,
        "StreamReadOnly",
        "A step needed to write the buffer, and its mapping is read-only (or it has none that writes).",
    ),
    (
        FaultCode::NoGpuAccess,
        "NoGpuAccess",
        "No GPU adapter was found, and this user is refused the GPU's device nodes. The detail names the nodes and the group that owns them.",
    ),
    (
        FaultCode::DeviceFailed,
        "DeviceFailed",
        "A device failed: no device opened, a job was refused or failed, or the device is gone. The detail carries the device's own words.",
    ),
    (
        FaultCode::NoDataPath,
        "NoDataPath",
        "A frame has neither a dma-buf nor CPU bytes, so there is nothing a device or a step can read.",
    ),
    (
        FaultCode::UnsupportedFormat,
        "UnsupportedFormat",
        "A step takes one pixel format and the frame is another. The detail names both.",
    ),
    (
        FaultCode::PlaneLayoutMismatch,
        "PlaneLayoutMismatch",
        "The buffer is smaller than the frame's planes need for its format and size, or the planes can't be laid out. The detail gives the sizes.",
    ),
    (
        FaultCode::OrionRevision,
        "OrionRevision",
        "Orion refused a node's revision. The detail names the node and the revision it is, and the revision this build is.",
    ),
    (
        FaultCode::OrionFeature,
        "OrionFeature",
        "Orion refused a feature at the level it is asked for. The detail names the node, the feature and the level it needs.",
    ),
];

/// A Styx step's fault: its code, a short reason, and the detail, which is the underlying error's
/// words in full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyxFault {
    pub code: FaultCode,
    pub reason: &'static str,
    pub detail: String,
}

impl StyxFault {
    pub fn new(code: FaultCode, reason: &'static str, detail: impl Into<String>) -> StyxFault {
        StyxFault {
            code,
            reason,
            detail: detail.into(),
        }
    }

    /// A fault with no code of its own, for an error that has no kind.
    pub fn other(reason: &'static str, detail: impl Into<String>) -> StyxFault {
        StyxFault::new(FaultCode::Other, reason, detail)
    }
}

impl FixedError for StyxFault {
    fn code(&self) -> u32 {
        self.code as u32
    }

    fn reason(&self) -> &str {
        self.reason
    }
}

impl fmt::Display for StyxFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} ({}): {}; {}",
            self.code, self.code as u32, self.reason, self.detail
        )
    }
}

/// Returns a step's fault: the detail is written first, then the code and reason. The step returns
/// [`StepEnd::Fault`] from this.
pub fn fail(cx: &mut StepCx, at: u32, fault: &StyxFault) -> StepEnd {
    if !fault.detail.is_empty() {
        cx.detail(&fault.detail);
    }
    cx.fail(at, fault);
    StepEnd::Fault
}

/// The bytes of a buffer are needed on the CPU: it must be mapped for the CPU.
pub fn check_bytes(memory: &dyn BufferMemory) -> Result<(), StyxFault> {
    if memory.cpu_ptr().is_some() {
        return Ok(());
    }
    Err(StyxFault::new(
        FaultCode::StreamNotMapped,
        "the buffer is not mapped for the CPU",
        format!(
            "the buffer is {:?} memory of {} bytes, and it has no CPU mapping here",
            memory.kind(),
            memory.len()
        ),
    ))
}

/// The buffer must be written by the CPU: its mapping must allow it.
pub fn check_writable(memory: &dyn BufferMemory) -> Result<(), StyxFault> {
    if memory.cpu_writable() {
        return Ok(());
    }
    Err(StyxFault::new(
        FaultCode::StreamReadOnly,
        "the buffer's mapping is read-only",
        format!(
            "the buffer is {:?} memory of {} bytes, mapped for reading only",
            memory.kind(),
            memory.len()
        ),
    ))
}

/// A frame must have something a device or a step can read: a dma-buf, or CPU bytes.
pub fn check_frame(frame: &StyxFrame) -> Result<(), StyxFault> {
    use daedalus_dmabuf::DmaBufBacked;
    if frame.dmabuf().is_some() || frame.bytes().is_some() {
        return Ok(());
    }
    Err(StyxFault::new(
        FaultCode::NoDataPath,
        "the frame has neither a dma-buf nor CPU bytes",
        format!(
            "frame {} is in {:?} memory, which is not a dma-buf and is not mapped for the CPU",
            frame.id(),
            frame.memory_kind()
        ),
    ))
}

/// The step takes frames of `want`, and this frame is another format.
pub fn check_format(format: &MediaFormat, want: FourCc) -> Result<(), StyxFault> {
    if format.code == want {
        return Ok(());
    }
    Err(StyxFault::new(
        FaultCode::UnsupportedFormat,
        "the step does not take this pixel format",
        format!("the step takes {want}, and the frame is {}", format.code),
    ))
}

/// A buffer of `len` bytes must hold the planes its frame needs, laid out as the format says.
pub fn check_planes(len: usize, format: &MediaFormat) -> Result<(), StyxFault> {
    let width = format.resolution.width.get();
    let height = format.resolution.height.get();
    let sizes = plane_sizes(format.code, width, height, None, None).map_err(|error| {
        StyxFault::new(
            FaultCode::PlaneLayoutMismatch,
            "the frame's planes can't be laid out",
            format!("{} at {width}x{height}: {error:?}", format.code),
        )
    })?;
    let need: usize = sizes.iter().map(|plane| plane.len).sum();
    if len >= need {
        return Ok(());
    }
    Err(StyxFault::new(
        FaultCode::PlaneLayoutMismatch,
        "the buffer is smaller than the frame's planes",
        format!(
            "{} at {width}x{height} needs {need} bytes in its planes ({:?}), and the buffer holds {len}",
            format.code,
            sizes.iter().map(|plane| plane.len).collect::<Vec<_>>()
        ),
    ))
}

/// The fault for an attach refused by layout: the other side's layout is not this build's. `kind`
/// is the underlying error's numeric kind, named in the detail. The underlying error is not a type
/// at the current locks, so the caller passes its words.
pub fn layout_fault(kind: u32, expected: u64, found: u64) -> StyxFault {
    StyxFault::new(
        FaultCode::LayoutRefused,
        "the shared-memory layout is not this build's",
        format!("layout refused (kind {kind}): expected {expected:#x}, found {found:#x}"),
    )
}

/// The fault for a revision or feature Orion refused. As [`layout_fault`], the underlying error
/// is passed as its words and its kind.
pub fn orion_fault(kind: u32, revision: &str, feature: &str) -> StyxFault {
    StyxFault::new(
        FaultCode::OrionRefused,
        "Orion refused this revision or feature",
        format!("orion refused (kind {kind}): revision {revision}, feature {feature}"),
    )
}

/// Orion's refusal as Styx sees it. Orion's own `ClientError::Revision { node, client }` and
/// `ClientError::Level { feature, needs, node }` are not at the pinned Orion revision, so this
/// stand-in carries the same words; when Orion's rev moves, the caller maps Orion's error into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrionRefusal {
    /// The node's revision is not the one the client was built for.
    Revision {
        /// The node, as Orion names it.
        node: String,
        /// The revision the client was built for.
        client: String,
    },
    /// The feature needs a level the node does not provide.
    Level {
        /// The feature, as Orion names it.
        feature: String,
        /// The level the feature needs.
        needs: String,
        /// The node, as Orion names it.
        node: String,
    },
}

/// The fault for an Orion refusal: [`FaultCode::OrionRevision`] or [`FaultCode::OrionFeature`],
/// with the node and the words Orion gave.
pub fn orion_refusal_fault(refusal: &OrionRefusal) -> StyxFault {
    match refusal {
        OrionRefusal::Revision { node, client } => StyxFault::new(
            FaultCode::OrionRevision,
            "Orion refused this node's revision",
            format!("node {node} is not the revision this client was built for ({client})"),
        ),
        OrionRefusal::Level {
            feature,
            needs,
            node,
        } => StyxFault::new(
            FaultCode::OrionFeature,
            "Orion refused a feature at this level",
            format!("feature {feature} needs level {needs}, and node {node} does not provide it"),
        ),
    }
}

/// The fault for a frame socket's handshake refused by layout
/// ([`styx_socket::SocketError::Layout`]): [`FaultCode::LayoutRefused`], with the structure and both
/// values. `None` for any other handshake error, which the caller reports with its own code.
pub fn socket_fault(error: &styx_socket::SocketError) -> Option<StyxFault> {
    match error {
        styx_socket::SocketError::Layout {
            which,
            found,
            expected,
        } => Some(StyxFault::new(
            FaultCode::LayoutRefused,
            "the shared-memory layout is not this build's",
            format!(
                "{} is layout {found}; this build reads layout {expected}",
                which.what()
            ),
        )),
        _ => None,
    }
}

/// The fault for a device failure, with the device's own words.
pub fn device_fault(words: &str) -> StyxFault {
    StyxFault::new(
        FaultCode::DeviceFailed,
        "the device failed",
        words.to_string(),
    )
}

/// The fault for a GPU that did not open: no access to its nodes is [`FaultCode::NoGpuAccess`],
/// and every other reason is a device failure. The detail is the error's own text, and the status
/// line that names the group.
#[cfg(target_os = "linux")]
pub fn open_fault(error: &daedalus_wgpu::OpenError) -> StyxFault {
    use daedalus_wgpu::OpenError;
    match error {
        OpenError::NoAccess(_) => StyxFault::new(
            FaultCode::NoGpuAccess,
            "this user is refused the GPU's device nodes",
            format!("{error}; {}", crate::open_error_status(error)),
        ),
        other => StyxFault::new(
            FaultCode::DeviceFailed,
            "no GPU device opened",
            other.to_string(),
        ),
    }
}
