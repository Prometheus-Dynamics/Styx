//! The focus lens of a camera module, moved frame-exactly, and the position each frame saw.
//!
//! Moves follow the control schedule's frame starts: a position for frame `F` is written at
//! the start of `F - delay` ([`LensSchedule`]), and every frame reports the position predicted
//! for its exposure from the moves and the lens's settle time. Phase detection data from the
//! embedded data (IMX708) is decoded per frame ([`PdafFrames`]).
//!
//! The actuator is any [`styx_hal::LensActuator`] (a kernel lens driver on Linux, a VCM over an
//! embedded-hal I²C bus); it is boxed, because moves are rare and a lens is optional.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::time::Duration;

use styx_hal::LensActuator;
use styx_sensor::lens::imx708_pdaf;
use styx_sensor::{LensDescription, LensFrame, LensSchedule};

use crate::error::{Error, Result};
use crate::sync::MaybeSend;

/// A lens actuator behind a box: [`LensActuator`] with its error as a message.
pub trait LensDrive: MaybeSend {
    /// Powers the actuator up or down.
    fn power(&mut self, on: bool) -> core::result::Result<(), String>;
    /// Moves to `position` (driver units).
    fn move_to(&mut self, position: i32) -> core::result::Result<(), String>;
}

impl<L: LensActuator + MaybeSend> LensDrive for L {
    fn power(&mut self, on: bool) -> core::result::Result<(), String> {
        LensActuator::power(self, on).map_err(|e| e.to_string())
    }

    fn move_to(&mut self, position: i32) -> core::result::Result<(), String> {
        LensActuator::move_to(self, position).map_err(|e| e.to_string())
    }
}

/// The lens side of a camera's controls.
pub struct LensControl {
    actuator: Box<dyn LensDrive>,
    schedule: LensSchedule,
    range: [i32; 2],
    /// Frame starts seen (sequence, time on the platform's monotonic clock), the latest last.
    starts: VecDeque<(u64, Duration)>,
    powered: bool,
}

impl core::fmt::Debug for LensControl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LensControl")
            .field("range", &self.range)
            .field("target", &self.schedule.target())
            .finish_non_exhaustive()
    }
}

/// Frame starts a lens remembers (for the position of frames delivered late).
const LENS_STARTS: usize = 32;

impl LensControl {
    /// A lens driven by `actuator`, resting at its default position (else the low end of its
    /// range).
    pub fn new<L: LensActuator + MaybeSend + 'static>(
        actuator: L,
        description: &LensDescription,
    ) -> Self {
        Self::boxed(Box::new(actuator), description)
    }

    /// [`Self::new`] with a boxed actuator.
    pub fn boxed(actuator: Box<dyn LensDrive>, description: &LensDescription) -> Self {
        let range = description.range();
        let rest = description.default_position.unwrap_or(range[0]);
        Self {
            actuator,
            schedule: LensSchedule::new(description.motion(), description.delay, rest),
            range,
            starts: VecDeque::with_capacity(LENS_STARTS + 1),
            powered: false,
        }
    }

    /// Driver position range.
    pub fn range(&self) -> [i32; 2] {
        self.range
    }

    /// Frames from a write to the frame it is for.
    pub fn delay(&self) -> u32 {
        self.schedule.delay()
    }

    fn write(&mut self, position: i32, at: Duration) -> Result<()> {
        if !self.powered {
            self.actuator
                .power(true)
                .map_err(|e| Error::InvalidConfig(format!("lens power: {e}")))?;
            self.powered = true;
        }
        let p = position.clamp(self.range[0], self.range[1]);
        self.actuator
            .move_to(p)
            .map_err(|e| Error::InvalidConfig(format!("lens move: {e}")))?;
        self.schedule.written(at, p);
        Ok(())
    }

    /// Moves for frame `frame` (written at once when due, else at its frame start);
    /// `current` is the latest frame start (`None` before streaming: at once), `now` the
    /// platform's monotonic time.
    pub fn request_at(
        &mut self,
        frame: u64,
        position: i32,
        current: Option<u64>,
        now: Duration,
    ) -> Result<()> {
        match self.schedule.request(frame, position, current) {
            None => self.write(position, now),
            Some(_) if current.is_none() => self.write(position, now),
            Some(_) => Ok(()),
        }
    }

    /// Frame `seq` started at `at`: writes what is due.
    pub fn frame_start(&mut self, seq: u64, at: Duration) -> Result<()> {
        self.starts.push_back((seq, at));
        while self.starts.len() > LENS_STARTS {
            self.starts.pop_front();
        }
        match self.schedule.due(seq) {
            Some(p) => self.write(p, at),
            None => Ok(()),
        }
    }

    /// Where the lens was for frame `seq` given its exposure and readout time.
    pub fn frame(&self, seq: u64, exposure: Duration, readout: Duration) -> Option<LensFrame> {
        let (_, at) = self.starts.iter().rev().find(|(s, _)| *s == seq)?;
        // Rows end their exposure from the frame start to the end of the readout.
        Some(
            self.schedule
                .frame(at.saturating_sub(exposure), *at + readout),
        )
    }

    /// Puts the actuator in standby.
    pub fn power_down(&mut self) {
        if self.powered {
            let _ = self.actuator.power(false);
            self.powered = false;
        }
    }

    /// Forgets frame starts (a new stream numbers frames from 0 again).
    pub fn restart(&mut self) {
        self.starts.clear();
    }
}

