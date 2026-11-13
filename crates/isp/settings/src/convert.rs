//! From the controller's neutral settings (`styx_controller::IspSettings`), field for field.

use alloc::vec::Vec;

use crate::IspSettings;

impl From<styx_controller::IspSettings> for IspSettings {
    /// Every field is copied. The controller has no outputs, so `outputs` starts empty.
    fn from(c: styx_controller::IspSettings) -> Self {
        // Destructured in full: a field added to the controller's settings fails to compile here
        // until it is mapped.
        let styx_controller::IspSettings {
            from_frame,
            black_level,
            black_channels,
            wb,
            digital_gain,
            flicker,
            flicker_bands,
            ccm,
            gamma,
            lens_shading,
            denoise,
            sharpen,
            histogram_weights,
        } = c;
        Self {
            from_frame,
            black_level,
            black_channels,
            wb,
            digital_gain,
            flicker,
            flicker_bands,
            ccm,
            gamma,
            lens_shading,
            denoise,
            sharpen,
            histogram_weights,
            outputs: Vec::new(),
        }
    }
}
