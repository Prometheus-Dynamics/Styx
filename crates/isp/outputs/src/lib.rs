//! The outputs an ISP is configured with for the sizes and formats its consumers ask for.
//!
//! Consumers ask for a picture format at a size ([`Ask`]). For one capture, [`plan`] decides
//! which of the ISP's outputs ([`IspCapability`]: how many, the sizes each makes, the formats
//! each writes) serve them:
//!
//! * a consumer asking for a size and format an output makes gets that output ([`Route::Isp`]);
//! * consumers asking for the same size and format share one output, and so one frame: each
//!   takes a share of the same lease, no copy ([`Route::Isp`] with `shared`);
//! * a consumer asking for an ISP output's size in another format of the same family is served
//!   by the converter of `styx-scale` ([`Route::Converted`]). That is the one fallback, and it is
//!   reported as a conversion, never hidden;
//! * anything else (a size larger than the capture, a size or format no output makes) is refused
//!   with the reason ([`PlanError`]).
//!
//! [`Plan::apply`] fills `IspSettings::outputs`, which the ISP maps into its own outputs.
//! [`Distributor`] then serves the frames: one pool per configured output, made once, and one
//! ISP pass per output for each capture frame (the producer is a closure, so any ISP can run
//! it).
//!
//! There is no route planner and no cost model. Crops, pyramids and sizes beyond the ISP's own
//! outputs are graph work, not this crate's. Captures are not upscaled.
//!
//! The ISPs: [`PISP`] and [`GPU`] are described as their crates expose them today; the soft ISP
//! is described in this crate's tests. What each would need to expose is in the PR.

mod capability;
mod distribute;
mod plan;

pub use capability::{Described, GPU, IspCapability, OutputCapability, PISP, SizeRule};
pub use distribute::{BuildError, DeliverError, Delivery, Distributor};
pub use plan::{Ask, Conversion, Output, Plan, PlanError, Route, plan};
