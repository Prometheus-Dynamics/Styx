//! `styx-sensor`'s bus traits over the kernel: [`RegisterBus`] over i2c-dev and [`SensorPins`]
//! over the bridge's power control.

use std::io;
use std::time::Duration;

use styx_kernel::bus::SensorBridge;
use styx_kernel::bus::i2c::{self, AddrWidth, I2cDevice, Message};
use styx_sensor::styx_hal::embedded_hal::delay::DelayNs;
use styx_sensor::{BusError, BusResult, RegWrite, RegisterBus, SensorPins};

/// The raw I²C transfers the register bus needs; [`I2cDevice`] in the spike, a recorder in
/// tests.
pub trait I2cIo {
    /// One combined transfer: write `addr`, repeated start, read `buf.len()` bytes.
    fn write_read(&mut self, addr: &[u8], buf: &mut [u8]) -> io::Result<()>;
    /// One combined transfer of several write messages (repeated starts, one stop).
    fn write_messages(&mut self, messages: &[&[u8]]) -> io::Result<()>;
}

impl I2cIo for I2cDevice {
    fn write_read(&mut self, addr: &[u8], buf: &mut [u8]) -> io::Result<()> {
        self.transfer(&mut [Message::Write(addr), Message::Read(buf)])
    }

    fn write_messages(&mut self, messages: &[&[u8]]) -> io::Result<()> {
        let mut msgs: Vec<Message<'_>> = messages.iter().map(|m| Message::Write(m)).collect();
        self.transfer(&mut msgs)
    }
}

/// A sensor's registers over I²C: big-endian register addresses of the description's width,
/// multi-byte values as consecutive registers, most significant byte first (one burst).
#[derive(Debug)]
pub struct I2cRegisterBus<T> {
    io: T,
    addr_width: AddrWidth,
}

impl<T: I2cIo> I2cRegisterBus<T> {
    /// A bus with `address_bits` (8 or 16) register addresses.
    pub fn new(io: T, address_bits: u8) -> io::Result<Self> {
        let addr_width = match address_bits {
            8 => AddrWidth::Bits8,
            16 => AddrWidth::Bits16,
            b => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{b}-bit register addresses"),
                ));
            }
        };
        Ok(Self { io, addr_width })
    }

    /// The transport.
    #[cfg(test)]
    pub fn io(&self) -> &T {
        &self.io
    }

    /// Reads `bytes` registers one at a time (separate transfers) and joins them big-endian:
    /// a cross-check for [`RegisterBus::read`]'s burst read.
    pub fn read_bytewise(&mut self, address: u16, bytes: u8) -> io::Result<u32> {
        let mut value = 0u32;
        for i in 0..bytes {
            let v = self.read(address.wrapping_add(u16::from(i)), 1)?;
            value = (value << 8) | v;
        }
        Ok(value)
    }
}

fn check_width(bytes: u8) -> io::Result<usize> {
    if (1..=4).contains(&bytes) {
        Ok(usize::from(bytes))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{bytes}-byte register access (1..=4)"),
        ))
    }
}

/// Encodes one register write (address, then the value's low `bytes` bytes, big-endian).
pub fn encode_write(address: u16, bytes: u8, value: u32, width: AddrWidth) -> io::Result<Vec<u8>> {
    let n = check_width(bytes)?;
    if n < 4 && value >> (8 * n) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("value {value:#x} does not fit {n} bytes at {address:#06x}"),
        ));
    }
    i2c::encode_burst(address, width, &value.to_be_bytes()[4 - n..])
}

