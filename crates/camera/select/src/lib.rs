//! Choosing a backend per camera, and reading the chosen camera the same way whichever backend
//! owns it.
//!
//! The chooser ([`select`] and [`plan`], in [`choose`]) takes what each backend lists, through the
//! [`NativeLister`], [`LibcameraLister`] and [`V4l2Lister`] traits, and returns one owner per
//! camera with its reason. The real listers are in [`real`]. [`Capture`] is the one frame, control
//! and status surface; [`adapters`] puts the three backends' streams behind it.
//!
//! Rules, identity and the open questions are in the [`choose`] module documentation and the
//! PR text.

pub mod adapters;
pub mod capture;
pub mod choose;
pub mod listing;
pub mod real;

pub use capture::{Capture, CaptureError, RateReport, Status};
pub use choose::{Config, Plan, Reason, Scope, SelectError, Selected, Skip, Skipped, plan, select};
pub use listing::{Anchor, Backend, LibcameraLister, ListError, Listed, NativeLister, V4l2Lister};
