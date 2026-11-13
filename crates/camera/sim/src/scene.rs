//! What the simulated sensor sees, and how a frame's raw Bayer samples are made from it.
//!
//! A [`Scene`] gives the light on each photosite at the reference exposure and gain. The camera
//! scales that light by the exposure and analogue gain in force on the frame, clamps it to the
//! 10-bit range and packs it as `pRAA` (RGGB10P): four samples in five bytes, the high eight bits
//! of each in four bytes and the two low bits of all four in the fifth.

use std::sync::Arc;

/// The exposure, in microseconds, at which a scene's light maps to half of full scale.
pub const REFERENCE_EXPOSURE_US: u32 = 10_000;
/// The analogue gain, in thousandths, at which the light is not scaled.
pub const REFERENCE_GAIN_MILLI: u32 = 1_000;
/// Largest 10-bit sample.
pub const FULL_SCALE: u16 = 1023;

/// Light on the photosites at the reference exposure and gain.
#[derive(Clone, Debug)]
pub enum Scene {
    /// A horizontal ramp with bands every eight rows: a pattern with known values everywhere.
    Pattern,
    /// Replayed raw frames, each of `width * height` 10-bit RGGB samples, looped by frame
    /// identity: frame `n` shows `frames[n % frames.len()]`.
    Replay(Arc<Vec<Vec<u16>>>),
}

impl Scene {
    /// The light (0 through 1) at the reference exposure and gain on photosite `(x, y)` of frame
    /// `identity`.
    pub fn light(&self, identity: u64, x: u32, y: u32, width: u32) -> f32 {
        match self {
            Scene::Pattern => {
                let ramp = (x as f32 + 0.5) / width as f32;
                let band = if (y / 8).is_multiple_of(2) { 1.0 } else { 0.6 };
                ramp * band
            }
            Scene::Replay(frames) => {
                let frame = &frames[(identity % frames.len() as u64) as usize];
                f32::from(frame[(y * width + x) as usize]) / f32::from(FULL_SCALE)
            }
        }
    }
}

/// The colour each photosite of an RGGB mosaic takes: red on even rows and columns, blue on odd
/// rows and columns, green on the other two. Each colour has its own sensitivity.
fn sensitivity(x: u32, y: u32) -> f32 {
    match (y % 2, x % 2) {
        (0, 0) => 0.9,
        (1, 1) => 0.8,
        _ => 1.0,
    }
}

/// The exposure and gain a frame is rendered at, from the controls in force. `Auto` (the sensor
/// has no automatic exposure here) and an unset control hold the reference value.
pub fn resolve(exposure: Option<u32>, gain: Option<u32>) -> (u32, u32) {
    (
        exposure.unwrap_or(REFERENCE_EXPOSURE_US),
        gain.unwrap_or(REFERENCE_GAIN_MILLI),
    )
}

/// The 10-bit sample on photosite `(x, y)` of a frame at `exposure_us` and `gain_milli`.
pub fn sample(light: f32, x: u32, y: u32, exposure_us: u32, gain_milli: u32) -> u16 {
    let scale = exposure_us as f32 / REFERENCE_EXPOSURE_US as f32
        * (gain_milli as f32 / REFERENCE_GAIN_MILLI as f32);
    let value = light * sensitivity(x, y) * 0.5 * scale * f32::from(FULL_SCALE);
    value.round().clamp(0.0, f32::from(FULL_SCALE)) as u16
}

/// Packs four 10-bit samples into the five bytes of `pRAA`.
pub fn pack_quad(samples: [u16; 4], out: &mut [u8]) {
    for (byte, sample) in out.iter_mut().zip(samples) {
        *byte = (sample >> 2) as u8;
    }
    out[4] = samples
        .iter()
        .enumerate()
        .fold(0u8, |acc, (i, s)| acc | (((s & 3) as u8) << (2 * i)));
}

/// The bytes of one row of `width` samples in `pRAA`.
pub fn row_bytes(width: u32) -> usize {
    width as usize * 10 / 8
}
