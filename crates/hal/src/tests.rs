use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use embedded_hal::i2c::I2c as _;

use crate::mock::{I2cMessage, MockDelay, MockI2c, MockPin, MockReceiver, block_on};
use crate::*;

struct Xclk(Vec<Option<u32>>);

impl ClockEnable for Xclk {
    fn enable(&mut self, hz: u32) -> Result<u32, ErrorKind> {
        self.0.push(Some(hz));
        Ok(hz)
    }
    fn disable(&mut self) {
        self.0.push(None);
    }
}

#[test]
fn board_pins_switch_roles_with_polarity() {
    let (reset, pwdn, avdd) = (MockPin::default(), MockPin::default(), MockPin::default());
    let delay = MockDelay::default();
    let mut pins = BoardPins::with_clock(
        [
            Line::gpio("reset", reset.clone()).active_low(),
            Line::gpio("powerdown", pwdn.clone()),
            Line::supply("avdd", avdd.clone()),
        ],
        "xclk",
        Xclk(Vec::new()),
        delay.clone(),
    );
    SensorPins::set_supply(&mut pins, "avdd", true).unwrap();
    SensorPins::set_clock(&mut pins, "xclk", Some(24_000_000)).unwrap();
    SensorPins::set_gpio(&mut pins, "reset", true).unwrap();
    wait(&mut pins, Duration::from_millis(5));
    SensorPins::set_gpio(&mut pins, "reset", false).unwrap();
    SensorPins::set_gpio(&mut pins, "powerdown", false).unwrap();
    assert_eq!(
        SensorPins::set_gpio(&mut pins, "avdd", true),
        Err(ErrorKind::NotFound)
    );
    assert_eq!(
        SensorPins::set_clock(&mut pins, "mclk", None),
        Err(ErrorKind::NotFound)
    );
    SensorPins::set_clock(&mut pins, "xclk", None).unwrap();
    assert_eq!(reset.history(), [false, true]);
    assert_eq!(pwdn.history(), [false]);
    assert_eq!(avdd.history(), [true]);
    assert_eq!(delay.history(), [Duration::from_millis(5)]);
    let (_, clock, _) = pins.into_parts();
    assert_eq!(clock.unwrap().0, [Some(24_000_000), None]);
}

#[test]
fn board_pins_and_blocking_adapters_work_async() {
    let pin = MockPin::default();
    let delay = MockDelay::default();
    let mut pins = BoardPins::new([Line::gpio("reset", pin.clone())], delay.clone());
    block_on(async {
        AsyncSensorPins::set_gpio(&mut pins, "reset", true)
            .await
            .unwrap();
        wait_async(&mut pins, Duration::from_micros(500)).await;
    });
    assert_eq!(pin.history(), [true]);
    assert_eq!(delay.history(), [Duration::from_micros(500)]);

    // A blocking implementation through the async trait completes on the first poll.
    let mut blocking = Blocking(BoardPins::new([Line::supply("avdd", pin.clone())], delay));
    let fut = core::pin::pin!(async {
        blocking.set_supply("avdd", false).await.unwrap();
        wait_async(&mut blocking, Duration::from_secs(5)).await;
    });
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    assert!(fut.poll(&mut cx).is_ready());
    assert_eq!(pin.history(), [true, false]);
}

#[test]
fn mock_i2c_models_a_register_file() {
    let mut i2c = MockI2c::new(0x36, 16).with_register(0x300a, 2, 0x9782);
    let mut buf = [0u8; 2];
    i2c.write_read(0x36, &[0x30, 0x0a], &mut buf).unwrap();
    assert_eq!(buf, [0x97, 0x82]);
    i2c.write(0x36, &[0x38, 0x0e, 0x03, 0x52]).unwrap();
    assert_eq!(i2c.value(0x380e, 2), 0x0352);
    assert!(i2c.write(0x10, &[0, 0, 0]).is_err());
    let async_i2c = i2c.clone().with_pending_polls(3);
    block_on(async {
        let mut a = async_i2c;
        embedded_hal_async::i2c::I2c::write(&mut a, 0x36, &[0x01, 0x00, 0x01])
            .await
            .unwrap();
    });
    assert_eq!(i2c.value(0x0100, 1), 1);
    assert_eq!(
        i2c.transactions().last().unwrap(),
        &[I2cMessage::Write(vec![0x01, 0x00, 0x01])]
    );
    i2c.set_dead(true);
    assert!(i2c.write(0x36, &[0x01, 0x00, 0x00]).is_err());
}

