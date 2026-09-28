//! The algorithms.

pub mod agc;
pub mod alsc;
pub mod awb;
pub mod black_level;
pub mod ccm;
pub mod contrast;
pub mod lux;

pub use agc::Agc;
pub use alsc::Alsc;
pub use awb::Awb;
pub use black_level::BlackLevel;
pub use ccm::Ccm;
pub use contrast::Contrast;
pub use lux::Lux;
