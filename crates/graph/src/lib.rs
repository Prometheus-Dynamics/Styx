//! The device graph of the Styx native stack: nodes (sensors, receivers, ISP stages, scalers,
//! codecs), their ports and capabilities, the providers that discover them, and a
//! runtime-agnostic async core for pollable devices.
//!
//! - [`DeviceGraph`]: [`Node`]s with [`Port`]s joined by [`Link`]s; [`GraphPath`]s from a
//!   sensor to the sinks where frames land in memory, and the [`LinkPlan`] that activates them.
//! - [`Provider`] / [`Device`]: discovery, hotplug, configuring paths and streaming frames of a
//!   provider-chosen type. [`MockProvider`] makes up a camera for tests.
//! - [`rt`]: an epoll reactor thread, readiness futures, timers and `block_on`, usable from any
//!   executor.
//!
//! ```
//! use styx_graph::{Device, FourCc, Fraction, MockProvider, Provider, Size, StreamConfig, rt};
//!
//! let provider = MockProvider::new();
//! let info = &provider.discover()?[0];
//! let sensor = info.graph.find("mock-sensor").unwrap().id;
//! let path = info.graph.output_paths(sensor).remove(0);
//!
//! let mut device = provider.open(&info.key)?;
//! device.configure(&[StreamConfig::new(path, FourCc::NV12, Size::new(64, 48))
//!     .interval(Fraction::from_fps(60))])?;
//! let mut streams = device.start()?;
//! let frame = rt::block_on(rt::next(&mut streams[0].frames)).unwrap()?;
//! assert_eq!(frame.sequence, 0);
//! # Ok::<(), styx_graph::ProviderError>(())
//! ```
//!
//! See `docs/native-stack/graph.md`.
#![deny(unsafe_code)]

mod caps;
mod chan;
mod graph;
mod mock;
mod path;
mod provider;
pub mod rt;
pub mod sample;

pub use caps::{
    BusCode, Capabilities, Cost, CostHint, FormatCaps, FormatCode, FourCc, Fraction, IntervalRange,
    MemoryDomains, Size, SizeRange,
};
pub use graph::{
    ComputeKind, DeviceGraph, Direction, GraphError, Link, LinkFlags, LinkId, LinkMedium, Node,
    NodeId, NodeKind, Port, PortPurpose, PortRef, ReceiverKind,
};
pub use mock::{DEFAULT_MOCK_FPS, MockDevice, MockFrame, MockProvider};
pub use path::{GraphPath, LinkPlan};
pub use provider::{
    Device, DeviceInfo, DeviceKey, FrameStream, HotplugEvent, HotplugStream, OutputStream,
    Provider, ProviderError, StreamConfig,
};
