//! Common control ids (`V4L2_CID_*`, linux/v4l2-controls.h).

/// User class base.
pub const USER_BASE: u32 = 0x0098_0900;
/// Camera class base.
pub const CAMERA_CLASS_BASE: u32 = 0x009a_0900;
/// Image source class base (sensor timing and gain).
pub const IMAGE_SOURCE_CLASS_BASE: u32 = 0x009e_0900;
/// Image processing class base (link frequency, pixel rate, test pattern).
pub const IMAGE_PROC_CLASS_BASE: u32 = 0x009f_0900;

/// Brightness.
pub const BRIGHTNESS: u32 = USER_BASE;
/// Contrast.
pub const CONTRAST: u32 = USER_BASE + 1;
/// Saturation.
pub const SATURATION: u32 = USER_BASE + 2;
/// Hue.
pub const HUE: u32 = USER_BASE + 3;
/// Automatic white balance.
pub const AUTO_WHITE_BALANCE: u32 = USER_BASE + 12;
/// Red balance.
pub const RED_BALANCE: u32 = USER_BASE + 14;
/// Blue balance.
pub const BLUE_BALANCE: u32 = USER_BASE + 15;
/// Gamma.
pub const GAMMA: u32 = USER_BASE + 16;
/// Exposure (sensor drivers: in lines).
pub const EXPOSURE: u32 = USER_BASE + 17;
/// Automatic gain.
pub const AUTOGAIN: u32 = USER_BASE + 18;
/// Gain.
pub const GAIN: u32 = USER_BASE + 19;
/// Horizontal flip.
pub const HFLIP: u32 = USER_BASE + 20;
/// Vertical flip.
pub const VFLIP: u32 = USER_BASE + 21;
/// Power line frequency filter.
pub const POWER_LINE_FREQUENCY: u32 = USER_BASE + 24;
/// White balance temperature.
pub const WHITE_BALANCE_TEMPERATURE: u32 = USER_BASE + 26;
/// Sharpness.
pub const SHARPNESS: u32 = USER_BASE + 27;
/// Backlight compensation.
pub const BACKLIGHT_COMPENSATION: u32 = USER_BASE + 28;

/// Auto exposure mode (menu).
pub const EXPOSURE_AUTO: u32 = CAMERA_CLASS_BASE + 1;
/// Absolute exposure time in 100 µs units.
pub const EXPOSURE_ABSOLUTE: u32 = CAMERA_CLASS_BASE + 2;
/// Allow frame rate to drop for exposure.
pub const EXPOSURE_AUTO_PRIORITY: u32 = CAMERA_CLASS_BASE + 3;
/// Pan.
pub const PAN_ABSOLUTE: u32 = CAMERA_CLASS_BASE + 8;
/// Tilt.
pub const TILT_ABSOLUTE: u32 = CAMERA_CLASS_BASE + 9;
/// Focus position.
pub const FOCUS_ABSOLUTE: u32 = CAMERA_CLASS_BASE + 10;
/// Continuous autofocus.
pub const FOCUS_AUTO: u32 = CAMERA_CLASS_BASE + 12;
/// Zoom.
pub const ZOOM_ABSOLUTE: u32 = CAMERA_CLASS_BASE + 13;
/// Camera orientation (front/back/external).
pub const CAMERA_ORIENTATION: u32 = CAMERA_CLASS_BASE + 34;
/// Sensor mounting rotation in degrees.
pub const CAMERA_SENSOR_ROTATION: u32 = CAMERA_CLASS_BASE + 35;

/// Vertical blanking in lines.
pub const VBLANK: u32 = IMAGE_SOURCE_CLASS_BASE + 1;
/// Horizontal blanking in pixels.
pub const HBLANK: u32 = IMAGE_SOURCE_CLASS_BASE + 2;
/// Analogue gain (sensor-specific units).
pub const ANALOGUE_GAIN: u32 = IMAGE_SOURCE_CLASS_BASE + 3;
/// Notify the sensor of colour gains.
pub const NOTIFY_GAINS: u32 = IMAGE_SOURCE_CLASS_BASE + 9;

/// CSI-2 link frequency (integer menu, Hz).
pub const LINK_FREQ: u32 = IMAGE_PROC_CLASS_BASE + 1;
/// Pixel rate in pixels per second (64-bit).
pub const PIXEL_RATE: u32 = IMAGE_PROC_CLASS_BASE + 2;
/// Test pattern (menu).
pub const TEST_PATTERN: u32 = IMAGE_PROC_CLASS_BASE + 3;
/// Digital gain.
pub const DIGITAL_GAIN: u32 = IMAGE_PROC_CLASS_BASE + 5;

/// The control class of an id (`V4L2_CTRL_ID2CLASS`).
pub const fn class_of(id: u32) -> u32 {
    id & 0x0fff_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_header() {
        assert_eq!(EXPOSURE, 0x0098_0911);
        assert_eq!(VBLANK, 0x009e_0901);
        assert_eq!(PIXEL_RATE, 0x009f_0902);
        assert_eq!(class_of(LINK_FREQ), 0x009f_0000);
    }
}
