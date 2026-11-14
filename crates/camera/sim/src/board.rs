//! The board text the simulated drivers are served from.
//!
//! A board is one isp, a receiver it links to (`csi0`), a sensor that links to that receiver, and
//! (unless turned off) a spare receiver with no link, listed first so that a tool which takes the
//! first receiver fails. Every device's behaviour comes from its `config` table (see `drivers`).
//!
//! [`Family`] selects the driver config the test simulators of this workspace used, so a test that
//! ran on one of them runs on the same behaviour. [`Family::Link`] is the lock-step camera's board.

/// The driver config a board's devices take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// The lock-step camera's devices: a clock (see [`Board::clocked`]), every mode field.
    Link,
    /// The `styx-native-run` simulator: timed, every mode field, two frames dropped after start,
    /// and no receiver layout.
    Run,
    /// The `styx-native` simulator: timed, bare modes, and no receiver layout.
    Native,
    /// The `styx-lemnos` simulator: timed, bare modes, frame bytes that vary by sequence, and every
    /// frame's identity is 0.
    Lemnos,
}

/// A board: which devices it names and how they are configured.
#[derive(Debug, Clone)]
pub struct Board<'a> {
    family: Family,
    camera: Option<&'a str>,
    sensor_link: Option<&'a str>,
    decoy: bool,
    second_sensor: bool,
    identity: Option<&'a str>,
    source: Option<&'a str>,
}

impl<'a> Board<'a> {
    /// A timed board in `family`: no clock, so the receiver makes frames on its own.
    pub fn timed(family: Family) -> Self {
        Self {
            family,
            camera: None,
            sensor_link: Some("csi0"),
            decoy: true,
            second_sensor: false,
            identity: None,
            source: None,
        }
    }

    /// A board whose sensor and receiver share the lock-step clock named `camera`.
    pub fn clocked(camera: &'a str) -> Self {
        Self {
            camera: Some(camera),
            family: Family::Link,
            ..Self::timed(Family::Link)
        }
    }

    /// The sensor's link: the receiver it is wired to (`None` leaves it unlinked).
    pub fn sensor_link(mut self, link: Option<&'a str>) -> Self {
        self.sensor_link = link;
        self
    }

    /// Whether the spare receiver with no link is listed.
    pub fn decoy(mut self, on: bool) -> Self {
        self.decoy = on;
        self
    }

    /// Adds a second sensor, linked to the receiver `csi0`.
    pub fn second_sensor(mut self) -> Self {
        self.second_sensor = true;
        self
    }

    /// The name the kernel gives the sensor (its `identity`), when given.
    pub fn identity(mut self, name: Option<&'a str>) -> Self {
        self.identity = name;
        self
    }

    /// The sensor wired to the receiver's input (its `source`), when given.
    pub fn source(mut self, name: Option<&'a str>) -> Self {
        self.source = name;
        self
    }

    /// The board's TOML text.
    pub fn text(&self) -> String {
        let mut text = String::from(
            "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-camera\"\n\
             [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n",
        );
        if self.decoy {
            text += "[[devices]]\nid = \"csi-spare\"\ndriver = \"sim-receiver\"\n";
            text += &config(&self.receiver_keys(None));
            text += STREAMS;
        }
        text += "[[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n";
        text += &config(&self.receiver_keys(self.source));
        text += STREAMS;
        text += "[[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\n";
        if let Some(link) = self.sensor_link {
            text += &format!("link = \"{link}\"\n");
        }
        text += &config(&self.sensor_keys(self.identity));
        if self.second_sensor {
            text += "[[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n";
            text += &config(&self.sensor_keys(None));
        }
        text
    }

    fn sensor_keys(&self, identity: Option<&str>) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(camera) = self.camera {
            keys.push(format!("camera = \"{camera}\""));
        }
        if let Some(name) = identity {
            keys.push(format!("identity = \"{name}\""));
        }
        match self.family {
            Family::Link => {}
            Family::Run => {
                keys.push("modes = \"run\"".to_string());
                keys.push("drop_frames = 2".to_string());
            }
            Family::Native | Family::Lemnos => keys.push("modes = \"bare\"".to_string()),
        }
        keys
    }

    fn receiver_keys(&self, source: Option<&str>) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(camera) = self.camera {
            keys.push(format!("camera = \"{camera}\""));
        }
        if let Some(name) = source {
            keys.push(format!("source = \"{name}\""));
        }
        match self.family {
            Family::Link => {}
            Family::Run | Family::Native => keys.push("layout = false".to_string()),
            Family::Lemnos => {
                keys.push("frames = \"varied\"".to_string());
                keys.push("frame_ids = \"zero\"".to_string());
            }
        }
        keys
    }
}

/// Each receiver's stream: eight buffers of 32000 bytes.
const STREAMS: &str = "[devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n";

/// A device's `config` line, when it has keys (it must precede the device's streams table).
fn config(keys: &[String]) -> String {
    if keys.is_empty() {
        String::new()
    } else {
        format!("config = {{ {} }}\n", keys.join(", "))
    }
}
