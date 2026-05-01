//! MJPEG decoders across backends.

#[cfg(feature = "codec-jpeg-decoder")]
pub use crate::mjpeg::MjpegDecoder;
#[cfg(feature = "codec-turbojpeg")]
pub use crate::mjpeg_turbojpeg::TurbojpegDecoder;
#[cfg(feature = "codec-zune")]
pub use crate::mjpeg_zune::ZuneMjpegDecoder;
