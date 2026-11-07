//! The served cameras' endpoints, published to the product: an [`EndpointSink`] it implements,
//! and/or a JSON file replaced atomically ([`JsonFile`]).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The transport of every endpoint: Styx's frame protocol over a Unix socket.
pub const TRANSPORT: &str = "styx-frames+unix";

/// One served camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    /// The camera's stable name ([`styx::DeviceIdentity::display`]).
    pub name: String,
    /// Its product name for people, when known.
    pub product: Option<String>,
    /// Its identity keys (the first names the socket).
    pub keys: Vec<String>,
    /// The backend it is captured through (its first as probed).
    pub backend: Option<styx::BackendKind>,
    /// Where that backend opens it: the V4L2 device node, the native or UVC key, or the
    /// libcamera id.
    pub node: Option<String>,
    /// Its service's socket.
    pub socket: PathBuf,
    /// [`TRANSPORT`].
    pub transport: &'static str,
}

/// Receives the whole endpoint list whenever it changes (a camera served or gone), on the
/// daemon's thread; keep it quick.
pub trait EndpointSink: Send {
    fn publish(&mut self, endpoints: &[Endpoint]);
}

impl<F: FnMut(&[Endpoint]) + Send> EndpointSink for F {
    fn publish(&mut self, endpoints: &[Endpoint]) {
        self(endpoints)
    }
}

/// No sink.
pub struct Nowhere;

impl EndpointSink for Nowhere {
    fn publish(&mut self, _: &[Endpoint]) {}
}

/// Writes the list as JSON to a file, replaced atomically (written beside it, then renamed),
/// so readers never see half a list.
pub struct JsonFile {
    path: PathBuf,
}

impl JsonFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, endpoints: &[Endpoint]) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut f, endpoints)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
}

impl EndpointSink for JsonFile {
    fn publish(&mut self, endpoints: &[Endpoint]) {
        if let Err(e) = self.write(endpoints) {
            eprintln!("styx-camerad: writing {}: {e}", self.path.display());
        }
    }
}

/// Both sinks: the product's and the file.
pub struct Both<A, B>(pub A, pub B);

impl<A: EndpointSink, B: EndpointSink> EndpointSink for Both<A, B> {
    fn publish(&mut self, endpoints: &[Endpoint]) {
        self.0.publish(endpoints);
        self.1.publish(endpoints);
    }
}