#[derive(Clone, Default)]
struct Starts(Arc<AtomicU32>);

impl SensorStart for Starts {
    fn start(&self) -> Result<(), ErrorKind> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) -> Result<(), ErrorKind> {
        self.0.fetch_add(100, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn mock_receiver_delivers_starts_and_buffers_on_separate_channels() {
    let rx = MockReceiver::new(4, 64 * 2 * 8);
    let cfg = ReceiverConfig {
        bus: Bus::Csi2 {
            lanes: 2,
            link_frequency: 0,
            continuous_clock: true,
            virtual_channel: 0,
        },
        bus_code: 0x300f,
        fourcc: *b"pBAA",
        width: 64,
        height: 8,
        stride: None,
        buffers: 3,
        memory: BufferSource::Own,
        embedded: None,
        frame_starts: true,
    };
    let c = rx.configure(&cfg).unwrap();
    assert_eq!((c.stride, c.buffer_len, c.buffers), (128, 1024, 3));
    for i in 0..3 {
        rx.queue(i).unwrap();
    }
    assert!(rx.queue(0).is_err(), "queued twice");
    let starts = Starts::default();
    rx.start(starts.clone()).unwrap();
    assert_eq!(starts.0.load(Ordering::SeqCst), 1);

    assert_eq!(rx.frame(1_000), Some(0));
    // The frame start is on the sync channel, the buffer on the done channel.
    let done = block_on(core::future::poll_fn(|cx| rx.poll_done(cx))).unwrap();
    assert_eq!((done.index, done.sequence), (0, 0));
    assert!(matches!(
        rx.try_sync().unwrap(),
        Some(SyncEvent::FrameStart { sequence: 0, .. })
    ));
    rx.frame(2_000);
    rx.frame(3_000);
    assert_eq!(rx.frame(4_000), None, "no buffer queued: overrun");
    let mut glitches = 0;
    while let Some(e) = rx.try_sync().unwrap() {
        glitches += usize::from(e == SyncEvent::Glitch(ErrorKind::Overrun));
    }
    assert_eq!(glitches, 1);
    rx.push_done(Err(ErrorKind::Disconnected));
    assert_eq!(rx.try_done().unwrap().unwrap().index, 1);
    assert_eq!(rx.try_done().unwrap().unwrap().index, 2);
    assert_eq!(rx.try_done(), Err(ErrorKind::Disconnected));
    rx.stop(starts.clone()).unwrap();
    assert_eq!(starts.0.load(Ordering::SeqCst), 101);
    assert!(!rx.streaming());
    rx.release().unwrap();
}

#[test]
fn no_lens_is_unsupported() {
    assert_eq!(NoLens.move_to(10), Err(ErrorKind::Unsupported));
    let mut lens = Blocking(NoLens);
    let r = block_on(AsyncLensActuator::power(&mut lens, true));
    assert_eq!(r, Err(ErrorKind::Unsupported));
}

#[test]
fn instants_and_kinds() {
    let a = Instant::from_nanos(1_000);
    let b = a.checked_add(Duration::from_micros(2)).unwrap();
    assert_eq!(b.saturating_duration_since(a), Duration::from_micros(2));
    assert_eq!(a.saturating_duration_since(b), Duration::ZERO);
    assert_eq!(
        ErrorKind::from_i2c(embedded_hal::i2c::ErrorKind::NoAcknowledge(
            embedded_hal::i2c::NoAcknowledgeSource::Data
        )),
        ErrorKind::Nack
    );
    let io = std::io::Error::from_raw_os_error(16);
    assert_eq!(HalError::kind(&io), ErrorKind::Busy);
    assert_eq!(HalError::code(&io), Some(16));
}
