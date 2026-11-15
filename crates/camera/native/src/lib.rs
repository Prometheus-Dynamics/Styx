//! Native cameras on a Lemnos board: every `camera.sensor/1` device, with its receiver and ISPs.
//!
//! A native camera is a sensor and the chain its `link`s lead through: its receiver and the ISPs
//! after it, in link order. styx-lemnos's [`find_chain`] resolves one such chain, by tag and link,
//! and refuses a board with more than one sensor. [`cameras`] resolves every sensor on the board,
//! one camera each, so a board with two sensors gives two cameras.
//!
//! The device list is the Lemnos service's. Nothing here walks sysfs or a media graph; the
//! hardware facts are Lemnos's, and this crate only says which devices make a camera.
//!
//! Health is carried, not judged: a sensor that reports `Degraded` but still answers is a usable
//! camera, and its health and reason are on the [`Camera`] for a caller to show. The one check
//! that refuses a camera is the wiring check [`discover`] makes with the contracts: the receiver's
//! `source` against the sensor's `identity` (see [`Refusal`]). A receiver that cannot say what is
//! wired to its input (`NOT_SUPPORTED`) is unknown, carried as [`LinkCheck::Unknown`], and neither
//! a match nor a mismatch.
//!
//! Finding a chain does not claim it. One backend owns a camera at a time, and two cameras may
//! name the same receiver on a board; choosing between them is for the layer that opens a camera.

use std::fmt;

use lemnos_client::{Client, DeviceEntry, DeviceHealth, RemoteCaller};
use lemnos_contract_camera_receiver::{CameraReceiver, MAX_FEED_NAME};
use lemnos_contract_camera_sensor::{CameraSensor, LinkCheck, MAX_IDENTITY_NAME, check_link};
use lemnos_core::{Bound, Status};
use styx_lemnos::{Chain, ChainError, Error, SENSOR_TAG, find_chains};

/// A camera: a sensor and the receiver and ISPs its `link`s lead to, as Lemnos names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    chain: Chain,
    link: LinkCheck,
}

impl Camera {
    fn new(chain: Chain) -> Self {
        Self {
            chain,
            link: LinkCheck::Unknown("the receiver's source has not been read"),
        }
    }

    /// The camera's name on this board: its sensor device's name. Unique on one board.
    pub fn name(&self) -> &str {
        &self.chain.sensor.name
    }

    /// The sensor, receiver and ISP devices.
    pub fn chain(&self) -> &Chain {
        &self.chain
    }

    /// The sensor device.
    pub fn sensor(&self) -> &DeviceEntry {
        &self.chain.sensor
    }

    /// The receiver device the sensor links to.
    pub fn receiver(&self) -> &DeviceEntry {
        &self.chain.receiver
    }

    /// The ISPs after the receiver, in link order. Empty when the receiver links to none.
    pub fn isps(&self) -> &[DeviceEntry] {
        &self.chain.isps
    }

    /// The sensor's health as the service reports it. A `Degraded` camera is usable.
    pub fn health(&self) -> DeviceHealth {
        self.chain.sensor.health
    }

    /// The sensor's reason for its health, as the service reports it (empty when it has none).
    pub fn reason(&self) -> &str {
        &self.chain.sensor.reason
    }

    /// What the wiring check found: [`LinkCheck::Confirmed`] when the receiver's input is this
    /// sensor, [`LinkCheck::Unknown`] with the reason when it cannot say (or was not asked, as
    /// [`cameras`] does). A mismatch or nothing wired is refused by [`discover`], not carried here.
    pub fn link(&self) -> &LinkCheck {
        &self.link
    }
}

/// Why a sensor on the board is not a camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The sensor's links do not make a camera (see [`ChainError`]).
    Chain(ChainError),
    /// The receiver's input is fed by another sensor than this one.
    Mismatch {
        sensor: String,
        identity: String,
        receiver: String,
        source: String,
    },
    /// The receiver has nothing wired to its input.
    Unwired { sensor: String, receiver: String },
    /// A device could not be asked now (the read failed); nothing is concluded, so a later
    /// discovery asks again.
    Probe { device: String, why: String },
    /// The board's Lemnos and this Styx are different builds: the device's contract `contract` is
    /// laid out another way here. `text` is the sentence the Lemnos client's error gives, as
    /// [`styx_lemnos::Error::ContractMismatch`] words it. Refused at bind.
    Contract {
        device: String,
        contract: String,
        text: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chain(e) => write!(f, "{e}"),
            Self::Mismatch {
                sensor,
                identity,
                receiver,
                source,
            } => write!(
                f,
                "sensor {sensor} ({identity}) is linked to receiver {receiver}, whose input is fed by {source}"
            ),
            Self::Unwired { sensor, receiver } => write!(
                f,
                "receiver {receiver} has nothing wired to its input (sensor {sensor})"
            ),
            Self::Probe { device, why } => write!(f, "{device}: could not be asked: {why}"),
            Self::Contract { text, .. } => f.write_str(text),
        }
    }
}

