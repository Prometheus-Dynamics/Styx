//! The focus lens of a camera module: found next to the sensor, moved frame-exactly, and the
//! position each frame saw reported with it.
//!
//! * A kernel lens driver (`dw9807-vcm`, `ak7375`, `dw9714`): the `MEDIA_ENT_F_LENS` entity
//!   linked to the sensor by an ancillary link; moves are `V4L2_CID_FOCUS_ABSOLUTE` on its
//!   subdevice ([`find_kernel_lens`]). The sensor's data file can add a `[lens]` section
//!   (settle time, dioptre map).
//! * A VCM Styx drives over I²C: the sensor description's `[lens]` with `i2c = { address,
//!   chip }` (`styx_sensor::lens`), driven by Lemnos's VCM driver (`lemnos-drivers-vcm`) on the
//!   sensor's bus.
//!
//! Moves follow the control schedule's frame starts (the runtime's [`LensControl`]): a position
//! for frame `F` is written at the start of `F - delay`, and every frame reports the position
//! predicted for its exposure from the moves and the lens's settle time
//! (`FrameControls::lens`). Phase detection data from the embedded data (IMX708) is decoded per
//! frame ([`ControlHandle::pdaf`](crate::ControlHandle::pdaf)). The actuators here implement
//! [`styx_hal::LensActuator`](LensActuator) with `std::io::Error`.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use lemnos_drivers_vcm::{Vcm, VcmError};
use lemnos_linux::hal::{I2cBus, IoError, StdDelay};
use styx_kernel::media::{EntityFunction, LinkType, Topology};
use styx_kernel::subdev::Subdev;
use styx_kernel::v4l2::{ControlValue, Controls, cid};
use styx_runtime::LensDrive;
use styx_sensor::{LensDescription, RegisterBus, SensorPins, VcmI2c};

pub use styx_runtime::styx_hal::LensActuator;
pub use styx_runtime::{LensControl, PdafFrames};

use crate::control::{SensorControl, lock};
use crate::error::{NativeError, Result};

/// Where a camera's lens is driven from.
#[derive(Clone, Debug, PartialEq)]
pub enum LensKind {
    /// A kernel lens driver's subdevice.
    Kernel {
        /// The lens subdevice node.
        subdev: PathBuf,
        /// Its media entity name (`dw9807 10-000c`).
        entity: String,
    },
    /// A VCM on I²C.
    I2c {
        /// Bus number.
        bus: u32,
        /// 7-bit address.
        address: u16,
    },
}

/// A camera's focus lens.
#[derive(Clone, Debug, PartialEq)]
pub struct LensInfo {
    /// How it is driven.
    pub kind: LensKind,
    /// What is known of it (range, settle time, map).
    pub description: LensDescription,
}

impl LensInfo {
    /// Lowest and highest driver position.
    pub fn range(&self) -> [i32; 2] {
        self.description.range()
    }
}

/// The lens linked to sensor entity `sensor` by an ancillary link, with its control range
/// (read from its subdevice) and `data` when it applies to the lens's driver.
pub fn find_kernel_lens(
    topology: &Topology,
    sensor: u32,
    data: Option<&LensDescription>,
) -> Option<LensInfo> {
    let lens = topology
        .links
        .iter()
        .filter(|l| l.flags.link_type() == LinkType::Ancillary)
        .filter_map(|l| {
            let other = if l.source_id == sensor {
                l.sink_id
            } else if l.sink_id == sensor {
                l.source_id
            } else {
                return None;
            };
            topology
                .entity(other)
                .filter(|e| e.function == EntityFunction::LENS)
        })
        .next()?;
    let subdev = topology.devnode_path(lens.id)?;
    let range = Subdev::open_read_only(&subdev)
        .ok()
        .and_then(|sd| sd.query_control(cid::FOCUS_ABSOLUTE).ok())
        .map(|c| [c.minimum as i32, c.maximum as i32]);
    let mut description = data
        .filter(|d| d.matches(&lens.name))
        .cloned()
        .unwrap_or_else(|| LensDescription::generic(range.unwrap_or([0, 1023])));
    if let Some(r) = range {
        description.range = Some(r);
    }
    description.i2c = None;
    Some(LensInfo {
        kind: LensKind::Kernel {
            subdev,
            entity: lens.name.clone(),
        },
        description,
    })
}

/// The lens of a sensor Styx drives itself: its description's `[lens]` with `i2c`, on the
/// sensor's bus unless it names another.
pub fn i2c_lens(
    description: Option<&LensDescription>,
    sensor_bus: Option<u32>,
) -> Option<LensInfo> {
    let d = description?;
    let i2c = d.i2c.as_ref()?;
    Some(LensInfo {
        kind: LensKind::I2c {
            bus: i2c.bus.or(sensor_bus)?,
            address: i2c.address,
        },
        description: d.clone(),
    })
}

