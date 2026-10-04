//! The sensor frame path allocates nothing once the sensor streams: frame starts with their
//! scheduled writes (group hold, the I²C encoding), 3A-style requests every frame, immediate
//! requests, embedded data decoded and reported, the values that produced each frame; over a
//! register sensor (blocking and async drivers, through `I2cRegisters` on an embedded-hal bus)
//! and a kernel-driven one (V4L2 control batches). Counted by a global allocator that counts
//! on this test's thread only.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use embedded_hal::delay::DelayNs;
use embedded_hal::i2c::{ErrorType, I2c, Operation};
use styx_sensor::styx_hal::{Blocking, ErrorKind};
use styx_sensor::{
    AsyncSensorDriver, BusResult, ControlRange, ControlRequest, I2cRegisters, KernelControl,
    KernelSensorData, MbusCode, Rect, RegisterBus, SensorDescription, SensorDriver, SensorPins,
    Size, SubdevFormat, SubdevReport,
};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: the caller's contract is passed on.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: the caller's contract is passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
    ALLOCATIONS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCATIONS.with(Cell::get)
}

/// An OV9782 on an I²C bus that allocates nothing: 64 KiB of registers, 16-bit addresses.
struct Sensor {
    registers: Box<[u8; 65536]>,
    pointer: u16,
    transfers: u64,
}

impl Sensor {
    fn new() -> Self {
        let mut registers = Box::new([0u8; 65536]);
        registers[0x300a] = 0x97;
        registers[0x300b] = 0x82;
        Self {
            registers,
            pointer: 0,
            transfers: 0,
        }
    }
}

impl ErrorType for Sensor {
    type Error = Infallible;
}

