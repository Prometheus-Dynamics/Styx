//! The driver over embedded-hal buses: the blocking driver over a blocking `I2c` and the async
//! driver over an async `I2c` that really suspends (the mock returns `Pending` before every
//! transfer) write the same transfers in the same order, and the same registers as the driver
//! over the register-level mock bus.

use std::sync::Arc;
use std::time::Duration;

use styx_hal::mock::{I2cMessage, MockDelay, MockI2c, MockPin, block_on};
use styx_hal::{Blocking, BoardPins, Line};
use styx_sensor::{
    AsyncSensorDriver, ControlRequest, I2cRegisters, MockBus, MockPins, PinOp, SensorDescription,
    SensorDriver, SensorError,
};

const OV9782: &str = include_str!("../sensors/ov9782.toml");

fn desc() -> Arc<SensorDescription> {
    Arc::new(SensorDescription::from_toml_str(OV9782, "ov9782.toml").unwrap())
}

fn sensor() -> MockI2c {
    MockI2c::new(0x60, 16).with_register(0x300a, 2, 0x9782)
}

fn exposure() -> ControlRequest {
    ControlRequest {
        exposure: Some(Duration::from_millis(8)),
        gain: Some(2.0),
        frame_duration: None,
    }
}

/// Bring-up, start, three frames with a request, stop, power down: the blocking driver.
fn run_blocking(i2c: MockI2c, bursts: bool) -> Vec<PinOp> {
    let bus = I2cRegisters::new(i2c, 0x60, 16)
        .unwrap()
        .with_bursts(bursts);
    let mut d = SensorDriver::new(desc(), bus, MockPins::with_roles(&["avdd", "xvclk"]));
    d.power_up().unwrap();
    assert_eq!(d.verify_chip_id().unwrap(), 0x9782);
    d.init().unwrap();
    d.set_mode("1280x800", "raw10").unwrap();
    d.start_streaming().unwrap();
    d.frame_start(0).unwrap();
    d.request(3, &exposure()).unwrap();
    for seq in 1..4 {
        d.frame_start(seq).unwrap();
    }
    d.request_now(5, &exposure()).unwrap();
    d.stop_streaming().unwrap();
    d.power_down().unwrap();
    let (_, pins) = d.into_parts();
    pins.log
}

/// The same steps on the async driver.
async fn run_async(i2c: MockI2c, bursts: bool) -> Vec<PinOp> {
    let bus = I2cRegisters::new(i2c, 0x60, 16)
        .unwrap()
        .with_bursts(bursts);
    let pins = Blocking(MockPins::with_roles(&["avdd", "xvclk"]));
    let mut d = AsyncSensorDriver::new(desc(), bus, pins);
    d.power_up().await.unwrap();
    assert_eq!(d.verify_chip_id().await.unwrap(), 0x9782);
    d.init().await.unwrap();
    d.set_mode("1280x800", "raw10").await.unwrap();
    d.start_streaming().await.unwrap();
    d.frame_start(0).await.unwrap();
    d.request(3, &exposure()).unwrap();
    for seq in 1..4 {
        d.frame_start(seq).await.unwrap();
    }
    d.request_now(5, &exposure()).await.unwrap();
    d.stop_streaming().await.unwrap();
    d.power_down().await.unwrap();
    let (_, pins) = d.into_parts();
    pins.0.log
}

#[test]
fn async_and_blocking_drivers_write_the_same_transfers() {
    for bursts in [false, true] {
        let sync_i2c = sensor();
        let sync_pins = run_blocking(sync_i2c.clone(), bursts);
        let async_i2c = sensor().with_pending_polls(2);
        let async_pins = block_on(run_async(async_i2c.clone(), bursts));
        assert_eq!(sync_i2c.transactions(), async_i2c.transactions());
        assert_eq!(sync_i2c.registers(), async_i2c.registers());
        assert_eq!(sync_pins, async_pins);
        assert!(sync_i2c.transactions().len() > 20);
    }
}

#[test]
fn i2c_registers_write_what_the_mock_bus_records() {
    let i2c = sensor();
    run_blocking(i2c.clone(), false);
    let mut d = SensorDriver::new(
        desc(),
        MockBus::new().with_register(0x300a, 2, 0x9782),
        MockPins::with_roles(&["avdd", "xvclk"]),
    );
    d.power_up().unwrap();
    d.verify_chip_id().unwrap();
    d.init().unwrap();
    d.set_mode("1280x800", "raw10").unwrap();
    d.start_streaming().unwrap();
    d.frame_start(0).unwrap();
    d.request(3, &exposure()).unwrap();
    for seq in 1..4 {
        d.frame_start(seq).unwrap();
    }
    d.request_now(5, &exposure()).unwrap();
    d.stop_streaming().unwrap();
    d.power_down().unwrap();
    // Every write as its own transfer: address (2 bytes) then the value, most significant first.
    let expected: Vec<Vec<I2cMessage>> = d
        .bus()
        .log
        .iter()
        .map(|op| match op {
            styx_sensor::BusOp::Read { address, bytes, .. } => vec![
                I2cMessage::Write(address.to_be_bytes().to_vec()),
                I2cMessage::Read(usize::from(*bytes)),
            ],
            styx_sensor::BusOp::Write(w) => {
                let mut m = w.address.to_be_bytes().to_vec();
                m.extend_from_slice(&w.value.to_be_bytes()[4 - usize::from(w.bytes)..]);
                vec![I2cMessage::Write(m)]
            }
        })
        .collect();
    assert_eq!(i2c.transactions(), expected);
}

#[test]
fn a_sensor_that_stops_answering_fails_the_async_frame_start() {
    let i2c = sensor().with_pending_polls(1);
    let probe = i2c.clone();
    block_on(async move {
        let bus = I2cRegisters::new(i2c, 0x60, 16).unwrap();
        let mut d = AsyncSensorDriver::new(desc(), bus, Blocking(MockPins::default()));
        d.power_up().await.unwrap();
        d.init().await.unwrap();
        d.set_mode("1280x800", "raw10").await.unwrap();
        d.start_streaming().await.unwrap();
        d.request(2, &exposure()).unwrap();
        probe.set_dead(true);
        let mut failed = None;
        for seq in 0..4 {
            if let Err(e) = d.frame_start(seq).await {
                failed = Some(e);
                break;
            }
        }
        match failed {
            Some(SensorError::Bus { source, .. }) => {
                assert_eq!(source.kind(), styx_sensor::BusErrorKind::Nack)
            }
            other => panic!("expected a bus error, got {other:?}"),
        }
        // Pins only: works without the bus.
        d.force_power_down().await.unwrap();
    });
}

#[test]
fn board_pins_with_an_async_delay_power_the_sensor() {
    let (reset, avdd) = (MockPin::default(), MockPin::default());
    let delay = MockDelay::default();
    let pins = BoardPins::new(
        [
            Line::gpio("reset", reset.clone()),
            Line::supply("avdd", avdd.clone()),
        ],
        delay.clone(),
    );
    let bus = I2cRegisters::new(sensor().with_pending_polls(1), 0x60, 16).unwrap();
    block_on(async move {
        let mut d = AsyncSensorDriver::new(desc(), bus, pins);
        d.power_up().await.unwrap();
        assert_eq!(d.verify_chip_id().await.unwrap(), 0x9782);
        d.power_down().await.unwrap();
    });
    assert_eq!(avdd.history().first(), Some(&true));
    assert_eq!(reset.history().first(), Some(&true));
    assert!(!delay.history().is_empty());
}
