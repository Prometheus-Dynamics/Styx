//! Finding UVC cameras in sysfs (`/sys/bus/usb/devices`), with no device access: identity,
//! speed, descriptors (the kernel's cached copy, readable by anyone) and which kernel driver
//! holds each of the camera's interfaces.

use std::path::{Path, PathBuf};

use styx_kernel::usbfs::{Speed, UsbDevice};

use crate::descriptors::UvcFunction;

/// The kernel driver bound to one of the camera's interfaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceBinding {
    pub number: u8,
    pub driver: Option<String>,
}

/// A UVC camera on the USB bus.
#[derive(Clone, Debug)]
pub struct UsbCameraInfo {
    /// `/sys/bus/usb/devices/3-1`.
    pub sysfs: PathBuf,
    /// The port path (`3-1`, `1-1.4`): stable across replugs into the same port.
    pub port: String,
    pub busnum: u32,
    pub devnum: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub speed: Speed,
    /// `usb-<host controller>-<devpath>`: what `uvcvideo` reports as the V4L2 `bus_info`.
    pub bus_info: String,
    pub function: UvcFunction,
    /// The video function's interfaces (control first) and their drivers.
    pub interfaces: Vec<InterfaceBinding>,
}

impl UsbCameraInfo {
    /// The key Styx uses for it: `usb:<port>`.
    pub fn key(&self) -> String {
        format!("usb:{}", self.port)
    }

    /// The usbfs node.
    pub fn node(&self) -> PathBuf {
        UsbDevice::node_path(self.busnum, self.devnum)
    }

    /// The product string, or `UVC Camera (vvvv:pppp)` as `uvcvideo` names it.
    pub fn name(&self) -> String {
        self.product.clone().unwrap_or_else(|| {
            format!(
                "UVC Camera ({:04x}:{:04x})",
                self.vendor_id, self.product_id
            )
        })
    }

    /// The driver of the video control interface (`uvcvideo` when the kernel has it).
    pub fn kernel_driver(&self) -> Option<&str> {
        self.interfaces
            .iter()
            .find(|i| i.number == self.function.control.number)
            .and_then(|i| i.driver.as_deref())
    }

    /// Whether a kernel driver other than usbfs holds one of the video interfaces.
    pub fn bound_to_kernel_driver(&self) -> bool {
        self.interfaces
            .iter()
            .any(|i| i.driver.as_deref().is_some_and(|d| d != "usbfs"))
    }

    /// Whether some process holds one of the video interfaces through usbfs.
    pub fn claimed_through_usbfs(&self) -> bool {
        self.interfaces
            .iter()
            .any(|i| i.driver.as_deref() == Some("usbfs"))
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn read_hex(path: &Path) -> Option<u16> {
    u16::from_str_radix(&read(path)?, 16).ok()
}

/// The host controller's device name (`xhci-hcd.1`, `0000:00:14.0`) of bus `busnum`.
fn hcd_name(root: &Path, busnum: u32) -> Option<String> {
    let real = std::fs::canonicalize(root.join(format!("usb{busnum}"))).ok()?;
    Some(real.parent()?.file_name()?.to_string_lossy().into_owned())
}

/// Reads one device directory; `None` when it is not a UVC camera.
pub fn read_device(root: &Path, dir: &Path) -> Option<std::result::Result<UsbCameraInfo, String>> {
    let port = dir.file_name()?.to_string_lossy().into_owned();
    if port.contains(':') || port.starts_with("usb") {
        return None;
    }
    // Interfaces of class 0x0e (video) and their drivers.
    let mut interfaces = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{port}:")) {
            continue;
        }
        let p = entry.path();
        if read(&p.join("bInterfaceClass")).as_deref() != Some("0e") {
            continue;
        }
        let Some(number) = read_hex(&p.join("bInterfaceNumber")) else {
            continue;
        };
        let driver = std::fs::read_link(p.join("driver"))
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
        interfaces.push(InterfaceBinding {
            number: number as u8,
            driver,
        });
    }
    if interfaces.is_empty() {
        return None;
    }
    interfaces.sort_by_key(|i| i.number);
    let raw = match std::fs::read(dir.join("descriptors")) {
        Ok(raw) => raw,
        Err(e) => return Some(Err(format!("{port}: descriptors: {e}"))),
    };
    let function = match UvcFunction::parse(&raw) {
        Ok(f) => f,
        Err(e) => return Some(Err(format!("{port}: {e}"))),
    };
    let ours: Vec<u8> = std::iter::once(function.control.number)
        .chain(function.streaming.iter().map(|s| s.number))
        .collect();
    interfaces.retain(|i| ours.contains(&i.number));
    let busnum = read(&dir.join("busnum"))?.parse().ok()?;
    let devnum = read(&dir.join("devnum"))?.parse().ok()?;
    let devpath = read(&dir.join("devpath")).unwrap_or_default();
    let bus_info = format!(
        "usb-{}-{devpath}",
        hcd_name(root, busnum).unwrap_or_else(|| format!("usb{busnum}"))
    );
    Some(Ok(UsbCameraInfo {
        sysfs: dir.to_owned(),
        busnum,
        devnum,
        vendor_id: read_hex(&dir.join("idVendor")).unwrap_or(function.device.vendor_id),
        product_id: read_hex(&dir.join("idProduct")).unwrap_or(function.device.product_id),
        manufacturer: read(&dir.join("manufacturer")),
        product: read(&dir.join("product")),
        serial: read(&dir.join("serial")),
        speed: read(&dir.join("speed")).map_or(Speed::Unknown, |s| Speed::from_mbps(&s)),
        bus_info,
        port,
        function,
        interfaces,
    }))
}