/// A kernel lens driver: `FOCUS_ABSOLUTE` on its subdevice (the driver powers it with its
/// runtime PM while the subdevice is open).
#[derive(Debug)]
pub struct KernelLens(Subdev);

impl LensActuator for KernelLens {
    type Error = io::Error;

    fn power(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        Ok(self
            .0
            .set_control(cid::FOCUS_ABSOLUTE, ControlValue::Integer(position))?)
    }
}

/// A VCM on I²C: Lemnos's driver (`lemnos_drivers_vcm::Vcm`) in the description's chip
/// format, on its own i2c-dev handle (the address claimed, never forced).
#[derive(Debug)]
pub struct I2cVcm {
    bus: I2cBus,
    address: u8,
    vcm: VcmI2c,
}

fn vcm_error(e: VcmError<IoError>) -> io::Error {
    match e {
        VcmError::I2c(e) => e.into_io(),
        VcmError::Format(e) => io::Error::new(io::ErrorKind::InvalidInput, e),
    }
}

impl I2cVcm {
    /// The VCM `vcm` describes at 7-bit `address` on `bus`.
    pub fn open(bus: u32, address: u16, vcm: VcmI2c) -> io::Result<Self> {
        let address = u8::try_from(address)
            .ok()
            .filter(|a| *a <= 0x7f)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "7-bit address"))?;
        let mut i2c = I2cBus::open(bus)?;
        i2c.claim(u16::from(address))?;
        Ok(Self {
            bus: i2c,
            address,
            vcm,
        })
    }

    /// Runs `f` on Lemnos's driver for this chip (built per call over the borrowed bus: the
    /// lens schedule, not the driver, keeps the state).
    fn with_driver<R>(
        &mut self,
        f: impl FnOnce(&mut Vcm<'_, &mut I2cBus>) -> std::result::Result<R, VcmError<IoError>>,
    ) -> io::Result<R> {
        let Self { bus, address, vcm } = self;
        vcm.with_format(|format| {
            let mut driver = Vcm::new(bus, *address, *format)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            f(&mut driver).map_err(vcm_error)
        })
    }
}

impl LensActuator for I2cVcm {
    type Error = io::Error;

    fn power(&mut self, on: bool) -> io::Result<()> {
        self.with_driver(|d| {
            if on {
                d.power_up(&mut StdDelay)
            } else {
                d.power_down()
            }
        })
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        self.with_driver(|d| d.move_to(position).map(|_| ()))
    }
}

/// Records moves, for tests.
#[derive(Debug, Default, Clone)]
pub struct MockLens {
    /// Every move, in order.
    pub moves: Arc<std::sync::Mutex<Vec<i32>>>,
}

impl LensActuator for MockLens {
    type Error = io::Error;

    fn power(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        lock(&self.moves).push(position);
        Ok(())
    }
}

/// Opens the actuator of a lens.
pub fn open_actuator(info: &LensInfo) -> Result<Box<dyn LensDrive>> {
    match &info.kind {
        LensKind::Kernel { subdev, .. } => {
            Ok(Box::new(KernelLens(Subdev::open(subdev).map_err(|e| {
                NativeError::InvalidConfig(format!("lens: {e}"))
            })?)))
        }
        LensKind::I2c { bus, address } => {
            let i2c = info
                .description
                .i2c
                .as_ref()
                .ok_or_else(|| NativeError::InvalidConfig("lens without i2c".into()))?;
            let vcm = I2cVcm::open(*bus, *address, i2c.clone()).map_err(|e| {
                NativeError::InvalidConfig(format!("lens I2C {bus}-{address:04x}: {e}"))
            })?;
            Ok(Box::new(vcm))
        }
    }
}

/// Opens the lens of `info` (if any) for a camera's control, and the phase detection layout
/// its data names. A lens that cannot be opened leaves the camera without one.
pub(crate) fn attach<B: RegisterBus, P: SensorPins>(
    info: &crate::CameraInfo,
    control: &std::sync::Mutex<SensorControl<B, P>>,
) {
    let mut c = lock(control);
    if let Some(l) = &info.lens
        && let Ok(actuator) = open_actuator(l)
    {
        c.set_lens(Some(LensControl::boxed(actuator, &l.description)));
    }
    let pdaf = info
        .kernel
        .as_ref()
        .and_then(|k| k.data.as_ref())
        .and_then(|d| d.pdaf.clone());
    c.set_pdaf_format(pdaf);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_mock_lens_drives_the_runtime_lens_control() {
        let lens = MockLens::default();
        let moves = Arc::clone(&lens.moves);
        let mut c = LensControl::new(lens, &LensDescription::generic([0, 1023]));
        c.request_at(0, 2000, None, Duration::ZERO).unwrap();
        assert_eq!(*lock(&moves), [1023]);
    }
}
