//! The receiver: whatever turns the sensor's bus into buffers in memory (a CSI-2 receiver and
//! its DMA, a parallel camera port, an ISP front end that owns the capture nodes).

use core::task::{Context, Poll};

use crate::dma::FrameBuffer;
use crate::error::{ErrorKind, HalError};
use crate::time::Instant;

/// How the sensor's bus is wired. Linux takes it from the device tree; microcontrollers from
/// the sensor description's `[bus]` section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    /// MIPI CSI-2.
    Csi2 {
        /// Data lanes.
        lanes: u8,
        /// Link frequency in Hz (0: from the sensor format).
        link_frequency: u64,
        /// Continuous clock (else the clock lane may stop between packets).
        continuous_clock: bool,
        /// Virtual channel of the image data.
        virtual_channel: u8,
    },
    /// A parallel (DVP) port.
    Parallel {
        /// Data bits.
        width: u8,
        /// Data sampled on the rising pixel clock edge.
        pclk_rising: bool,
        /// HSYNC (HREF) active high.
        hsync_active_high: bool,
        /// VSYNC active high.
        vsync_active_high: bool,
        /// BT.656 embedded sync codes instead of sync lines.
        embedded_sync: bool,
    },
    /// Anything else (USB, a vendor link).
    Other,
}

/// Embedded data lines (CSI-2 data type 0x12) to capture with each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedConfig {
    /// CSI-2 data type.
    pub data_type: u8,
    /// Lines per frame.
    pub lines: u32,
    /// Bytes per line.
    pub line_bytes: u32,
}

/// Where frame buffers come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferSource {
    /// The receiver allocates its own.
    Own,
    /// From the platform's [`DmaMemory`](crate::DmaMemory) in this region.
    Allocate(crate::dma::Region),
}

/// A capture configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverConfig {
    /// The bus.
    pub bus: Bus,
    /// Media bus code (`MEDIA_BUS_FMT_*` numbering, as the sensor description's codes).
    pub bus_code: u32,
    /// Memory format (packed raw, YUYV, JPEG, ...).
    pub fourcc: [u8; 4],
    /// Width in pixels.
    pub width: u32,
    /// Height in lines.
    pub height: u32,
    /// Bytes per line, if fixed by the caller.
    pub stride: Option<u32>,
    /// Buffers to allocate.
    pub buffers: u32,
    /// Where they come from.
    pub memory: BufferSource,
    /// Embedded data to capture.
    pub embedded: Option<EmbeddedConfig>,
    /// Deliver [`SyncEvent::FrameStart`] events.
    pub frame_starts: bool,
}

/// Who starts first when streaming starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartOrder {
    /// Arm the receiver, then start the sensor (CSI-2 receivers that must see LP-11 first;
    /// a DCMI waits for VSYNC either way).
    ReceiverFirst,
    /// Start the sensor, then the receiver.
    SensorFirst,
    /// The receiver's own start asks for the sensor start and waits for it (the Styx sensor
    /// bridge: `STREAMON`, a bridge request, the acknowledgement).
    ReceiverDriven,
}

/// Which moment of a frame its timestamp marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampPoint {
    /// Start of frame.
    FrameStart,
    /// End of frame.
    FrameEnd,
    /// When the buffer was dequeued (no hardware timestamp).
    Dequeue,
}

/// What a receiver can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverCaps {
    /// Frame-start events (without them frame starts are inferred from dequeues).
    pub frame_start_events: bool,
    /// Embedded data capture.
    pub embedded_data: bool,
    /// What timestamps mark.
    pub timestamp: TimestampPoint,
    /// Start order.
    pub start_order: StartOrder,
    /// Most buffers.
    pub max_buffers: u32,
}

/// The configuration a receiver settled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configured {
    /// Bytes per line.
    pub stride: u32,
    /// Bytes per buffer.
    pub buffer_len: usize,
    /// Buffers allocated.
    pub buffers: u32,
}