impl I2c for Sensor {
    fn transaction(&mut self, _: u8, ops: &mut [Operation<'_>]) -> Result<(), Infallible> {
        self.transfers += 1;
        for op in ops {
            match op {
                Operation::Write(b) => {
                    self.pointer = u16::from_be_bytes([b[0], b[1]]);
                    for v in &b[2..] {
                        self.registers[usize::from(self.pointer)] = *v;
                        self.pointer = self.pointer.wrapping_add(1);
                    }
                }
                Operation::Read(buf) => {
                    for v in buf.iter_mut() {
                        *v = self.registers[usize::from(self.pointer)];
                        self.pointer = self.pointer.wrapping_add(1);
                    }
                }
            }
        }
        Ok(())
    }
}

impl embedded_hal_async::i2c::I2c for Sensor {
    async fn transaction(&mut self, a: u8, ops: &mut [Operation<'_>]) -> Result<(), Infallible> {
        I2c::transaction(self, a, ops)
    }
}

/// Pins with every role and no waiting.
struct Pins;

impl DelayNs for Pins {
    fn delay_ns(&mut self, _: u32) {}
}

impl SensorPins for Pins {
    type Error = ErrorKind;
    fn set_gpio(&mut self, _: &str, _: bool) -> Result<(), ErrorKind> {
        Ok(())
    }
    fn set_clock(&mut self, _: &str, _: Option<u32>) -> Result<(), ErrorKind> {
        Ok(())
    }
    fn set_supply(&mut self, _: &str, _: bool) -> Result<(), ErrorKind> {
        Ok(())
    }
}

/// The V4L2 controls of a kernel-driven sensor, counted.
#[derive(Default)]
struct Controls {
    calls: u64,
}

impl RegisterBus for Controls {
    fn read(&mut self, _: u16, _: u8) -> BusResult<u32> {
        Ok(0)
    }
    fn write(&mut self, _: u16, _: u8, _: u32) -> BusResult<()> {
        Ok(())
    }
    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        assert!(!controls.is_empty());
        self.calls += 1;
        Ok(())
    }
}

fn ov9782() -> Arc<SensorDescription> {
    let toml = include_str!("../sensors/ov9782.toml");
    Arc::new(SensorDescription::from_toml_str(toml, "ov9782.toml").unwrap())
}

/// The OV9782's embedded line (RAW10-packed, values at the layout's offsets: gain at 5,
/// exposure at 6-7, frame length at 19-20).
fn embedded_line(exposure: u16, gain: u8, frame_length: u16) -> Vec<u8> {
    let mut words = [0u8; 24];
    words[5] = gain;
    words[6..8].copy_from_slice(&exposure.to_be_bytes());
    words[19..21].copy_from_slice(&frame_length.to_be_bytes());
    words
        .chunks(4)
        .flat_map(|w| {
            [
                w[0] >> 2,
                w[1] >> 2,
                w[2] >> 2,
                w[3] >> 2,
                (w[0] & 3) | (w[1] & 3) << 2 | (w[2] & 3) << 4 | (w[3] & 3) << 6,
            ]
        })
        .collect()
}

fn request(seq: u64) -> ControlRequest {
    ControlRequest {
        exposure: Some(Duration::from_micros(4000 + 37 * (seq % 50))),
        gain: Some(1.0 + (seq % 7) as f64 * 0.25),
        frame_duration: seq
            .is_multiple_of(40)
            .then(|| Duration::from_micros(33_333 + seq % 3)),
    }
}

const FRAMES: u64 = 300;

#[test]
fn the_blocking_driver_streams_without_allocating() {
    let bus = I2cRegisters::new(Sensor::new(), 0x60, 16)
        .unwrap()
        .with_bursts(true);
    let mut d = SensorDriver::new(ov9782(), bus, Pins);
    d.power_up().unwrap();
    d.verify_chip_id().unwrap();
    d.init().unwrap();
    d.set_mode("1280x800", "raw10").unwrap();
    let line = embedded_line(0x0282, 0x10, 0x0386);
    let n = allocations(|| {
        d.start_streaming().unwrap();
        for seq in 0..FRAMES {
            d.frame_start(seq).unwrap();
            let landed = d.request(seq + 3, &request(seq)).unwrap();
            assert!(!landed.is_empty());
            if seq.is_multiple_of(5) {
                d.request_now(seq + 2, &request(seq + 1)).unwrap();
            }
            let codes = d.description().decode_embedded(&line);
            assert!(!codes.is_empty());
            d.report(seq, &codes).unwrap();
            assert!(d.applied(seq).is_some());
        }
        d.stop_streaming().unwrap();
    });
    assert_eq!(n, 0, "allocations in {FRAMES} frames");
    assert!(d.bus().inner().transfers > FRAMES);
    assert_eq!(d.scheduler().unwrap().dropped_requests(), 0);
}

/// Polls `f` until ready (the bus never waits, so this spins at most once per await).
fn run<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

#[test]
fn the_async_driver_streams_without_allocating() {
    let bus = I2cRegisters::new(Sensor::new(), 0x60, 16).unwrap();
    let mut d = AsyncSensorDriver::new(ov9782(), bus, Blocking(Pins));
    run(async {
        d.power_up().await.unwrap();
        d.init().await.unwrap();
        d.set_mode("1280x800", "raw10").await.unwrap();
    });
    let n = allocations(|| {
        run(async {
            d.start_streaming().await.unwrap();
            for seq in 0..FRAMES {
                d.frame_start(seq).await.unwrap();
                d.request(seq + 3, &request(seq)).unwrap();
                d.request_now(seq + 2, &request(seq + 1)).await.unwrap();
                assert!(d.applied(seq).is_some());
            }
            d.stop_streaming().await.unwrap();
        })
    });
    assert_eq!(n, 0, "allocations in {FRAMES} frames");
}

fn range(min: i64, max: i64, default: i64, value: Option<i64>) -> ControlRange {
    ControlRange {
        min,
        max,
        step: 1,
        default,
        value,
    }
}

#[test]
fn a_kernel_driven_sensor_streams_without_allocating() {
    let sizes = vec![Size::new(1280, 800)];
    let report = SubdevReport {
        name: "ov9782 10-0060".into(),
        formats: vec![SubdevFormat {
            code: MbusCode(0x3007),
            sizes,
        }],
        native_size: Some(Size::new(1296, 816)),
        crop_bounds: Some(Rect {
            left: 8,
            top: 8,
            width: 1280,
            height: 800,
        }),
        current_size: Some(Size::new(1280, 800)),
        controls: BTreeMap::from([
            (KernelControl::Exposure, range(1, 3638, 642, None)),
            (KernelControl::AnalogueGain, range(16, 255, 16, None)),
            (KernelControl::Vblank, range(110, 51540, 1022, Some(2850))),
            (KernelControl::Hblank, range(176, 31487, 176, None)),
            (
                KernelControl::PixelRate,
                range(160_000_000, 160_000_000, 160_000_000, None),
            ),
        ]),
        link_frequencies: vec![400_000_000],
        ..Default::default()
    };
    let data: Option<&KernelSensorData> = None;
    let desc = Arc::new(SensorDescription::from_subdev_with(&report, data).unwrap());
    let mode = desc.modes[0].name.clone();
    let format = desc.formats.keys().next().unwrap().clone();
    let mut d = SensorDriver::new(desc, Controls::default(), Pins);
    d.power_up().unwrap();
    d.set_mode(&mode, &format).unwrap();
    let n = allocations(|| {
        d.start_streaming().unwrap();
        for seq in 0..FRAMES {
            d.frame_start(seq).unwrap();
            d.request(seq + 3, &request(seq)).unwrap();
            assert!(d.applied(seq).is_some());
        }
        d.stop_streaming().unwrap();
    });
    assert_eq!(n, 0, "allocations in {FRAMES} frames");
    assert!(d.bus().calls > FRAMES / 2);
}
