use std::sync::{LazyLock, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use styx::prelude::*;

use crate::caps;

#[derive(Default)]
pub struct StyxDeviceProvider;

#[glib::object_subclass]
impl ObjectSubclass for StyxDeviceProvider {
    const NAME: &'static str = "GstStyxDeviceProvider";
    type Type = super::StyxDeviceProvider;
    type ParentType = gst::DeviceProvider;
}

impl ObjectImpl for StyxDeviceProvider {}
impl GstObjectImpl for StyxDeviceProvider {}

impl DeviceProviderImpl for StyxDeviceProvider {
    fn metadata() -> Option<&'static gst::subclass::DeviceProviderMetadata> {
        static METADATA: LazyLock<gst::subclass::DeviceProviderMetadata> = LazyLock::new(|| {
            gst::subclass::DeviceProviderMetadata::new(
                "Styx camera provider",
                "Source/Video",
                "Lists cameras Styx can open (V4L2, native sensors, libcamera)",
                "Mathias Petersen",
            )
        });
        Some(&*METADATA)
    }

    fn probe(&self) -> Vec<gst::Device> {
        probe_all()
            .into_iter()
            .map(|device| super::StyxDevice::new(&device).upcast())
            .collect()
    }
}

/// What a device's element selects its camera by (exactly): a backend's node path when it has
/// one, else the camera's display name (for USB cameras, its bus path).
fn selector(device: &ProbedDevice) -> String {
    device
        .backends
        .iter()
        .flat_map(|b| b.properties.iter())
        .find(|(k, _)| k == "path")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| device.identity.display.clone())
}

impl super::StyxDevice {
    pub fn new(device: &ProbedDevice) -> Self {
        let backends: Vec<String> = device
            .backends
            .iter()
            .map(|b| format!("{:?}", b.kind).to_ascii_lowercase())
            .collect();
        let properties = gst::Structure::builder("styx-device")
            .field("styx.camera", selector(device))
            .field("styx.backends", backends.join(","))
            .field("styx.keys", device.identity.keys.join(","))
            .build();
        let obj: Self = glib::Object::builder()
            .property("display-name", &device.identity.display)
            .property("device-class", "Video/Source")
            .property("caps", caps::device_caps(device))
            .property("properties", properties)
            .build();
        *obj.imp().camera.lock().unwrap_or_else(|e| e.into_inner()) = selector(device);
        obj
    }
}

#[derive(Default)]
pub struct StyxDevice {
    camera: Mutex<String>,
}

#[glib::object_subclass]
impl ObjectSubclass for StyxDevice {
    const NAME: &'static str = "GstStyxDevice";
    type Type = super::StyxDevice;
    type ParentType = gst::Device;
}

impl ObjectImpl for StyxDevice {}
impl GstObjectImpl for StyxDevice {}

impl DeviceImpl for StyxDevice {
    fn create_element(&self, name: Option<&str>) -> Result<gst::Element, gst::LoggableError> {
        let camera = self
            .camera
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut builder = gst::ElementFactory::make("styxsrc").property("camera", camera);
        if let Some(name) = name {
            builder = builder.name(name);
        }
        builder.build().map_err(|err| {
            gst::loggable_error!(
                gst::CAT_RUST,
                "cannot create styxsrc (is the plugin registered?): {err}"
            )
        })
    }
}
