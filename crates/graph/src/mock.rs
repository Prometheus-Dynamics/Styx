//! A synthetic provider for tests and examples: sensor → ISP → two outputs (plus statistics),
//! producing patterned frames at the configured rate.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;

use crate::caps::{
    BusCode, Capabilities, CostHint, FormatCaps, FourCc, Fraction, IntervalRange, MemoryDomains,
    Size, SizeRange,
};
use crate::chan::{self, Sender};
use crate::graph::{DeviceGraph, LinkFlags, Node, NodeKind, Port, PortPurpose, PortRef};
use crate::provider::{
    Device, DeviceInfo, DeviceKey, HotplugEvent, HotplugStream, OutputStream, Provider,
    ProviderError, StreamConfig,
};
use crate::rt::Sleep;

/// The frame rate when a stream does not ask for one.
pub const DEFAULT_MOCK_FPS: u32 = 30;

/// A synthetic frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MockFrame {
    /// The sink port it came out of.
    pub output: PortRef,
    /// Frame number since start; frames of one exposure share it across outputs.
    pub sequence: u64,
    /// Time of the frame's scheduled capture since streaming started.
    pub timestamp: Duration,
    pub format: FourCc,
    pub size: Size,
    /// A pattern that moves with `sequence`: byte `i` is `(i + sequence) mod 256`.
    pub data: Vec<u8>,
}

/// Devices made up on demand. `MockProvider::new()` has one camera, `mock0`.
#[derive(Clone)]
pub struct MockProvider {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    cameras: Vec<Camera>,
    subscribers: Vec<Sender<HotplugEvent>>,
}

