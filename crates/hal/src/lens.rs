//! Focus lens actuators that are not a plain register device: a kernel lens driver
//! (`FOCUS_ABSOLUTE`), a module's own firmware. A VCM chip on the sensor's I²C bus is driven
//! by Lemnos (`lemnos-drivers-vcm`); Styx keeps only the frame-exact schedule of its moves.

use crate::error::{ErrorKind, HalError};
use crate::power::Blocking;

/// Moves a lens.
pub trait LensActuator {
    /// The error.
    type Error: HalError;
    /// Powers the actuator up or down.
    fn power(&mut self, on: bool) -> Result<(), Self::Error>;
    /// Moves to `position` (driver units).
    fn move_to(&mut self, position: i32) -> Result<(), Self::Error>;
}

impl<L: LensActuator + ?Sized> LensActuator for &mut L {
    type Error = L::Error;
    fn power(&mut self, on: bool) -> Result<(), Self::Error> {
        (**self).power(on)
    }
    fn move_to(&mut self, position: i32) -> Result<(), Self::Error> {
        (**self).move_to(position)
    }
}

/// [`LensActuator`] over an async bus. Any [`LensActuator`] is one through [`Blocking`].
#[allow(async_fn_in_trait)]
pub trait AsyncLensActuator {
    /// The error.
    type Error: HalError;
    /// See [`LensActuator::power`].
    async fn power(&mut self, on: bool) -> Result<(), Self::Error>;
    /// See [`LensActuator::move_to`].
    async fn move_to(&mut self, position: i32) -> Result<(), Self::Error>;
}

impl<L: LensActuator> AsyncLensActuator for Blocking<L> {
    type Error = L::Error;
    async fn power(&mut self, on: bool) -> Result<(), Self::Error> {
        self.0.power(on)
    }
    async fn move_to(&mut self, position: i32) -> Result<(), Self::Error> {
        self.0.move_to(position)
    }
}

/// No lens: every call is [`ErrorKind::Unsupported`], which the runtime treats as "no lens".
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLens;

impl LensActuator for NoLens {
    type Error = ErrorKind;
    fn power(&mut self, _on: bool) -> Result<(), ErrorKind> {
        Err(ErrorKind::Unsupported)
    }
    fn move_to(&mut self, _position: i32) -> Result<(), ErrorKind> {
        Err(ErrorKind::Unsupported)
    }
}