/// Frame starts and embedded data: what the sensor service needs, on its own waker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    /// A frame started.
    FrameStart {
        /// Frame sequence number.
        sequence: u64,
        /// When.
        at: Instant,
    },
    /// Embedded data of a frame arrived in `slot` ([`Receiver::embedded`]).
    Embedded {
        /// Frame sequence number.
        sequence: u64,
        /// Slot.
        slot: u32,
        /// Bytes filled.
        bytes: usize,
    },
    /// A non-fatal problem (an overrun, a sync error): counted, the stream goes on.
    Glitch(ErrorKind),
}

/// A filled buffer: what the frame stream needs, on its own waker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDone {
    /// Buffer index.
    pub index: u32,
    /// Frame sequence number.
    pub sequence: u64,
    /// See [`ReceiverCaps::timestamp`].
    pub timestamp: Instant,
    /// Bytes filled (variable for JPEG).
    pub bytes_used: usize,
    /// The frame had a sync or CRC error.
    pub corrupt: bool,
    /// Statistics captured with the frame by an inline ISP ([`Receiver::statistics`]).
    pub stats_slot: Option<u32>,
}

/// `Send + Sync` with the `std` feature (the Linux event thread serves the sensor bridge with
/// it), nothing without (a microcontroller camera lives in one task).
#[cfg(feature = "std")]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(feature = "std")]
impl<T: Send + Sync> MaybeSendSync for T {}

/// `Send + Sync` with the `std` feature (the Linux event thread serves the sensor bridge with
/// it), nothing without (a microcontroller camera lives in one task).
#[cfg(not(feature = "std"))]
pub trait MaybeSendSync {}
#[cfg(not(feature = "std"))]
impl<T> MaybeSendSync for T {}

/// The runtime's sensor side, as a receiver calls it when it starts and stops.
pub trait SensorStart: MaybeSendSync + 'static {
    /// Write stream-on, start the control schedule.
    fn start(&self) -> Result<(), ErrorKind>;
    /// Write stream-off.
    fn stop(&self) -> Result<(), ErrorKind>;
}

/// A receiver. Methods take `&self` and it is `Sync`: interrupt handlers and the Linux event
/// thread share it with the frame path.
///
/// Two event channels with separate wakers: frame starts must be served at the frame start
/// even while the consumer still processes the previous frame, so they cannot queue behind
/// filled buffers.
pub trait Receiver: Sync {
    /// The error.
    type Error: HalError;
    /// A handle on one of its buffers: it keeps the buffer's memory while held, also after
    /// [`Self::release`] (an `Arc` on Linux, a `&'static` buffer on a microcontroller), so a
    /// frame can outlive its stream.
    type Buffer: FrameBuffer;

    /// What it can do.
    fn caps(&self) -> ReceiverCaps;
    /// Configure (allocates the buffers).
    fn configure(&self, cfg: &ReceiverConfig) -> Result<Configured, Self::Error>;
    /// Buffer `index` of the current configuration.
    fn buffer(&self, index: u32) -> Option<Self::Buffer>;
    /// Embedded data in `slot`.
    fn embedded(&self, slot: u32) -> Option<&[u8]> {
        let _ = slot;
        None
    }
    /// Statistics in `slot`.
    fn statistics(&self, slot: u32) -> Option<&[u8]> {
        let _ = slot;
        None
    }
    /// Give buffer `index` to the hardware; callable from any thread or task.
    fn queue(&self, index: u32) -> Result<(), Self::Error>;
    /// Start capturing; `sensor` is started here or by the receiver's own machinery
    /// ([`StartOrder`]).
    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), Self::Error>;
    /// Stop capturing.
    fn stop<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), Self::Error>;
    /// Free the buffers; frames still held keep their memory until dropped.
    fn release(&self) -> Result<(), Self::Error>;
    /// The next frame start or embedded data event.
    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, Self::Error>>;
    /// The next filled buffer.
    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, Self::Error>>;
    /// [`Self::poll_sync`] without waiting (superloops; the Linux event thread after its own
    /// `poll(2)`).
    fn try_sync(&self) -> Result<Option<SyncEvent>, Self::Error>;
    /// [`Self::poll_done`] without waiting.
    fn try_done(&self) -> Result<Option<FrameDone>, Self::Error>;
}
