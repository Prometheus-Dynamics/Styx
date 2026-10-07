//! Sensor power sequencing: GPIO lines, the input clock and supplies by role, plus a delay.
//!
//! A sensor description's power sequence names roles (`reset`, `powerdown`, `xclk`, `avdd`,
//! ...); [`SensorPins`] maps them onto the generic hardware: embedded-hal `OutputPin`s and
//! `DelayNs`, and Lemnos's [`ClockOutput`] for the input clock ([`BoardPins`]). Only the role
//! mapping is camera-specific; pins, clocks and regulators are Lemnos's (`lemnos_hal`).

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
pub use lemnos_hal::ClockOutput;
use lemnos_hal::HalError as _;

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

/// A blocking implementation used through the async traits: Lemnos's adapter
/// (`lemnos_hal::asynch::Blocking`), whose every future is ready when first polled. Lemnos
/// gives it embedded-hal-async's `I2c`, `SpiDevice` and `DelayNs` and
/// `lemnos_hal::asynch::RegisterBus` over their blocking twins; Styx adds
/// [`AsyncSensorPins`] over [`SensorPins`] and `AsyncLensActuator` over `LensActuator`.
///
/// This is the sound direction. The other one, blocking code over an async-only
/// implementation, needs an executor to wait on its futures; Styx ships none (Linux code has
/// its own threads and reactor, firmware its own executor, e.g. Embassy). To drive an
/// async-only bus from blocking code, run the async API (e.g. `styx_sensor::AsyncSensorDriver`)
/// in the platform's executor (`embassy_futures::block_on`, `pollster`, a task) instead.
pub use lemnos_hal::asynch::Blocking;

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

/// A board without a switchable camera clock (a fixed oscillator on the module): the type of
/// [`BoardPins::new`]'s absent clock. Every clock role is [`ErrorKind::NotFound`], so the
/// description's clock steps must be `optional`. (A clock that always runs at one rate and
/// should accept its role is Lemnos's `FixedClock`.)
#[derive(Clone, Copy, Debug, Default)]
pub struct NoClock;

impl ClockOutput for NoClock {
    type Error = lemnos_hal::ErrorKind;
    fn enable(&mut self) -> Result<(), Self::Error> {
        Err(lemnos_hal::ErrorKind::NotFound)
    }
    fn disable(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn rate_hz(&mut self) -> Result<u32, Self::Error> {
        Err(lemnos_hal::ErrorKind::NotFound)
    }
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

/// [`SensorPins`] from generic parts: `N` embedded-hal output pins by role (GPIOs and supply
/// enables, as Lemnos's `GpioRegulator`), an optional input clock (a Lemnos [`ClockOutput`]:
/// `FixedClock`, a PWM or PLL output), and a delay (blocking `DelayNs` for [`SensorPins`], async for
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

impl<G: OutputPin, C: ClockOutput, D, const N: usize> BoardPins<G, C, D, N> {
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
            Some((r, clock)) if *r == role => clock.set(rate_hz).map_err(|e| e.kind().into()),
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

impl<G: OutputPin, C: ClockOutput, D: DelayNs, const N: usize> SensorPins
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

impl<G: OutputPin, C: ClockOutput, D: embedded_hal_async::delay::DelayNs, const N: usize>
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