/// Every camera on a board, and the sensors that did not resolve to one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// The cameras, in the order the board lists their sensors.
    pub cameras: Vec<Camera>,
    /// Each sensor that is not a camera, by its name, with why.
    pub refused: Vec<(String, Refusal)>,
}

fn has_tag(device: &DeviceEntry, tag: &str) -> bool {
    device.tags.iter().any(|t| t == tag)
}

/// Resolves every `camera.sensor/1` device in `devices` to its chain. The wiring check is not
/// made here (it needs the contracts): each camera's [`Camera::link`] says so.
pub fn cameras(devices: &[DeviceEntry]) -> Discovery {
    let mut found = Discovery::default();
    let sensors = devices.iter().filter(|d| has_tag(d, SENSOR_TAG));
    for (sensor, chain) in sensors.zip(find_chains(devices)) {
        match chain {
            Ok(chain) => found.cameras.push(Camera::new(chain)),
            Err(why) => found
                .refused
                .push((sensor.name.clone(), Refusal::Chain(why))),
        }
    }
    found
}

/// Lists the board's devices through `client` and returns its cameras, each checked for wiring:
/// the receiver's `source` against the sensor's `identity`, through the sensor contract's
/// [`check_link`].
pub fn discover(client: &Client) -> Result<Discovery, Error> {
    let devices = client.list().map_err(Error::Client)?;
    let mut found = cameras(&devices);
    let mut checked = Vec::with_capacity(found.cameras.len());
    for camera in std::mem::take(&mut found.cameras) {
        match wiring(client, camera) {
            Ok(camera) => checked.push(camera),
            Err(refusal) => found.refused.push((refusal.0, refusal.1)),
        }
    }
    found.cameras = checked;
    Ok(found)
}

/// The receiver's `source` and the sensor's `identity`, checked. A camera comes back with its
/// [`LinkCheck`]; a mismatch, nothing wired, or a failed read refuses it.
fn wiring(client: &Client, mut camera: Camera) -> Result<Camera, (String, Refusal)> {
    let sensor_name = camera.sensor().name.clone();
    let receiver_name = camera.receiver().name.clone();
    let refused = |device: &str, why: String| (sensor_name.clone(), probe(device, why));
    let mut sensor: Bound<dyn CameraSensor, RemoteCaller> = client
        .open::<dyn CameraSensor>(&sensor_name)
        .map_err(|e| (sensor_name.clone(), open_refusal(&sensor_name, e)))?;
    let mut receiver: Bound<dyn CameraReceiver, RemoteCaller> = client
        .open::<dyn CameraReceiver>(&receiver_name)
        .map_err(|e| (sensor_name.clone(), open_refusal(&receiver_name, e)))?;

    let mut own = [0u8; MAX_IDENTITY_NAME];
    let identity: Option<String> = match sensor.identity(&mut own) {
        Ok(len) => Some(text(own.get(..len), &sensor_name).map_err(|e| refused(&sensor_name, e))?),
        Err(Status::NOT_SUPPORTED) => None,
        Err(status) => return Err(refused(&sensor_name, format!("identity: {status:?}"))),
    };

    let mut fed = [0u8; MAX_FEED_NAME];
    // `source` is the receiver's answer: a name, nothing wired (an empty name), or a status.
    let source: Result<String, Status> = match receiver.source(&mut fed) {
        Ok(len) => {
            Ok(text(fed.get(..len), &receiver_name).map_err(|e| refused(&receiver_name, e))?)
        }
        Err(status) => Err(status),
    };
    let answer = match &source {
        Ok(name) => Ok(name.as_bytes()),
        Err(status) => Err(*status),
    };
    let link = check_link(answer, identity.as_deref().map(str::as_bytes))
        .map_err(|status| refused(&receiver_name, format!("source: {status:?}")))?;
    match (link, source) {
        (LinkCheck::Mismatch(_), Ok(name)) if name.is_empty() => Err((
            sensor_name.clone(),
            Refusal::Unwired {
                sensor: sensor_name,
                receiver: receiver_name,
            },
        )),
        (LinkCheck::Mismatch(_), Ok(name)) => Err((
            sensor_name.clone(),
            Refusal::Mismatch {
                sensor: sensor_name,
                identity: identity.unwrap_or_default(),
                receiver: receiver_name,
                source: name,
            },
        )),
        (other, _) => {
            camera.link = other;
            Ok(camera)
        }
    }
}