impl<T: I2cIo> RegisterBus for I2cRegisterBus<T> {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let n = check_width(bytes)?;
        let mut addr = [0u8; 2];
        let a = i2c::encode_reg_addr(address, self.addr_width, &mut addr)?;
        let mut buf = [0u8; 4];
        self.io.write_read(&addr[..a], &mut buf[..n])?;
        Ok(i2c::decode_value(&buf[..n]))
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        let buf = encode_write(address, bytes, value, self.addr_width)?;
        Ok(self.io.write_messages(&[&buf])?)
    }

    /// One transfer (start, address, register, data, stop) per register write, as the kernel
    /// drivers do.
    ///
    /// Not combined transfers: on the CM5 (RP1 DesignWare I²C, `/dev/i2c-10`) with the OV9782,
    /// packing consecutive writes into one `I2C_RDWR` call with repeated starts lost or
    /// misplaced some of them (measured: `0x380a` read back 0x00 instead of 0x03, so the sensor
    /// sent 32 lines instead of 800, and `0x4509`/`0x450a` read back 0x09/0x00, as if a message
    /// had been appended to the previous one and auto-incremented). Probably the controller
    /// does not always issue the repeated start between two write messages, and the SCCB
    /// target then takes the next message's register address as data. Reads (write then read)
    /// are unaffected: the direction change always restarts.
    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        for w in writes {
            let buf = encode_write(w.address, w.bytes, w.value, self.addr_width)?;
            self.io.write_messages(&[&buf])?;
        }
        Ok(())
    }
}

/// Something that switches the sensor's supplies and clock together: the bridge's
/// `STYX_CID_POWER`, or a recorder in tests.
pub trait PowerSwitch {
    /// Switch on or off.
    fn set_power(&self, on: bool) -> io::Result<()>;
}

impl PowerSwitch for SensorBridge {
    fn set_power(&self, on: bool) -> io::Result<()> {
        SensorBridge::set_power(self, on)
    }
}

impl<P: PowerSwitch + ?Sized> PowerSwitch for std::sync::Arc<P> {
    fn set_power(&self, on: bool) -> io::Result<()> {
        (**self).set_power(on)
    }
}

/// [`SensorPins`] for a sensor whose supplies and clock the bridge owns (the CM5 camera
/// regulator and the fixed 24 MHz clock) and that has no GPIO lines.
///
/// The bridge switches all of them at once, so the first supply or clock turned on powers the
/// sensor and the last one turned off powers it down; the roles only track the description's
/// sequence. GPIO roles report `NotFound` (optional steps are skipped).
#[derive(Debug)]
pub struct BridgePins<S> {
    switch: S,
    supplies: Vec<(String, bool)>,
    clocks: Vec<(String, u32, bool)>,
    powered: bool,
    settle: Duration,
}

impl<S: PowerSwitch> BridgePins<S> {
    /// Pins for the given supply roles and `(clock role, rate in Hz)`; `settle` is waited after
    /// switching power on, before the sequence goes on.
    pub fn new(switch: S, supplies: &[&str], clocks: &[(&str, u32)], settle: Duration) -> Self {
        Self {
            switch,
            supplies: supplies.iter().map(|s| ((*s).to_owned(), false)).collect(),
            clocks: clocks
                .iter()
                .map(|(c, rate)| ((*c).to_owned(), *rate, false))
                .collect(),
            powered: false,
            settle,
        }
    }

    /// Whether this has switched the bridge on.
    #[cfg(test)]
    pub fn powered(&self) -> bool {
        self.powered
    }

    fn any_on(&self) -> bool {
        self.supplies.iter().any(|s| s.1) || self.clocks.iter().any(|c| c.2)
    }

    fn update(&mut self) -> io::Result<()> {
        let want = self.any_on();
        if want != self.powered {
            self.switch.set_power(want)?;
            self.powered = want;
            if want {
                std::thread::sleep(self.settle);
            }
        }
        Ok(())
    }
}

fn not_found(what: &str, role: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no {what} '{role}' behind the bridge"),
    )
}

impl<S> DelayNs for BridgePins<S> {
    fn delay_ns(&mut self, ns: u32) {
        std::thread::sleep(Duration::from_nanos(u64::from(ns)));
    }
}

