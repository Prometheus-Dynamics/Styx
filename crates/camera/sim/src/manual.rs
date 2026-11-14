//! A `camera.sensor/1` double that the test drives frame by frame: no daemon, no driver.
//!
//! A `post_values` written during frame `w` shows from frame `w + delay` for each control, the way
//! the mode's `exposure_delay`, `gain_delay` and `frame_length_delay` say. The test sets the frame
//! the sensor is in. This is the sensor the `styx-native-controls` tests check their arithmetic
//! against; the lock-step camera ([`crate::SimCamera`]) is the same clock behind a daemon.

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_contract_camera_sensor::{CameraSensor, Controls, FrameValues, Mode, SensorInfo};
use lemnos_core::{Result, Status};

/// plus that control's delay. `frame` is the frame the sensor is in; the test sets it.
#[derive(Debug)]
pub struct ManualSensor {
    delays: [u64; 3],
    pub frame: u64,
    /// `(first frame shown, code)` per control, in landing order; index 0 is the initial value.
    pub exposure: Vec<(u64, u32)>,
    pub gain: Vec<(u64, u32)>,
    pub frame_length: Vec<(u64, u32)>,
    /// Frame each `post_values` was made in, with the identity Styx passed (opaque to Lemnos).
    pub posts: Vec<(u64, u64)>,
}

impl ManualSensor {
    pub fn new(mode: &Mode) -> Self {
        Self {
            delays: [
                u64::from(mode.exposure_delay),
                u64::from(mode.gain_delay),
                u64::from(mode.frame_length_delay),
            ],
            frame: 0,
            exposure: Vec::new(),
            gain: Vec::new(),
            frame_length: Vec::new(),
            posts: Vec::new(),
        }
    }

    fn in_force(list: &[(u64, u32)], frame: u64) -> u32 {
        list.iter()
            .rev()
            .find(|&&(first, _)| first <= frame)
            .or(list.first())
            .map_or(0, |&(_, code)| code)
    }

    pub fn exposure_at(&self, frame: u64) -> u32 {
        Self::in_force(&self.exposure, frame)
    }

    pub fn gain_at(&self, frame: u64) -> u32 {
        Self::in_force(&self.gain, frame)
    }

    /// The frame's brightness: the exposure code times the gain code in force on it.
    pub fn brightness(&self, frame: u64) -> u64 {
        u64::from(self.exposure_at(frame)) * u64::from(self.gain_at(frame))
    }
}

impl CameraSensor for ManualSensor {
    fn identity(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }
    fn info(&mut self) -> Result<SensorInfo> {
        Ok(SensorInfo::default())
    }

    fn mode(&mut self, _index: u16) -> Result<Mode> {
        Err(Status::NOT_FOUND)
    }

    fn select_mode(&mut self, _index: u16) -> Result<()> {
        Ok(())
    }

    fn selected(&mut self) -> Result<Option<u16>> {
        Ok(Some(0))
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    /// Before streaming: the value is shown from frame 0.
    fn set_controls(&mut self, controls: Controls) -> Result<()> {
        if let Some(e) = controls.exposure {
            self.exposure = vec![(0, e)];
        }
        if let Some(g) = controls.analogue_gain {
            self.gain = vec![(0, g)];
        }
        if let Some(fl) = controls.frame_length {
            self.frame_length = vec![(0, fl)];
        }
        Ok(())
    }

    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        let c = values.controls;
        if let Some(e) = c.exposure {
            self.exposure.push((self.frame + self.delays[0], e));
        }
        if let Some(g) = c.analogue_gain {
            self.gain.push((self.frame + self.delays[1], g));
        }
        if let Some(fl) = c.frame_length {
            self.frame_length.push((self.frame + self.delays[2], fl));
        }
        self.posts.push((self.frame, values.identity));
        Ok(())
    }

    fn applied(&mut self) -> Result<Option<FrameValues>> {
        Ok(None)
    }

    fn set_flips(&mut self, _horizontal: bool, _vertical: bool) -> Result<BayerOrder> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_test_pattern(&mut self, _code: u16) -> Result<()> {
        Err(Status::NOT_SUPPORTED)
    }
}
