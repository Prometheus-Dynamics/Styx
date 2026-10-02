//! Native cameras (bridged sensors and sensors with a kernel driver) as a `styx-graph`
//! [`Provider`]: discovery with the media topology as the device graph, exclusive open,
//! configuration by graph path, frame streams, and hotplug by watching bridges and sensor
//! entities appear and disappear.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;
use styx_graph::rt::Sleep;
use styx_graph::{
    Device, DeviceGraph, DeviceInfo, DeviceKey, HotplugEvent, HotplugStream, OutputStream,
    Provider, ProviderError, StreamConfig,
};
use styx_kernel::FourCc;

use crate::buffers::NativeFrame;
use crate::camera::{CameraOptions, NativeCamera, StreamSettings};
use crate::discover::{CameraInfo, camera_keys, discover, discover_key};
use crate::error::NativeError;
use crate::library::SensorLibrary;
use crate::stream::FrameStream;

/// The provider name.
pub const PROVIDER_NAME: &str = "native";

/// Cameras behind Styx sensor bridges, and sensors with a kernel driver.
#[derive(Clone, Debug)]
pub struct NativeProvider {
    library: Arc<SensorLibrary>,
    options: CameraOptions,
    hotplug_interval: Duration,
}

impl Default for NativeProvider {
    fn default() -> Self {
        Self::new(SensorLibrary::system())
    }
}

impl NativeProvider {
    /// A provider looking descriptions up in `library`.
    pub fn new(library: SensorLibrary) -> Self {
        Self {
            library: Arc::new(library),
            options: CameraOptions::default(),
            hotplug_interval: Duration::from_secs(1),
        }
    }

    /// Options for the cameras it opens.
    pub fn with_options(mut self, options: CameraOptions) -> Self {
        self.options = options;
        self
    }

    /// How often hotplug looks for bridges (sysfs reads).
    pub fn with_hotplug_interval(mut self, interval: Duration) -> Self {
        self.hotplug_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// The description library.
    pub fn library(&self) -> &SensorLibrary {
        &self.library
    }

    /// Discovery with the problems met (a bridge without a description).
    pub fn discover_cameras(&self) -> (Vec<CameraInfo>, Vec<NativeError>) {
        discover(&self.library)
    }

    /// Opens a camera directly (the typed API rather than [`Device`]).
    pub fn open_camera(&self, key: &str) -> Result<NativeCamera, NativeError> {
        let info = find(&self.library, key)?;
        NativeCamera::open(info, self.options.clone())
    }
}

fn find(library: &SensorLibrary, key: &str) -> Result<CameraInfo, NativeError> {
    discover_key(key, library)
}

/// The `styx-graph` view of a camera.
pub fn device_info(info: &CameraInfo) -> DeviceInfo {
    DeviceInfo {
        key: DeviceKey(info.key.clone()),
        name: info.display_name(),
        identity: info.identity(),
        graph: info.graph.graph.clone(),
        properties: info.properties(),
    }
}

impl Provider for NativeProvider {
    type Frame = NativeFrame;

    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>, ProviderError> {
        let (found, _) = discover(&self.library);
        Ok(found.iter().map(device_info).collect())
    }

    fn hotplug(&self) -> HotplugStream {
        Box::pin(Hotplug {
            library: Arc::clone(&self.library),
            known: camera_keys().into_iter().map(|k| (k, ())).collect(),
            queue: VecDeque::new(),
            sleep: styx_graph::rt::sleep(self.hotplug_interval),
            interval: self.hotplug_interval,
        })
    }

    fn open(&self, key: &DeviceKey) -> Result<Box<dyn Device<Frame = NativeFrame>>, ProviderError> {
        let info = find(&self.library, &key.0).map_err(|e| match e {
            NativeError::Topology(_) => ProviderError::NotFound(key.clone()),
            e => e.into(),
        })?;
        let device_info = device_info(&info);
        let camera = NativeCamera::open(info, self.options.clone())?;
        Ok(Box::new(NativeDevice {
            camera,
            info: device_info,
            configs: Vec::new(),
        }))
    }
}

/// An open camera as a [`Device`].
pub struct NativeDevice {
    camera: NativeCamera,
    info: DeviceInfo,
    configs: Vec<StreamConfig>,
}

impl NativeDevice {
    /// The camera (typed controls, configuration details).
    pub fn camera(&self) -> &NativeCamera {
        &self.camera
    }
}

impl Device for NativeDevice {
    type Frame = NativeFrame;

    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn graph(&self) -> &DeviceGraph {
        &self.camera.info().graph.graph
    }

