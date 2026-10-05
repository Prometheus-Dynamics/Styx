//! Cameras coming and going: kernel uevents (netlink, through Lemnos's `lemnos_linux::uevent`)
//! trigger a sysfs rescan, and the difference is reported. Without a netlink socket (some containers) it rescans on every
//! poll instead.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use lemnos_linux::uevent::{UeventRecv, UeventSocket};

use crate::sysfs::{self, UsbCameraInfo};

/// A change in the cameras present.
#[derive(Clone, Debug)]
pub enum HotplugEvent {
    /// A camera appeared (plugged in, authorized, or reconfigured).
    Added(UsbCameraInfo),
    /// A camera went away; its key.
    Removed(String),
    /// The drivers holding a camera's interfaces changed (`uvcvideo` bound or unbound, a
    /// process claimed it through usbfs).
    DriversChanged(UsbCameraInfo),
}

fn identity(c: &UsbCameraInfo) -> (u32, u32, String) {
    (c.busnum, c.devnum, c.bus_info.clone())
}

/// Watches for UVC cameras.
pub struct Hotplug {
    socket: Option<UeventSocket>,
    known: BTreeMap<String, UsbCameraInfo>,
    /// uevents come in bursts (one per interface): wait this long after the last one.
    settle: Duration,
}

impl std::fmt::Debug for Hotplug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hotplug")
            .field("netlink", &self.socket.is_some())
            .field("known", &self.known.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Hotplug {
    /// Starts watching; the cameras present now are [`Hotplug::cameras`].
    pub fn new() -> Hotplug {
        let socket = UeventSocket::open()
            .inspect_err(|_e| {
                #[cfg(feature = "tracing")]
                tracing::debug!(error = %_e, "uvc hotplug: no netlink, rescanning");
            })
            .ok();
        let known = sysfs::enumerate()
            .0
            .into_iter()
            .map(|c| (c.key(), c))
            .collect();
        Hotplug {
            socket,
            known,
            settle: Duration::from_millis(50),
        }
    }

    /// The cameras present as of the last poll.
    pub fn cameras(&self) -> impl Iterator<Item = &UsbCameraInfo> {
        self.known.values()
    }

    /// Whether kernel uevents are available (otherwise every poll rescans).
    pub fn uses_uevents(&self) -> bool {
        self.socket.is_some()
    }

    /// Waits up to `timeout` for changes and returns them (empty on a timeout).
    pub fn poll(&mut self, timeout: Duration) -> Vec<HotplugEvent> {
        let deadline = Instant::now() + timeout;
        let Some(sock) = &mut self.socket else {
            std::thread::sleep(timeout);
            return self.rescan();
        };
        let mut relevant = false;
        loop {
            match sock.recv() {
                Ok(Some(UeventRecv::Event(ev))) => {
                    relevant |= ev.subsystem() == Some("usb");
                    continue;
                }
                Ok(None) => {}
                // Overrun (events lost) or a failed receive: something happened, rescan.
                Ok(Some(UeventRecv::Overflow)) | Err(_) => relevant = true,
            }
            let now = Instant::now();
            let wait = if relevant {
                self.settle
            } else if now < deadline {
                deadline - now
            } else {
                return Vec::new();
            };
            match sock.wait(Some(wait)) {
                Ok(true) => continue,
                _ if relevant => return self.rescan(),
                _ => {}
            }
        }
    }

    /// Compares sysfs with what was known and reports the difference.
    pub fn rescan(&mut self) -> Vec<HotplugEvent> {
        let now: BTreeMap<String, UsbCameraInfo> = sysfs::enumerate()
            .0
            .into_iter()
            .map(|c| (c.key(), c))
            .collect();
        let events = diff(&self.known, &now);
        self.known = now;
        events
    }
}

impl Default for Hotplug {
    fn default() -> Self {
        Hotplug::new()
    }
}

fn diff(
    old: &BTreeMap<String, UsbCameraInfo>,
    new: &BTreeMap<String, UsbCameraInfo>,
) -> Vec<HotplugEvent> {
    let mut events = Vec::new();
    for (key, c) in old {
        match new.get(key) {
            None => events.push(HotplugEvent::Removed(key.clone())),
            // Same port, another device (replugged between scans).
            Some(n) if identity(n) != identity(c) => {
                events.push(HotplugEvent::Removed(key.clone()));
                events.push(HotplugEvent::Added(n.clone()));
            }
            Some(n) if n.interfaces != c.interfaces => {
                events.push(HotplugEvent::DriversChanged(n.clone()));
            }
            Some(_) => {}
        }
    }
    for (key, c) in new {
        if !old.contains_key(key) {
            events.push(HotplugEvent::Added(c.clone()));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::InterfaceBinding;

    fn cam(port: &str, devnum: u32, driver: Option<&str>) -> UsbCameraInfo {
        let function =
            crate::descriptors::UvcFunction::parse(include_bytes!("../tests/data/c270.bin"))
                .unwrap();
        UsbCameraInfo {
            sysfs: format!("/sys/bus/usb/devices/{port}").into(),
            port: port.into(),
            busnum: 3,
            devnum,
            vendor_id: 0x046d,
            product_id: 0x0825,
            manufacturer: None,
            product: None,
            serial: None,
            speed: styx_kernel::usbfs::Speed::High,
            bus_info: format!("usb-xhci-hcd.1-{port}"),
            function,
            interfaces: vec![
                InterfaceBinding {
                    number: 0,
                    driver: driver.map(Into::into),
                },
                InterfaceBinding {
                    number: 1,
                    driver: driver.map(Into::into),
                },
            ],
        }
    }

    fn map(cams: &[UsbCameraInfo]) -> BTreeMap<String, UsbCameraInfo> {
        cams.iter().map(|c| (c.key(), c.clone())).collect()
    }

    #[test]
    fn reports_adds_removes_replugs_and_driver_changes() {
        let a = map(&[cam("3-1", 2, Some("uvcvideo"))]);
        let none = map(&[]);
        assert!(matches!(&diff(&a, &none)[..], [HotplugEvent::Removed(k)] if k == "usb:3-1"));
        assert!(matches!(&diff(&none, &a)[..], [HotplugEvent::Added(c)] if c.devnum == 2));
        let replugged = map(&[cam("3-1", 5, Some("uvcvideo"))]);
        let ev = diff(&a, &replugged);
        assert!(
            matches!(&ev[..], [HotplugEvent::Removed(_), HotplugEvent::Added(c)] if c.devnum == 5)
        );
        let unbound = map(&[cam("3-1", 2, None)]);
        assert!(
            matches!(&diff(&a, &unbound)[..], [HotplugEvent::DriversChanged(c)] if !c.bound_to_kernel_driver())
        );
        assert!(diff(&a, &a).is_empty());
    }
}
