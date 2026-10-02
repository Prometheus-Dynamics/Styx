use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    /// Lists Styx cameras for `GstDeviceMonitor`.
    pub struct StyxDeviceProvider(ObjectSubclass<imp::StyxDeviceProvider>)
        @extends gst::DeviceProvider, gst::Object;
}

glib::wrapper! {
    /// A Styx camera as a `GstDevice`; its element is a `styxsrc` for that camera.
    pub struct StyxDevice(ObjectSubclass<imp::StyxDevice>)
        @extends gst::Device, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::DeviceProvider::register(
        Some(plugin),
        "styxdeviceprovider",
        gst::Rank::SECONDARY,
        StyxDeviceProvider::static_type(),
    )
}
