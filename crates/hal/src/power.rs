//! Sensor power sequencing: GPIO lines, the input clock and supplies by role, plus a delay.
//!
//! A sensor description's power sequence names roles (`reset`, `powerdown`, `xclk`, `avdd`,
//! ...); [`SensorPins`] switches them. Delays are embedded-hal's `DelayNs` (the pins are one),
//! so a board's pins are any embedded-hal `OutputPin`s and its delay ([`BoardPins`]).

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;

use crate::error::{ErrorKind, HalError};

/// GPIO lines, clocks and supplies of one sensor, by role name, and the waits between them.
///
/// Roles the board does not have fail with [`ErrorKind::NotFound`]: the sensor driver skips
/// optional steps then, and fails on required ones.
pub trait SensorPins: DelayNs {
    /// The implementation's error.
    type Error: HalError;
    /// Sets a GPIO line to a logical level (the implementation applies the line's polarity).
    fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error>;
    /// Enables a clock at `rate_hz`, or disables it with `None`.
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error>;
    /// Switches a supply.
    fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error>;
}

impl<P: SensorPins + ?Sized> SensorPins for &mut P {
    type Error = P::Error;
    fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error> {
        (**self).set_gpio(role, value)
    }
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error> {
        (**self).set_clock(role, rate_hz)
    }
    fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error> {
        (**self).set_supply(role, on)
    }
}

/// [`SensorPins`] whose operations may wait (a supply behind an I²C PMIC on an async bus), with
/// embedded-hal-async's `DelayNs`. Any [`SensorPins`] is one through [`Blocking`].
#[allow(async_fn_in_trait)]
pub trait AsyncSensorPins: embedded_hal_async::delay::DelayNs {
    /// The implementation's error.
    type Error: HalError;
    /// See [`SensorPins::set_gpio`].
    async fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error>;
    /// See [`SensorPins::set_clock`].
    async fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error>;
    /// See [`SensorPins::set_supply`].
    async fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error>;
}

impl<P: AsyncSensorPins + ?Sized> AsyncSensorPins for &mut P {
    type Error = P::Error;
    async fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error> {
        (**self).set_gpio(role, value).await
    }
    async fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error> {
        (**self).set_clock(role, rate_hz).await
    }
    async fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error> {
        (**self).set_supply(role, on).await
    }
}

/// A blocking implementation used through an async trait: every operation runs to completion
/// when first polled (its future is always ready).
///
/// This is the sound direction. The other one, blocking code over an async-only
/// implementation, needs an executor to wait on its futures; Styx ships none (Linux code has
/// its own threads and reactor, firmware its own executor, e.g. Embassy). To drive an
/// async-only bus from blocking code, run the async API (e.g. `styx_sensor::AsyncSensorDriver`)
/// in the platform's executor (`embassy_futures::block_on`, `pollster`, a task) instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blocking<T>(pub T);

impl<T: DelayNs> embedded_hal_async::delay::DelayNs for Blocking<T> {
    async fn delay_ns(&mut self, ns: u32) {
        self.0.delay_ns(ns)
    }
    async fn delay_us(&mut self, us: u32) {
        self.0.delay_us(us)
    }
    async fn delay_ms(&mut self, ms: u32) {
        self.0.delay_ms(ms)
    }
}

impl<P: SensorPins> AsyncSensorPins for Blocking<P> {
    type Error = P::Error;
    async fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error> {
        self.0.set_gpio(role, value)
    }
    async fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error> {
        self.0.set_clock(role, rate_hz)
    }
    async fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error> {
        self.0.set_supply(role, on)
    }
}

/// The camera's input clock where the board makes it (an STM32 MCO output, an ESP32 LEDC
/// channel): enable at a rate, disable.
pub trait ClockEnable {
    /// Starts the clock at (about) `hz`; returns the rate obtained.
    fn enable(&mut self, hz: u32) -> Result<u32, ErrorKind>;
    /// Stops the clock.
    fn disable(&mut self);
}

/// A board without a switchable camera clock (a fixed oscillator on the module): every clock
/// role is [`ErrorKind::NotFound`], so the description's clock steps must be `optional`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoClock;

impl ClockEnable for NoClock {
    fn enable(&mut self, _hz: u32) -> Result<u32, ErrorKind> {
        Err(ErrorKind::NotFound)
    }
    fn disable(&mut self) {}
}

/// What a [`Line`] of [`BoardPins`] is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// A GPIO role (`reset`, `powerdown`, `enable`).
    Gpio,
    /// A supply enable (a regulator's enable pin).
    Supply,
}

/// One output pin of [`BoardPins`] with its role.
#[derive(Debug)]
pub struct Line<G> {
    /// The role name in the sensor description.
    pub role: &'static str,
    /// GPIO or supply.
    pub kind: LineKind,
    /// The pin.
    pub pin: G,
    /// Logical high drives the pin low.
    pub active_low: bool,
}

