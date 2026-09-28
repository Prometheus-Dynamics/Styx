//! Providers: where devices come from, and how a device is configured and streamed.

use std::pin::Pin;

use futures_core::Stream;

use crate::caps::{FormatCode, FourCc, Fraction, Size};
use crate::graph::{DeviceGraph, GraphError, PortRef};
use crate::path::GraphPath;

/// A stable identifier of a device within its provider (a media device path, a USB port path,
/// a libcamera id, a URL).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DeviceKey(pub String);

impl std::fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What discovery reports about a device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub key: DeviceKey,
    /// Human-readable name.
    pub name: String,
    /// Fingerprints that identify the same physical camera across providers (`usb:046d:0825`,
    /// `of:/base/axi/pcie@1000120000/rp1/csi@110000/ov9782@60`), like Styx's `DeviceIdentity`.
    pub identity: Vec<String>,
    pub graph: DeviceGraph,
    pub properties: Vec<(String, String)>,
}

/// Devices appearing and disappearing.
#[derive(Clone, Debug)]
pub enum HotplugEvent {
    Added(DeviceInfo),
    Removed(DeviceKey),
}

pub type HotplugStream = Pin<Box<dyn Stream<Item = HotplugEvent> + Send>>;

/// Frames (or the error that ended the stream). The stream ends after the device stops.
pub type FrameStream<F> = Pin<Box<dyn Stream<Item = Result<F, ProviderError>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no device {0}")]
    NotFound(DeviceKey),
    #[error("device is busy: {0}")]
    Busy(String),
    #[error("device disconnected")]
    Disconnected,
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One stream to configure: a path to a sink and the format frames should have there.
#[derive(Clone, PartialEq, Debug)]
pub struct StreamConfig {
    pub path: GraphPath,
    pub format: FourCc,
    pub size: Size,
    /// Frame interval; `None` lets the device choose.
    pub interval: Option<Fraction>,
}

impl StreamConfig {
    pub fn new(path: GraphPath, format: FourCc, size: Size) -> Self {
        StreamConfig {
            path,
            format,
            size,
            interval: None,
        }
    }

    pub fn interval(mut self, interval: Fraction) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Check against `graph`: the path is intact, the sink takes this format and size, and every
    /// port on the path that lists intervals for the formats it carries allows this one.
    pub fn check(&self, graph: &DeviceGraph) -> Result<(), ProviderError> {
        graph.check_path(&self.path)?;
        let sink = graph.port(self.path.output())?;
        let code = FormatCode::Memory(self.format);
        if !sink.caps.accepts(code, self.size, None) {
            return Err(ProviderError::InvalidConfig(format!(
                "{} {}x{} not supported by {}",
                self.format,
                self.size.width,
                self.size.height,
                graph.node(self.path.sink())?.name
            )));
        }
        if let Some(interval) = self.interval {
            for link in self.path.links() {
                let link = graph.link_by_id(*link)?;
                for port in [link.from, link.to] {
                    if !interval_allowed(graph, port, interval)? {
                        return Err(ProviderError::InvalidConfig(format!(
                            "{:.2} fps not supported by {}",
                            interval.fps(),
                            graph.node(port.node)?.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn interval_allowed(
    graph: &DeviceGraph,
    port: PortRef,
    interval: Fraction,
) -> Result<bool, GraphError> {
    let caps = &graph.port(port)?.caps;
    let constrained: Vec<_> = caps
        .formats
        .iter()
        .filter(|f| !f.intervals.is_empty())
        .collect();
    Ok(constrained.is_empty() || constrained.iter().any(|f| f.accepts_interval(interval)))
}

/// A stream a started device delivers.
pub struct OutputStream<F> {
    /// The sink port frames arrive at.
    pub output: PortRef,
    pub config: StreamConfig,
    pub frames: FrameStream<F>,
}

impl<F> std::fmt::Debug for OutputStream<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputStream")
            .field("output", &self.output)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// An open device.
///
/// Lifecycle: [`configure`](Device::configure) the paths to use, [`start`](Device::start) to
/// get one frame stream per configured path, [`stop`](Device::stop) (or drop) to end them.
/// Configuration applies the link changes the paths need (see [`DeviceGraph::link_plan`]).
pub trait Device: Send {
    /// What frames look like: an opaque type, so this crate needs nothing from `styx-core`.
    type Frame: Send + 'static;

    fn info(&self) -> &DeviceInfo;

    /// The graph as currently configured (link states follow the last configuration).
    fn graph(&self) -> &DeviceGraph;

    /// Configure these streams, replacing any previous configuration. Fails while started.
    fn configure(&mut self, streams: &[StreamConfig]) -> Result<(), ProviderError>;

    /// Start streaming; one [`OutputStream`] per configured stream, in configuration order.
    fn start(&mut self) -> Result<Vec<OutputStream<Self::Frame>>, ProviderError>;

    /// Stop streaming; frame streams end.
    fn stop(&mut self) -> Result<(), ProviderError>;
}

/// A source of devices: a kernel interface (V4L2/media controller, UVC), a compatibility layer
/// (libcamera), or something synthetic (mock, replay, network).
pub trait Provider: Send + Sync {
    type Frame: Send + 'static;

    /// Short, stable name (`v4l2`, `libcamera`, `mock`).
    fn name(&self) -> &str;

    /// The devices present now.
    fn discover(&self) -> Result<Vec<DeviceInfo>, ProviderError>;

    /// Devices added and removed from now on.
    fn hotplug(&self) -> HotplugStream;

    fn open(&self, key: &DeviceKey) -> Result<Box<dyn Device<Frame = Self::Frame>>, ProviderError>;
}
