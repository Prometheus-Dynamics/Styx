//! MJPEG decoders across backends.

pub use crate::mjpeg::MjpegDecoder;
#[cfg(feature = "codec-turbojpeg")]
pub use crate::mjpeg_turbojpeg::TurbojpegDecoder;
#[cfg(feature = "codec-zune")]
pub use crate::mjpeg_zune::ZuneMjpegDecoder;