impl<S: PowerSwitch> SensorPins for BridgePins<S> {
    type Error = BusError;

    fn set_gpio(&mut self, role: &str, _: bool) -> BusResult<()> {
        Err(not_found("gpio", role).into())
    }

    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()> {
        let clock = self
            .clocks
            .iter_mut()
            .find(|c| c.0 == role)
            .ok_or_else(|| not_found("clock", role))?;
        if let Some(rate) = rate_hz
            && clock.1 != 0
            && rate != clock.1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("clock '{role}' runs at {} Hz, not {rate} Hz", clock.1),
            )
            .into());
        }
        clock.2 = rate_hz.is_some();
        Ok(self.update()?)
    }

    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()> {
        let supply = self
            .supplies
            .iter_mut()
            .find(|s| s.0 == role)
            .ok_or_else(|| not_found("supply", role))?;
        supply.1 = on;
        Ok(self.update()?)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use styx_sensor::{BusErrorKind, MockBus, SensorDescription, SensorDriver};

    use super::*;

    /// Records transfers and answers reads from a byte map (auto-incrementing addresses), as
    /// the sensor does.
    #[derive(Default)]
    struct Recorder {
        regs: BTreeMap<u16, u8>,
        transfers: Vec<Vec<Vec<u8>>>,
        reads: Vec<(Vec<u8>, usize)>,
    }

    impl I2cIo for Recorder {
        fn write_read(&mut self, addr: &[u8], buf: &mut [u8]) -> io::Result<()> {
            self.reads.push((addr.to_vec(), buf.len()));
            let start = if addr.len() == 2 {
                u16::from_be_bytes([addr[0], addr[1]])
            } else {
                u16::from(addr[0])
            };
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.regs.get(&(start + i as u16)).unwrap_or(&0);
            }
            Ok(())
        }

        fn write_messages(&mut self, messages: &[&[u8]]) -> io::Result<()> {
            self.transfers
                .push(messages.iter().map(|m| m.to_vec()).collect());
            for m in messages {
                let start = u16::from_be_bytes([m[0], m[1]]);
                for (i, b) in m[2..].iter().enumerate() {
                    self.regs.insert(start + i as u16, *b);
                }
            }
            Ok(())
        }
    }

    fn ov9782_bus() -> I2cRegisterBus<Recorder> {
        let mut rec = Recorder::default();
        rec.regs.insert(0x300a, 0x97);
        rec.regs.insert(0x300b, 0x82);
        I2cRegisterBus::new(rec, 16).unwrap()
    }

    #[test]
    fn two_byte_chip_id_is_one_burst_with_a_16_bit_address() {
        let mut bus = ov9782_bus();
        assert_eq!(bus.read(0x300a, 2).unwrap(), 0x9782);
        assert_eq!(bus.io().reads, vec![(vec![0x30, 0x0a], 2)]);
        assert_eq!(bus.read_bytewise(0x300a, 2).unwrap(), 0x9782);
        assert_eq!(
            bus.io().reads[1..],
            [(vec![0x30, 0x0a], 1), (vec![0x30, 0x0b], 1)]
        );
    }

    #[test]
    fn encodes_writes_big_endian() {
        let w = AddrWidth::Bits16;
        assert_eq!(encode_write(0x0100, 1, 1, w).unwrap(), [0x01, 0x00, 0x01]);
        assert_eq!(
            encode_write(0x380e, 2, 0x071e, w).unwrap(),
            [0x38, 0x0e, 0x07, 0x1e]
        );
        assert_eq!(
            encode_write(0x3500, 3, 0x002820, w).unwrap(),
            [0x35, 0x00, 0x00, 0x28, 0x20]
        );
        assert_eq!(
            encode_write(0x12, 1, 0xab, AddrWidth::Bits8).unwrap(),
            [0x12, 0xab]
        );
        assert!(encode_write(0x0100, 1, 0x100, w).is_err());
        assert!(encode_write(0x0100, 0, 0, w).is_err());
        assert!(encode_write(0x0100, 5, 0, w).is_err());
        assert!(I2cRegisterBus::new(Recorder::default(), 12).is_err());
    }

    #[test]
    fn sequences_are_one_transfer_per_write() {
        let mut bus = ov9782_bus();
        let writes: Vec<RegWrite> = (0..50)
            .map(|i| RegWrite::byte(0x3000 + i, i as u8))
            .collect();
        bus.write_sequence(&writes).unwrap();
        let t = &bus.io().transfers;
        assert_eq!(t.len(), 50);
        assert!(t.iter().all(|m| m.len() == 1));
        assert_eq!(t[1][0], [0x30, 0x01, 0x01]);
    }

    /// The driver run over this bus writes exactly what it writes over the mock bus.
    #[test]
    fn driver_writes_match_the_mock_bus() {
        let desc = Arc::new(
            SensorDescription::from_file(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../crates/sensor/sensors/ov9782.toml"
            ))
            .unwrap(),
        );
        let switch = PowerLog::default();
        let pins = BridgePins::new(
            &switch,
            &["avdd", "dovdd", "dvdd"],
            &[("xvclk", 24_000_000)],
            Duration::ZERO,
        );
        let mut real = SensorDriver::new(Arc::clone(&desc), ov9782_bus(), pins);
        let mock = MockBus::new().with_register(0x300a, 2, 0x9782);
        let mut fake = SensorDriver::new(desc, mock, styx_sensor::MockPins::default());
        for d in [&mut real as &mut dyn Steps, &mut fake] {
            d.bring_up();
        }
        let flat: Vec<u8> = real
            .bus()
            .io()
            .transfers
            .iter()
            .flatten()
            .flatten()
            .copied()
            .collect();
        let mut expected = Vec::new();
        for w in fake.bus().writes() {
            expected.extend(encode_write(w.address, w.bytes, w.value, AddrWidth::Bits16).unwrap());
        }
        assert_eq!(flat, expected);
        assert_eq!(*switch.log.borrow(), [true]);
    }

    trait Steps {
        fn bring_up(&mut self);
    }

    impl<B: RegisterBus, P: SensorPins> Steps for SensorDriver<B, P> {
        fn bring_up(&mut self) {
            self.power_up().unwrap();
            assert_eq!(self.verify_chip_id().unwrap(), 0x9782);
            self.init().unwrap();
            self.set_mode("1280x800", "raw10").unwrap();
            self.start_streaming().unwrap();
            self.stop_streaming().unwrap();
        }
    }

    #[derive(Default)]
    struct PowerLog {
        log: RefCell<Vec<bool>>,
    }

    impl PowerSwitch for &PowerLog {
        fn set_power(&self, on: bool) -> io::Result<()> {
            self.log.borrow_mut().push(on);
            Ok(())
        }
    }

    #[test]
    fn pins_switch_the_bridge_once_per_edge() {
        let switch = PowerLog::default();
        let mut pins = BridgePins::new(
            &switch,
            &["avdd", "dovdd"],
            &[("xvclk", 24_000_000)],
            Duration::ZERO,
        );
        pins.set_supply("avdd", true).unwrap();
        pins.set_supply("dovdd", true).unwrap();
        pins.set_clock("xvclk", Some(24_000_000)).unwrap();
        assert!(pins.powered());
        assert!(pins.set_clock("xvclk", Some(19_200_000)).is_err());
        assert_eq!(
            pins.set_gpio("reset", true).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
        assert_eq!(
            pins.set_supply("vana", true).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
        pins.set_clock("xvclk", None).unwrap();
        pins.set_supply("dovdd", false).unwrap();
        assert!(pins.powered());
        pins.set_supply("avdd", false).unwrap();
        assert!(!pins.powered());
        assert_eq!(*switch.log.borrow(), [true, false]);
    }
}