/// Why a device could not be opened: a contract another build's is a refusal of its own, with the
/// Lemnos error's words; anything else is a device that could not be asked now.
fn open_refusal(device: &str, error: lemnos_client::Error) -> Refusal {
    if matches!(error, lemnos_client::Error::ContractMismatch { .. }) {
        let typed = Error::from_client(error);
        return Refusal::Contract {
            device: device.to_string(),
            contract: typed.contract_mismatch().unwrap_or_default().to_string(),
            text: typed.to_string(),
        };
    }
    probe(device, error.to_string())
}

fn probe(device: &str, why: String) -> Refusal {
    Refusal::Probe {
        device: device.to_string(),
        why,
    }
}

/// A name the device wrote, as text. Not UTF-8 is a failed read.
fn text(bytes: Option<&[u8]>, device: &str) -> Result<String, String> {
    let bytes = bytes.ok_or_else(|| format!("{device}: name longer than its buffer"))?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| format!("{device}: name is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemnos_client::DeviceState;

    fn device(index: u32, name: &str, tags: &[&str], link: Option<&str>) -> DeviceEntry {
        DeviceEntry {
            index,
            name: name.to_string(),
            driver: String::new(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            state: DeviceState::Running,
            health: DeviceHealth::Ok,
            reason: String::new(),
            link: link.map(str::to_string),
        }
    }

    #[test]
    fn no_sensor_is_no_camera() {
        let found = cameras(&[device(0, "gpio", &["gpio"], None)]);
        assert!(found.cameras.is_empty());
        assert!(found.refused.is_empty());
    }

    #[test]
    fn one_sensor_is_one_camera_with_its_receiver_and_isp() {
        let devices = [
            device(0, "sensor", &["camera.sensor/1"], Some("receiver")),
            device(1, "receiver", &["camera.receiver/1"], Some("isp")),
            device(2, "isp", &["camera.isp/1"], None),
        ];
        let found = cameras(&devices);
        assert_eq!(found.cameras.len(), 1);
        let camera = &found.cameras[0];
        assert_eq!(camera.name(), "sensor");
        assert_eq!(camera.receiver().name, "receiver");
        assert_eq!(
            camera
                .isps()
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["isp"]
        );
        assert!(found.refused.is_empty());
    }

    #[test]
    fn two_sensors_on_one_receiver_are_two_cameras() {
        let devices = [
            device(0, "a", &["camera.sensor/1"], Some("receiver")),
            device(1, "b", &["camera.sensor/1"], Some("receiver")),
            device(2, "receiver", &["camera.receiver/1"], None),
        ];
        let found = cameras(&devices);
        let names: Vec<&str> = found.cameras.iter().map(Camera::name).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(found.cameras.iter().all(|c| c.isps().is_empty()));
        assert!(found.refused.is_empty());
    }

    #[test]
    fn an_unlinked_sensor_is_refused_and_the_others_still_found() {
        let devices = [
            device(0, "lost", &["camera.sensor/1"], None),
            device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
            device(2, "receiver", &["camera.receiver/1"], None),
        ];
        let found = cameras(&devices);
        assert_eq!(found.cameras.len(), 1);
        assert_eq!(found.cameras[0].name(), "sensor");
        assert_eq!(
            found.refused,
            [(
                "lost".to_string(),
                Refusal::Chain(ChainError::NotLinked("lost".into()))
            )]
        );
    }

    #[test]
    fn a_degraded_sensor_is_a_camera_that_carries_its_health_and_reason() {
        let mut sensor = device(0, "sensor", &["camera.sensor/1"], Some("receiver"));
        sensor.health = DeviceHealth::Degraded;
        sensor.reason = "the linked receiver cannot say what is wired to its input".to_string();
        let devices = [sensor, device(1, "receiver", &["camera.receiver/1"], None)];
        let found = cameras(&devices);
        assert!(found.refused.is_empty(), "{:?}", found.refused);
        let camera = &found.cameras[0];
        assert_eq!(camera.health(), DeviceHealth::Degraded);
        assert_eq!(
            camera.reason(),
            "the linked receiver cannot say what is wired to its input"
        );
        // Not checked yet: the wiring check is the contracts' and `discover` makes it.
        assert!(matches!(camera.link(), LinkCheck::Unknown(_)));
    }

    #[test]
    fn a_sensor_on_a_receiver_that_is_not_one_is_refused() {
        let devices = [
            device(0, "sensor", &["camera.sensor/1"], Some("fan")),
            device(1, "fan", &["fan"], None),
        ];
        let found = cameras(&devices);
        assert!(found.cameras.is_empty());
        assert!(matches!(
            found.refused.as_slice(),
            [(name, Refusal::Chain(ChainError::NotReceiver { device, .. }))] if name == "sensor" && device == "fan"
        ));
    }
}
