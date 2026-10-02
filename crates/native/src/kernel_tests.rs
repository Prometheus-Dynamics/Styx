//! A kernel driver's sensor through the session, over the fake capture queue (whose receiver
//! starts the "driver" without requests) and a mock bus that records the V4L2 controls set:
//! start values before `STREAMON`, scheduled values at frame starts, the predicted values
//! reported with each frame, and a clean stop and shutdown.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use styx_sensor::{
    ControlRange, ControlRequest, DriverState, KernelControl, MbusCode, MockBus, NoPins,
    SensorDescription, SensorDriver, Size, SubdevFormat, SubdevReport,
};

use crate::control::{ControlHandle, SensorControl, lock};
use crate::device::{BridgeDevice, CaptureDevice};
use crate::fake::{FakeQueue, pattern_of};
use crate::fake_bridge::FakeBridge;
use crate::fault_tests::{BUFFER_LEN, FORMAT, WAIT, template};
use crate::sensor_bus::KernelBridge;
use crate::session::{BufferSource, Session, SessionOptions};
use crate::stream::SensorSide;

fn report() -> SubdevReport {
    let r = |min, max, default| ControlRange {
        min,
        max,
        step: 1,
        default,
        value: Some(default),
    };
    SubdevReport {
        name: "ov9782 10-0060".into(),
        formats: vec![SubdevFormat {
            code: MbusCode(0x3007),
            sizes: vec![Size::new(1280, 800)],
        }],
        current_size: Some(Size::new(1280, 800)),
        controls: BTreeMap::from([
            (KernelControl::Exposure, r(1, 1797, 642)),
            (KernelControl::AnalogueGain, r(16, 255, 16)),
            (KernelControl::Vblank, r(110, 51540, 1022)),
            (KernelControl::Hblank, r(176, 31487, 176)),
            (
                KernelControl::PixelRate,
                r(160_000_000, 160_000_000, 160_000_000),
            ),
        ]),
        ..Default::default()
    }
}

type Control = SensorControl<MockBus, NoPins>;

#[test]
fn a_kernel_driven_sensor_streams_with_scheduled_controls() {
    let bridge = FakeBridge::new(template());
    bridge.set_kernel_driver();
    let queue = FakeQueue::new(Arc::clone(&bridge), BUFFER_LEN);
    let desc = Arc::new(SensorDescription::from_subdev(&report()).unwrap());
    let mut c: Control = SensorControl::new(SensorDriver::new(desc, MockBus::new(), NoPins));
    c.bring_up("1280x800", "raw10").unwrap();
    let control = Arc::new(Mutex::new(c));
    let sensor: Arc<dyn SensorSide> = control.clone();
    let kb: Arc<dyn BridgeDevice> = Arc::new(KernelBridge::new().unwrap());
    let options = SessionOptions {
        buffers: 4,
        source: BufferSource::Memory(Default::default()),
        max_error_frames: 30,
    };
    let mut session = Session::new(kb, sensor, options);
    let video: Arc<dyn CaptureDevice> = queue.open();
    session.attach_video(video, true).unwrap();
    let handle = ControlHandle::new(Arc::clone(&control), None);
    // Frame 0's values, before the stream: set as the session starts it.
    let start = ControlRequest {
        gain: Some(2.0),
        ..Default::default()
    };
    handle.request_at(0, &start).unwrap();
    let mut stream = session.start(FORMAT).unwrap();
    {
        let c = lock(&control);
        assert_eq!(c.driver().state(), DriverState::Streaming);
        assert_eq!(
            c.driver().bus().control(KernelControl::AnalogueGain),
            Some(32)
        );
    }
    let f = {
        queue.tick().unwrap();
        stream.next_blocking(WAIT).unwrap().unwrap()
    };
    assert_eq!((f.sequence, pattern_of(f.data())), (0, Some(0)));
    assert!((f.controls.unwrap().analog_gain - 2.0).abs() < 1e-9);
    drop(f);
    // An exposure for frame 5 (delay 2) is set at the start of frame 3.
    let lands = handle
        .request_at(
            5,
            &ControlRequest {
                exposure: Some(Duration::from_millis(4)),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(lands[0].frame, 5);
    let lines = (0.004_f64 * 160e6 / 1456.0).round() as i64;
    for i in 1..8u32 {
        queue.tick().unwrap();
        let f = stream.next_blocking(WAIT).unwrap().unwrap();
        assert_eq!(f.sequence, i);
        let set = lock(&control)
            .driver()
            .bus()
            .control(KernelControl::Exposure);
        if i >= 3 {
            assert_eq!(set, Some(lines), "set by the start of frame {i}");
        } else {
            assert_ne!(set, Some(lines), "not before frame 3 ({i})");
        }
        let e = f.controls.unwrap().exposure.as_secs_f64();
        assert_eq!((e - 0.004).abs() < 1e-4, i >= 5, "frame {i}: {e}");
    }
    session.stop().unwrap();
    assert_ne!(lock(&control).driver().state(), DriverState::Streaming);
    assert_eq!(queue.num_buffers(), 0);
    session.shutdown().unwrap();
    assert_eq!(lock(&control).driver().state(), DriverState::Off);
}