impl<G> Line<G> {
    /// A GPIO role on `pin`, active high.
    pub fn gpio(role: &'static str, pin: G) -> Self {
        Self {
            role,
            kind: LineKind::Gpio,
            pin,
            active_low: false,
        }
    }

    /// A supply enable on `pin`, active high.
    pub fn supply(role: &'static str, pin: G) -> Self {
        Self {
            role,
            kind: LineKind::Supply,
            pin,
            active_low: false,
        }
    }

    /// Marks the line active low.
    pub fn active_low(mut self) -> Self {
        self.active_low = true;
        self
    }
}

/// [`SensorPins`] from embedded-hal parts: `N` output pins by role (GPIOs and supply enables),
/// an optional input clock, and a delay (blocking `DelayNs` for [`SensorPins`], async for
/// [`AsyncSensorPins`]). Use one pin type for all lines (most HALs have a type-erased output,
/// e.g. Embassy's `Output<'static>`).
#[derive(Debug)]
pub struct BoardPins<G, C, D, const N: usize> {
    lines: [Line<G>; N],
    clock: Option<(&'static str, C)>,
    delay: D,
}

impl<G: OutputPin, D, const N: usize> BoardPins<G, NoClock, D, N> {
    /// Pins on `lines` with `delay`, no switchable clock.
    pub fn new(lines: [Line<G>; N], delay: D) -> Self {
        Self {
            lines,
            clock: None,
            delay,
        }
    }
}

impl<G: OutputPin, C: ClockEnable, D, const N: usize> BoardPins<G, C, D, N> {
    /// Pins on `lines` with `delay` and the clock role `role` on `clock`.
    pub fn with_clock(lines: [Line<G>; N], role: &'static str, clock: C, delay: D) -> Self {
        Self {
            lines,
            clock: Some((role, clock)),
            delay,
        }
    }

    /// The parts back.
    pub fn into_parts(self) -> ([Line<G>; N], Option<C>, D) {
        (self.lines, self.clock.map(|(_, c)| c), self.delay)
    }

    fn set_line(&mut self, kind: LineKind, role: &str, on: bool) -> Result<(), ErrorKind> {
        let line = self
            .lines
            .iter_mut()
            .find(|l| l.kind == kind && l.role == role)
            .ok_or(ErrorKind::NotFound)?;
        let high = on != line.active_low;
        let r = if high {
            line.pin.set_high()
        } else {
            line.pin.set_low()
        };
        r.map_err(|e| ErrorKind::from_digital(embedded_hal::digital::Error::kind(&e)))
    }

    fn switch_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), ErrorKind> {
        match &mut self.clock {
            Some((r, clock)) if *r == role => match rate_hz {
                Some(hz) => clock.enable(hz).map(|_| ()),
                None => {
                    clock.disable();
                    Ok(())
                }
            },
            _ => Err(ErrorKind::NotFound),
        }
    }
}

impl<G, C, D: DelayNs, const N: usize> DelayNs for BoardPins<G, C, D, N> {
    fn delay_ns(&mut self, ns: u32) {
        self.delay.delay_ns(ns)
    }
    fn delay_us(&mut self, us: u32) {
        self.delay.delay_us(us)
    }
    fn delay_ms(&mut self, ms: u32) {
        self.delay.delay_ms(ms)
    }
}

impl<G, C, D: embedded_hal_async::delay::DelayNs, const N: usize> embedded_hal_async::delay::DelayNs
    for BoardPins<G, C, D, N>
{
    async fn delay_ns(&mut self, ns: u32) {
        self.delay.delay_ns(ns).await
    }
    async fn delay_us(&mut self, us: u32) {
        self.delay.delay_us(us).await
    }
    async fn delay_ms(&mut self, ms: u32) {
        self.delay.delay_ms(ms).await
    }
}

impl<G: OutputPin, C: ClockEnable, D: DelayNs, const N: usize> SensorPins
    for BoardPins<G, C, D, N>
{
    type Error = ErrorKind;
    fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), ErrorKind> {
        self.set_line(LineKind::Gpio, role, value)
    }
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), ErrorKind> {
        self.switch_clock(role, rate_hz)
    }
    fn set_supply(&mut self, role: &str, on: bool) -> Result<(), ErrorKind> {
        self.set_line(LineKind::Supply, role, on)
    }
}

impl<G: OutputPin, C: ClockEnable, D: embedded_hal_async::delay::DelayNs, const N: usize>
    AsyncSensorPins for BoardPins<G, C, D, N>
{
    type Error = ErrorKind;
    async fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), ErrorKind> {
        self.set_line(LineKind::Gpio, role, value)
    }
    async fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), ErrorKind> {
        self.switch_clock(role, rate_hz)
    }
    async fn set_supply(&mut self, role: &str, on: bool) -> Result<(), ErrorKind> {
        self.set_line(LineKind::Supply, role, on)
    }
}
