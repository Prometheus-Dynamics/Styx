//! The ways out: where each camera's frames go, besides the service's own status.
//!
//! The service makes one [`Outlet`] per camera with [`Outlets::attach`] and calls it on the
//! camera's thread for every frame, with the frame's lease. The lease is released when the call
//! returns (drop it or keep it, as the outlet needs). Nothing in this crate reaches the frames
//! any other way.
//!
//! The socket way out (a `styx-socket` producer per camera, on the camera's own frame pool) is
//! not wired here, see the crate documentation. Orion and Daedalus outlets are other crates'
//! features and are not wired here either: they implement [`Outlets`] and are passed to the
//! service by whoever builds it.

use styx_controls::FrameMetadata;
use styx_frame::FrameLease;

/// One camera's way out. Called on that camera's thread.
pub trait Outlet: Send {
    /// A frame from camera `camera`.
    fn frame(&mut self, camera: &str, lease: &FrameLease<FrameMetadata>);
}

/// Makes each camera's outlet. Shared by the service's threads.
pub trait Outlets: Send + Sync {
    /// The outlet for camera `camera`, made once per camera when its worker starts.
    fn attach(&self, camera: &str) -> Box<dyn Outlet>;
}

/// The default: frames go nowhere. Each lease is released as soon as the call returns.
#[derive(Debug, Default, Clone, Copy)]
pub struct Discard;

#[derive(Debug)]
struct DiscardOutlet;

impl Outlet for DiscardOutlet {
    fn frame(&mut self, _camera: &str, _lease: &FrameLease<FrameMetadata>) {}
}

impl Outlets for Discard {
    fn attach(&self, _camera: &str) -> Box<dyn Outlet> {
        Box::new(DiscardOutlet)
    }
}
