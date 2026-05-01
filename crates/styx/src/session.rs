//! Unified pipeline that wires capture, decode, hook, and encode in one object.

#[cfg(feature = "hooks")]
use styx_codec::prelude::FrameLease;

#[cfg(feature = "hooks")]
type HookFn = Box<dyn FnMut(FrameLease) -> FrameLease + Send>;
#[cfg(feature = "hooks")]
type FrameHookFn = Box<dyn FnMut(FrameLease) -> FrameLease + Send>;

#[cfg(feature = "hooks")]
enum HookStore<T> {
    Local(Option<T>),
}

#[cfg(feature = "hooks")]
impl<T> HookStore<T> {
    fn take(&mut self) -> T {
        match self {
            HookStore::Local(h) => h.take().expect("hook missing"),
        }
    }

    fn put(&mut self, hook: T) {
        match self {
            HookStore::Local(h) => *h = Some(hook),
        }
    }
}

mod builder;
mod runtime;

pub use builder::{MediaPipelineBuilder, PipelineExecutionMode};
pub use runtime::{MediaPipeline, MediaPipelineFrameIter};
