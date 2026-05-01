use crate::ProbedDevice;

use super::{CaptureError, CaptureRequest, CaptureStartPolicy, StyxConfig};

#[derive(Clone, Debug)]
pub struct CaptureSource {
    device: ProbedDevice,
}

impl CaptureSource {
    pub fn new(device: ProbedDevice) -> Self {
        Self { device }
    }

    pub fn device(&self) -> &ProbedDevice {
        &self.device
    }

    pub fn into_device(self) -> ProbedDevice {
        self.device
    }

    pub fn capture_request(&self) -> CaptureRequest<'_> {
        CaptureRequest::new(&self.device)
    }

    pub fn open(&self) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start()
    }

    pub fn open_with_config(
        &self,
        config: StyxConfig,
    ) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.capture_request().config(config).start()
    }

    pub fn open_with_policy(
        &self,
        policy: CaptureStartPolicy,
    ) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start_with_policy(policy)
    }

    pub fn pipeline(&self) -> crate::session::MediaPipelineBuilder<'_> {
        crate::session::MediaPipelineBuilder::new(self.capture_request())
    }
}

impl AsRef<ProbedDevice> for CaptureSource {
    fn as_ref(&self) -> &ProbedDevice {
        self.device()
    }
}

impl From<ProbedDevice> for CaptureSource {
    fn from(device: ProbedDevice) -> Self {
        Self::new(device)
    }
}

impl From<CaptureSource> for ProbedDevice {
    fn from(source: CaptureSource) -> Self {
        source.into_device()
    }
}
