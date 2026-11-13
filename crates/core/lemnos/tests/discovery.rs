//! Finding the camera chain by tag and link, on device lists shaped as the Lemnos client returns
//! them.

use lemnos_client::{DeviceHealth, DeviceState};
use styx_lemnos::{ChainError, DeviceEntry, find_chain, find_chains};

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
fn finds_sensor_receiver_and_isp_by_tag_and_link() {
    let devices = [
        device(0, "other", &["gpio"], None),
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("isp")),
        device(3, "isp", &["camera.isp/1"], None),
    ];
    let chain = find_chain(&devices).expect("chain");
    assert_eq!(chain.sensor.name, "sensor");
    assert_eq!(chain.receiver.name, "receiver");
    assert_eq!(names(&chain.isps), ["isp"]);
}

fn names(devices: &[DeviceEntry]) -> Vec<&str> {
    devices.iter().map(|d| d.name.as_str()).collect()
}

#[test]
fn a_receiver_with_one_isp_then_two_follows_every_link_in_order() {
    let one = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("front")),
        device(3, "front", &["camera.isp/1"], None),
    ];
    assert_eq!(names(&find_chain(&one).unwrap().isps), ["front"]);

    let two = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("front")),
        device(3, "front", &["camera.isp/1"], Some("back")),
        device(4, "back", &["camera.isp/1"], None),
    ];
    let chain = find_chain(&two).expect("two isps");
    assert_eq!(names(&chain.isps), ["front", "back"]);
    assert_eq!(chain.sensor.name, "sensor");
    assert_eq!(chain.receiver.name, "receiver");
}

#[test]
fn a_sensor_linked_straight_to_an_isp_is_refused_naming_both() {
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("isp")),
        device(2, "isp", &["camera.isp/1"], None),
    ];
    assert!(matches!(
        find_chain(&devices),
        Err(ChainError::NotReceiver { device, .. }) if device == "isp"
    ));
}

#[test]
fn a_non_isp_after_the_receiver_is_refused_even_after_an_isp() {
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("front")),
        device(3, "front", &["camera.isp/1"], Some("fan")),
        device(4, "fan", &["fan"], None),
    ];
    assert!(matches!(
        find_chain(&devices),
        Err(ChainError::NotIsp { device, .. }) if device == "fan"
    ));
}

#[test]
fn a_link_that_names_no_device_is_refused_naming_both_ends() {
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("front")),
        device(3, "front", &["camera.isp/1"], Some("gone")),
    ];
    assert_eq!(
        find_chain(&devices),
        Err(ChainError::Dangling {
            from: "front".into(),
            to: "gone".into()
        })
    );
}

#[test]
fn a_link_cycle_is_refused_naming_the_devices_in_it() {
    // The ISP links back to the receiver: the circle is receiver -> front -> receiver.
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("front")),
        device(3, "front", &["camera.isp/1"], Some("receiver")),
    ];
    assert_eq!(
        find_chain(&devices),
        Err(ChainError::Cycle {
            path: vec!["receiver".into(), "front".into(), "receiver".into()]
        })
    );
    // A receiver linked to itself.
    let itself = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("receiver")),
    ];
    assert!(matches!(
        find_chain(&itself),
        Err(ChainError::Cycle { path }) if path == ["receiver", "receiver"]
    ));
}

#[test]
fn a_sensor_that_is_one_of_two_on_a_receiver_chains_through_it() {
    let devices = [
        device(1, "a", &["camera.sensor/1"], Some("rx")),
        device(2, "b", &["camera.sensor/1"], Some("rx")),
        device(3, "rx", &["camera.receiver/1"], Some("isp")),
        device(4, "isp", &["camera.isp/1"], None),
    ];
    let found = find_chains(&devices);
    assert_eq!(found.len(), 2);
    for chain in &found {
        let chain = chain.as_ref().expect("both sensors are cameras");
        assert_eq!(chain.receiver.name, "rx");
        assert_eq!(names(&chain.isps), ["isp"]);
    }
}

#[test]
fn a_receiver_with_no_isp_is_a_chain_without_one() {
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], None),
    ];
    assert!(find_chain(&devices).unwrap().isps.is_empty());
}

#[test]
fn no_sensor_and_two_sensors_are_refused() {
    assert_eq!(find_chain(&[]), Err(ChainError::NoSensor));
    let two = [
        device(1, "a", &["camera.sensor/1"], None),
        device(2, "b", &["camera.sensor/1"], None),
    ];
    assert_eq!(find_chain(&two), Err(ChainError::SensorCount(2)));
}

#[test]
fn a_sensor_with_no_link_or_a_link_to_the_wrong_kind_is_refused() {
    let unlinked = [device(1, "sensor", &["camera.sensor/1"], None)];
    assert_eq!(
        find_chain(&unlinked),
        Err(ChainError::NotLinked("sensor".into()))
    );
    let wrong = [
        device(1, "sensor", &["camera.sensor/1"], Some("pump")),
        device(2, "pump", &["fan"], None),
    ];
    assert!(matches!(
        find_chain(&wrong),
        Err(ChainError::NotReceiver { device, .. }) if device == "pump"
    ));
}

#[test]
fn a_receiver_linked_to_a_non_isp_is_refused() {
    let devices = [
        device(1, "sensor", &["camera.sensor/1"], Some("receiver")),
        device(2, "receiver", &["camera.receiver/1"], Some("fan")),
        device(3, "fan", &["fan"], None),
    ];
    assert!(matches!(
        find_chain(&devices),
        Err(ChainError::NotIsp { device, .. }) if device == "fan"
    ));
}

#[test]
fn find_chains_gives_one_entry_per_sensor_in_listing_order() {
    let devices = [
        device(0, "a", &["camera.sensor/1"], Some("rx")),
        device(1, "b", &["camera.sensor/1"], None),
        device(2, "rx", &["camera.receiver/1"], None),
        device(3, "c", &["camera.sensor/1"], Some("rx")),
    ];
    let found = find_chains(&devices);
    assert_eq!(found.len(), 3);
    let first = found[0].as_ref().expect("a is a camera");
    assert_eq!(first.sensor.name, "a");
    assert_eq!(first.receiver.name, "rx");
    assert_eq!(found[1], Err(ChainError::NotLinked("b".to_string())));
    let third = found[2].as_ref().expect("c is a camera");
    assert_eq!(third.sensor.name, "c");
    assert_eq!(third.receiver.name, "rx");
    assert!(find_chains(&[]).is_empty());
}