struct Camera {
    info: DeviceInfo,
    connected: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProvider {
    /// A provider with one camera, `mock0`.
    pub fn new() -> Self {
        let provider = Self::empty();
        provider.plug("mock0");
        provider
    }

    /// A provider with no cameras.
    pub fn empty() -> Self {
        MockProvider {
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add a camera called `name` and announce it; returns its key.
    pub fn plug(&self, name: &str) -> DeviceKey {
        let key = DeviceKey(format!("mock:{name}"));
        let info = DeviceInfo {
            key: key.clone(),
            name: name.to_string(),
            identity: vec![format!("mock:{name}")],
            graph: Self::graph(),
            properties: vec![("provider".into(), "mock".into())],
        };
        let mut state = self.lock();
        state.cameras.push(Camera {
            info: info.clone(),
            connected: Arc::new(AtomicBool::new(true)),
            open: Arc::new(AtomicBool::new(false)),
        });
        state
            .subscribers
            .retain(|s| s.send(HotplugEvent::Added(info.clone())));
        key
    }

    /// Remove a camera: its streams end with [`ProviderError::Disconnected`]. False if unknown.
    pub fn unplug(&self, key: &DeviceKey) -> bool {
        let mut state = self.lock();
        let Some(pos) = state.cameras.iter().position(|c| &c.info.key == key) else {
            return false;
        };
        let camera = state.cameras.remove(pos);
        camera.connected.store(false, Ordering::Release);
        state
            .subscribers
            .retain(|s| s.send(HotplugEvent::Removed(key.clone())));
        true
    }

    /// The graph of every mock camera:
    ///
    /// ```text
    /// mock-sensor -> mock-isp -+-> main     (NV12/RGB3/GREY up to 1920x1080)
    ///                          +-> preview  (NV12/GREY up to 1280x720)
    ///                          +-> stats
    /// ```
    pub fn graph() -> DeviceGraph {
        let raw = BusCode::SRGGB10_1X10;
        let sizes = |w, h| SizeRange::stepwise(Size::new(16, 16), Size::new(w, h), Size::new(2, 2));
        let memory = |codes: &[FourCc], range: SizeRange| {
            codes
                .iter()
                .fold(Capabilities::new(), |caps, c| {
                    caps.format(FormatCaps::new(*c).size(range))
                })
                .memory(MemoryDomains::CPU)
        };
        let main = memory(
            &[FourCc::NV12, FourCc::RGB3, FourCc::GREY],
            sizes(1920, 1080),
        );
        let preview = memory(&[FourCc::NV12, FourCc::GREY], sizes(1280, 720));
        let stats = Capabilities::new()
            .format(FormatCaps::new(FourCc::new(b"MSTA")))
            .memory(MemoryDomains::CPU);
        let bus = Capabilities::new().format(FormatCaps::new(raw));

        let mut g = DeviceGraph::new();
        let sensor = g.add_node(
            Node::new("mock-sensor", NodeKind::Sensor).port(Port::output(
                "out",
                Capabilities::new().format(
                    FormatCaps::new(raw)
                        .size(SizeRange::discrete(1920, 1080))
                        .interval(IntervalRange::fps(1, 240)),
                ),
            )),
        );
        let isp = g.add_node(
            Node::new("mock-isp", NodeKind::IspStage)
                .cost(CostHint::fixed(2.0, 0.0))
                .port(Port::input("in", bus))
                .port(Port::output("main", main.clone()))
                .port(Port::output("preview", preview.clone()))
                .port(Port::output("stats", stats.clone()).purpose(PortPurpose::Stats)),
        );
        let sinks = [
            ("main", Port::input("in", main)),
            ("preview", Port::input("in", preview)),
            (
                "stats",
                Port::input("in", stats).purpose(PortPurpose::Stats),
            ),
        ];
        g.link(sensor.port(0), isp.port(0), LinkFlags::IMMUTABLE)
            .expect("mock link");
        for (i, (name, port)) in sinks.into_iter().enumerate() {
            let sink = g.add_node(Node::new(name, NodeKind::Sink).port(port));
            g.link(isp.port(i as u16 + 1), sink.port(0), LinkFlags::ENABLED)
                .expect("mock link");
        }
        g
    }
}

impl Provider for MockProvider {
    type Frame = MockFrame;

    fn name(&self) -> &str {
        "mock"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>, ProviderError> {
        Ok(self.lock().cameras.iter().map(|c| c.info.clone()).collect())
    }

    fn hotplug(&self) -> HotplugStream {
        let (tx, rx) = chan::channel();
        self.lock().subscribers.push(tx);
        Box::pin(rx)
    }

    fn open(&self, key: &DeviceKey) -> Result<Box<dyn Device<Frame = MockFrame>>, ProviderError> {
        let state = self.lock();
        let camera = state
            .cameras
            .iter()
            .find(|c| &c.info.key == key)
            .ok_or_else(|| ProviderError::NotFound(key.clone()))?;
        if camera.open.swap(true, Ordering::AcqRel) {
            return Err(ProviderError::Busy(format!("{key} is already open")));
        }
        Ok(Box::new(MockDevice {
            graph: camera.info.graph.clone(),
            info: camera.info.clone(),
            configured: Vec::new(),
            running: None,
            connected: camera.connected.clone(),
            open: camera.open.clone(),
        }))
    }
}

/// An open mock camera.
pub struct MockDevice {
    info: DeviceInfo,
    graph: DeviceGraph,
    configured: Vec<StreamConfig>,
    running: Option<Arc<AtomicBool>>,
    connected: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
}

impl MockDevice {
    fn check_connected(&self) -> Result<(), ProviderError> {
        if self.connected.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(ProviderError::Disconnected)
        }
    }
}

impl Device for MockDevice {
    type Frame = MockFrame;

    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn graph(&self) -> &DeviceGraph {
        &self.graph
    }

    fn configure(&mut self, streams: &[StreamConfig]) -> Result<(), ProviderError> {
        self.check_connected()?;
        if self.running.is_some() {
            return Err(ProviderError::Busy("stop before configuring".into()));
        }
        if streams.is_empty() {
            return Err(ProviderError::InvalidConfig("no streams".into()));
        }
        for (i, stream) in streams.iter().enumerate() {
            stream.check(&self.graph)?;
            if streams[..i]
                .iter()
                .any(|s| s.path.output() == stream.path.output())
            {
                return Err(ProviderError::InvalidConfig(
                    "two streams for one output".into(),
                ));
            }
        }
        let paths: Vec<_> = streams.iter().map(|s| &s.path).collect();
        let plan = self.graph.link_plan(&paths)?;
        self.graph.apply(&plan)?;
        self.configured = streams.to_vec();
        Ok(())
    }

    fn start(&mut self) -> Result<Vec<OutputStream<MockFrame>>, ProviderError> {
        self.check_connected()?;
        if self.running.is_some() {
            return Err(ProviderError::Busy("already streaming".into()));
        }
        if self.configured.is_empty() {
            return Err(ProviderError::InvalidConfig("configure first".into()));
        }
        let running = Arc::new(AtomicBool::new(true));
        let start = Instant::now();
        let streams = self
            .configured
            .iter()
            .map(|config| OutputStream {
                output: config.path.output(),
                config: config.clone(),
                frames: Box::pin(MockFrames {
                    tick: crate::rt::sleep_until(start),
                    start,
                    period: config
                        .interval
                        .unwrap_or(Fraction::from_fps(DEFAULT_MOCK_FPS))
                        .as_duration(),
                    sequence: 0,
                    config: config.clone(),
                    running: running.clone(),
                    connected: self.connected.clone(),
                    ended: false,
                }),
            })
            .collect();
        self.running = Some(running);
        Ok(streams)
    }

    fn stop(&mut self) -> Result<(), ProviderError> {
        if let Some(running) = self.running.take() {
            running.store(false, Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for MockDevice {
    fn drop(&mut self) {
        let _ = self.stop();
        self.open.store(false, Ordering::Release);
    }
}

struct MockFrames {
    tick: Sleep,
    start: Instant,
    period: Duration,
    sequence: u64,
    config: StreamConfig,
    running: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    ended: bool,
}

impl MockFrames {
    fn frame(&self) -> MockFrame {
        let size = self.config.size;
        let bytes = self
            .config
            .format
            .frame_bytes(size.width, size.height)
            .unwrap_or(size.width as usize * size.height as usize);
        let shift = self.sequence as usize;
        MockFrame {
            output: self.config.path.output(),
            sequence: self.sequence,
            timestamp: self.tick.deadline() - self.start,
            format: self.config.format,
            size,
            data: (0..bytes).map(|i| (i.wrapping_add(shift)) as u8).collect(),
        }
    }
}

impl Stream for MockFrames {
    type Item = Result<MockFrame, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        if !this.connected.load(Ordering::Acquire) {
            this.ended = true;
            return Poll::Ready(Some(Err(ProviderError::Disconnected)));
        }
        if !this.running.load(Ordering::Acquire) {
            this.ended = true;
            return Poll::Ready(None);
        }
        if Pin::new(&mut this.tick).poll(cx).is_pending() {
            return Poll::Pending;
        }
        let frame = this.frame();
        this.sequence += 1;
        let offset = this.period.as_nanos().saturating_mul(this.sequence as u128);
        let next = this.start + Duration::from_nanos(offset.min(u64::MAX as u128) as u64);
        this.tick.reset(next);
        Poll::Ready(Some(Ok(frame)))
    }
}

#[cfg(test)]
#[path = "mock_tests.rs"]
mod tests;