/// Every UVC camera under `root` (normally `/sys/bus/usb/devices`), and the problems met.
pub fn enumerate_in(root: &Path) -> (Vec<UsbCameraInfo>, Vec<String>) {
    let mut cams = Vec::new();
    let mut errors = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        match read_device(root, &dir) {
            Some(Ok(cam)) => cams.push(cam),
            Some(Err(e)) => errors.push(e),
            None => {}
        }
    }
    (cams, errors)
}

/// The sysfs directory of USB devices.
pub const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Every UVC camera on the system, and the problems met.
pub fn enumerate() -> (Vec<UsbCameraInfo>, Vec<String>) {
    enumerate_in(Path::new(SYSFS_USB_DEVICES))
}

/// The camera with key `key` (`usb:<port>`) or port `key`, if present now.
pub fn find(key: &str) -> Option<UsbCameraInfo> {
    let port = key.strip_prefix("usb:").unwrap_or(key);
    let root = Path::new(SYSFS_USB_DEVICES);
    read_device(root, &root.join(port))?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sysfs tree with the C270 recorded on the CM5.
    fn fake_sysfs(dir: &Path, vc_driver: Option<&str>) {
        let platform = dir.join("platform/xhci-hcd.1/usb3");
        let dev = platform.join("3-1");
        std::fs::create_dir_all(&dev).unwrap();
        let devices = dir.join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        std::os::unix::fs::symlink(&platform, devices.join("usb3")).unwrap();
        std::os::unix::fs::symlink(&dev, devices.join("3-1")).unwrap();
        let w = |p: &Path, s: &str| std::fs::write(p, s).unwrap();
        w(&dev.join("busnum"), "3\n");
        w(&dev.join("devnum"), "2\n");
        w(&dev.join("devpath"), "1\n");
        w(&dev.join("idVendor"), "046d\n");
        w(&dev.join("idProduct"), "0825\n");
        w(&dev.join("speed"), "480\n");
        w(&dev.join("serial"), "01A989E0\n");
        std::fs::write(
            dev.join("descriptors"),
            include_bytes!("../tests/data/c270.bin"),
        )
        .unwrap();
        let drivers = dir.join("drivers");
        std::fs::create_dir_all(drivers.join("uvcvideo")).unwrap();
        std::fs::create_dir_all(drivers.join("snd-usb-audio")).unwrap();
        for (n, class, driver) in [
            (0, "0e", vc_driver),
            (1, "0e", vc_driver),
            (2, "01", Some("snd-usb-audio")),
            (3, "01", Some("snd-usb-audio")),
        ] {
            let i = dev.join(format!("3-1:1.{n}"));
            std::fs::create_dir_all(&i).unwrap();
            w(&i.join("bInterfaceClass"), class);
            w(&i.join("bInterfaceNumber"), &format!("{n:02x}"));
            if let Some(d) = driver {
                std::os::unix::fs::symlink(drivers.join(d), i.join("driver")).unwrap();
            }
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("styx-uvc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn finds_the_c270_and_its_drivers() {
        let d = tmp("sysfs-bound");
        fake_sysfs(&d, Some("uvcvideo"));
        let (cams, errors) = enumerate_in(&d.join("devices"));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(cams.len(), 1);
        let c = &cams[0];
        assert_eq!(c.key(), "usb:3-1");
        assert_eq!(c.name(), "UVC Camera (046d:0825)");
        assert_eq!(c.bus_info, "usb-xhci-hcd.1-1");
        assert_eq!(c.node(), PathBuf::from("/dev/bus/usb/003/002"));
        assert_eq!(c.speed, Speed::High);
        assert_eq!(c.kernel_driver(), Some("uvcvideo"));
        assert!(c.bound_to_kernel_driver());
        // The audio interfaces are not the camera's.
        assert_eq!(c.interfaces.len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_unbound_camera_is_free() {
        let d = tmp("sysfs-free");
        fake_sysfs(&d, None);
        let (cams, _) = enumerate_in(&d.join("devices"));
        assert!(!cams[0].bound_to_kernel_driver());
        assert_eq!(cams[0].kernel_driver(), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
