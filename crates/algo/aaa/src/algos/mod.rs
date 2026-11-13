//! The algorithms. AF, ALSC, denoise and lux need their features (on with `std`); their
//! tuning types are always there.

pub mod af;
pub mod agc;
pub mod alsc;
pub mod awb;
pub mod black_level;
pub mod ccm;
pub mod contrast;
pub mod denoise;
pub mod lux;

#[cfg(feature = "af")]
pub use af::Af;
pub use agc::Agc;
#[cfg(feature = "alsc")]
pub use alsc::Alsc;
pub use awb::Awb;
pub use black_level::BlackLevel;
pub use ccm::Ccm;
pub use contrast::Contrast;
#[cfg(feature = "denoise")]
pub use denoise::Denoise;
#[cfg(feature = "lux")]
pub use lux::Lux;
