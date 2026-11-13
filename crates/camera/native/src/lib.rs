//! Native cameras on a Lemnos board: every `camera.sensor/1` device, with its receiver and ISP.
//!
//! A native camera is a sensor and the chain `link` leads from it: its receiver and, when it has
//! one, its ISP. styx-lemnos's [`find_chain`] resolves one such chain, by tag and link, and refuses
//! a board with more than one sensor. [`cameras`] resolves every sensor on the board, one camera
//! each, so a board with two sensors gives two cameras.
//!
//! The device list is the Lemnos service's. Nothing here walks sysfs or a media graph; the
//! hardware facts are Lemnos's, and this crate only says which devices make a camera.
//!
//! Finding a chain does not claim it. One backend owns a camera at a time, and two cameras may
//! name the same receiver on a board; choosing between them is for the layer that opens a camera.

use styx_lemnos::{Chain, ChainError, Client, DeviceEntry, Error, SENSOR_TAG, find_chain};

/// A camera: a sensor and the receiver and ISP its `link` leads to, as Lemnos names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    chain: Chain,
}

impl Camera {
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

    /// The ISP the receiver links to, when it has one.
    pub fn isp(&self) -> Option<&DeviceEntry> {
        self.chain.isp.as_ref()
    }
}

/// Every camera on a board, and the sensors that did not resolve to one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// The cameras, in the order the board lists their sensors.
    pub cameras: Vec<Camera>,
    /// Each sensor that is not a camera, by its name, with why (no link, a link to the wrong kind
    /// of device, and so on).
    pub refused: Vec<(String, ChainError)>,
}

fn has_tag(device: &DeviceEntry, tag: &str) -> bool {
    device.tags.iter().any(|t| t == tag)
}

/// `devices` with every sensor but `sensor` left out, so [`find_chain`] sees exactly one sensor.
/// Receivers and ISPs stay, since a chain may run through them.
fn one_sensor_view(devices: &[DeviceEntry], sensor: &str) -> Vec<DeviceEntry> {
    devices
        .iter()
        .filter(|d| d.name == sensor || !has_tag(d, SENSOR_TAG))
        .cloned()
        .collect()
}

/// Resolves every `camera.sensor/1` device in `devices` to its chain.
pub fn cameras(devices: &[DeviceEntry]) -> Discovery {
    let mut found = Discovery::default();
    for sensor in devices.iter().filter(|d| has_tag(d, SENSOR_TAG)) {
        let view = one_sensor_view(devices, &sensor.name);
        match find_chain(&view) {
            Ok(chain) => found.cameras.push(Camera { chain }),
            Err(why) => found.refused.push((sensor.name.clone(), why)),
        }
    }
    found
}

/// Lists the board's devices through `client` and returns its cameras.
pub fn discover(client: &Client) -> Result<Discovery, Error> {
    let devices = client.list().map_err(Error::Client)?;
    Ok(cameras(&devices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemnos_client::{DeviceHealth, DeviceState};

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
        assert_eq!(camera.isp().map(|d| d.name.as_str()), Some("isp"));
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
        assert!(found.cameras.iter().all(|c| c.isp().is_none()));
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
            [("lost".to_string(), ChainError::NotLinked("lost".into()))]
        );
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
            [(name, ChainError::NotReceiver { device, .. })] if name == "sensor" && device == "fan"
        ));
    }
}