/// Phase detection data decoded from the embedded data: which layout, and the last frames'.
#[derive(Debug, Default)]
pub struct PdafFrames {
    /// The sensor's layout (`imx708`).
    pub format: Option<String>,
    frames: VecDeque<(u64, Arc<[imx708_pdaf::Cell]>)>,
}

impl PdafFrames {
    /// Decodes frame `seq`'s embedded data (mode `width`, `bits` per pixel).
    pub fn decode(&mut self, seq: u64, data: &[u8], width: u32, bits: u32) {
        if self.format.as_deref() != Some("imx708") {
            return;
        }
        // As libcamera: the PDAF line starts two mode lines into the buffer.
        let offset = 2 * (width as usize * bits as usize / 8);
        if let Some(cells) = data.get(offset..).and_then(|l| imx708_pdaf::parse(l, bits)) {
            self.frames.push_back((seq, cells.into()));
            while self.frames.len() > 8 {
                self.frames.pop_front();
            }
        }
    }

    /// Frame `seq`'s cells (16×12, row-major).
    pub fn get(&self, seq: u64) -> Option<Arc<[imx708_pdaf::Cell]>> {
        self.frames
            .iter()
            .rev()
            .find(|(s, _)| *s == seq)
            .map(|(_, c)| Arc::clone(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use styx_hal::ErrorKind;

    /// Records moves.
    #[derive(Default, Clone)]
    struct Moves(crate::sync::Shared<Vec<i32>>);

    impl LensActuator for Moves {
        type Error = ErrorKind;
        fn power(&mut self, _on: bool) -> core::result::Result<(), ErrorKind> {
            Ok(())
        }
        fn move_to(&mut self, position: i32) -> core::result::Result<(), ErrorKind> {
            crate::sync::lock(&self.0).push(position);
            Ok(())
        }
    }

    #[test]
    fn moves_are_written_at_their_frame_start_and_frames_report_them() {
        let lens = Moves::default();
        let moves = lens.0.clone();
        let mut d = LensDescription::generic([0, 1023]);
        d.settle_us = 10_000;
        let mut c = LensControl::new(lens, &d);
        let ms = Duration::from_millis;
        for seq in 0..3 {
            c.frame_start(seq, ms(33 * seq)).unwrap();
        }
        // For frame 6: written at the start of frame 4.
        c.request_at(6, 500, Some(2), ms(70)).unwrap();
        c.frame_start(3, ms(99)).unwrap();
        assert!(crate::sync::lock(&moves).is_empty());
        c.frame_start(4, ms(132)).unwrap();
        assert_eq!(*crate::sync::lock(&moves), [500]);
        for seq in 5..7 {
            c.frame_start(seq, ms(33 * seq)).unwrap();
        }
        // Frame 3 saw the rest position; frame 4's rows were read out while the lens moved
        // (written at its start); frame 5's exposure began 1 ms after the move; frame 6 saw
        // the new position.
        let f = c.frame(3, ms(10), ms(5)).unwrap();
        assert_eq!((f.position, f.settled), (0.0, true));
        assert!(!c.frame(4, ms(10), ms(5)).unwrap().settled);
        let f = c.frame(5, ms(32), ms(5)).unwrap();
        assert!(!f.settled);
        let f = c.frame(6, ms(10), ms(5)).unwrap();
        assert_eq!((f.position, f.settled, f.target), (500.0, true, 500));
        // Late requests are written at once; positions are clamped to the range.
        c.request_at(5, 2000, Some(6), ms(200)).unwrap();
        assert_eq!(*crate::sync::lock(&moves), [500, 1023]);
    }

    #[test]
    fn pdaf_frames_decode_at_the_third_line() {
        let mut p = PdafFrames {
            format: Some("imx708".into()),
            ..Default::default()
        };
        let width = 1536;
        let mut data = vec![0u8; 3 * width * 10 / 8];
        let at = 2 * width * 10 / 8 + 2 * 5;
        data[at] = 0x10;
        p.decode(7, &data, width as u32, 10);
        let cells = p.get(7).unwrap();
        assert_eq!(cells.len(), 192);
        assert_eq!(cells[0].conf, 0x80);
        assert!(p.get(6).is_none());
    }
}
