use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    /// `styxsrc`: live frames from a Styx camera.
    pub struct StyxSrc(ObjectSubclass<imp::StyxSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "styxsrc",
        gst::Rank::SECONDARY,
        StyxSrc::static_type(),
    )
}