    fn configure(&mut self, streams: &[StreamConfig]) -> Result<(), ProviderError> {
        let [config] = streams else {
            return Err(ProviderError::Unsupported(format!(
                "{} streams: one raw stream is supported until the ISP paths land",
                streams.len()
            )));
        };
        let map = &self.camera.info().graph;
        if config.path.sink() != map.raw_sink() {
            return Err(ProviderError::Unsupported(format!(
                "path {}: only the raw path is supported",
                config.path.describe(&map.graph)
            )));
        }
        config.check(&map.graph)?;
        let settings = StreamSettings {
            width: config.size.width,
            height: config.size.height,
            fourcc: Some(FourCc(config.format.0)),
            code: None,
            interval: config.interval,
        };
        self.camera.configure(&settings)?;
        self.info.graph = self.camera.info().graph.graph.clone();
        self.configs = vec![config.clone()];
        Ok(())
    }

    fn start(&mut self) -> Result<Vec<OutputStream<NativeFrame>>, ProviderError> {
        let config = self
            .configs
            .first()
            .cloned()
            .ok_or_else(|| ProviderError::InvalidConfig("configure before starting".into()))?;
        let frames = self.camera.start()?;
        Ok(vec![OutputStream {
            output: config.path.output(),
            config,
            frames: Box::pin(ProviderFrames(frames)),
        }])
    }

    fn stop(&mut self) -> Result<(), ProviderError> {
        Ok(self.camera.stop()?)
    }
}

struct ProviderFrames(FrameStream);

impl Stream for ProviderFrames {
    type Item = Result<NativeFrame, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|o| o.map(|r| r.map_err(ProviderError::from)))
    }
}

/// Polls for cameras on a timer (bridges in sysfs, sensor entities in the media graphs):
/// cheap (a directory listing, a topology read per media device) and needs no thread.
struct Hotplug {
    library: Arc<SensorLibrary>,
    known: BTreeMap<String, ()>,
    queue: VecDeque<HotplugEvent>,
    sleep: Sleep,
    interval: Duration,
}

impl Hotplug {
    fn rescan(&mut self) {
        let now = camera_keys();
        let gone: Vec<String> = self
            .known
            .keys()
            .filter(|k| !now.contains(k))
            .cloned()
            .collect();
        for k in gone {
            self.known.remove(&k);
            self.queue.push_back(HotplugEvent::Removed(DeviceKey(k)));
        }
        for k in now {
            if self.known.contains_key(&k) {
                continue;
            }
            // A camera whose graph is not complete yet (receiver still binding) is retried on
            // the next scan.
            if let Ok(info) = discover_key(&k, &self.library) {
                self.known.insert(k, ());
                self.queue
                    .push_back(HotplugEvent::Added(device_info(&info)));
            }
        }
    }
}

impl Stream for Hotplug {
    type Item = HotplugEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<HotplugEvent>> {
        let this = &mut *self;
        loop {
            if let Some(ev) = this.queue.pop_front() {
                return Poll::Ready(Some(ev));
            }
            match Pin::new(&mut this.sleep).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => {
                    this.sleep.reset(Instant::now() + this.interval);
                    this.rescan();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_without_bridges_has_no_cameras() {
        let p = NativeProvider::new(SensorLibrary::new())
            .with_hotplug_interval(Duration::from_millis(10));
        assert_eq!(p.name(), "native");
        assert!(p.discover().unwrap().is_empty());
        assert!(matches!(
            p.open(&DeviceKey("bridge:/dev/v4l-subdev99".into())),
            Err(ProviderError::NotFound(_))
        ));
        // Nothing appears: the hotplug stream stays pending across a few scans.
        let mut hp = p.hotplug();
        let r = styx_graph::rt::block_on(styx_graph::rt::timeout(
            Duration::from_millis(50),
            styx_graph::rt::next(&mut hp),
        ));
        assert!(r.is_err());
    }
}
