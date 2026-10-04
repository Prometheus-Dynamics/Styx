//! Error paths and recovery of a running camera, over the fake bridge and capture queue
//! (`fake.rs`) and a register bus that can stop answering: restarts with frames still held,
//! acknowledgement timeouts and late acknowledgements, a sensor that does not answer on I²C,
//! receiver errors, missing frame-start events, devices that go away, and clients that drop
//! things at awkward moments. Every case checks the state left behind: sensor in standby and
//! powered down, bridge idle (and off after shutdown), queue without buffers.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use styx_kernel::FourCc;
use styx_kernel::bus::{StreamAction, StreamRequest};
use styx_sensor::{DriverState, MockBus, RegWrite, RegisterBus, SensorDescription, SensorDriver};

use crate::buffers::NativeFrame;
use crate::control::{ExpectedStart, SensorControl, lock};
use crate::device::{BridgeDevice, CaptureDevice};
use crate::error::NativeError;
use crate::fake::{BridgeState, FakeBridge, FakeQueue, pattern_of};
use crate::regbus::BridgePins;
use crate::session::{BufferSource, Session, SessionOptions, StreamFormat};
use crate::stream::{FrameStream, SensorSide};

pub(crate) const BUFFER_LEN: usize = 4096;
pub(crate) const WAIT: Duration = Duration::from_secs(3);

/// A register bus over [`MockBus`] that can stop answering (every access fails with
/// `EREMOTEIO`, as an I²C NACK) or answer slowly.
#[derive(Clone, Default)]
pub(crate) struct FaultBus {
    pub(crate) bus: Arc<Mutex<MockBus>>,
    pub(crate) dead: Arc<AtomicBool>,
    pub(crate) delay: Arc<Mutex<Duration>>,
}

impl FaultBus {
    fn check(&self) -> io::Result<()> {
        let d = *lock(&self.delay);
        if !d.is_zero() {
            std::thread::sleep(d);
        }
        if self.dead.load(Ordering::Acquire) {
            Err(io::Error::from_raw_os_error(libc::EREMOTEIO))
        } else {
            Ok(())
        }
    }

    /// Whether the sensor streams (its `stream_on` register is set).
    pub(crate) fn streaming(&self) -> bool {
        lock(&self.bus).value(0x0100, 1) == 1
    }
}

impl RegisterBus for FaultBus {
    fn read(&mut self, address: u16, bytes: u8) -> io::Result<u32> {
        self.check()?;
        lock(&self.bus).read(address, bytes)
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> io::Result<()> {
        self.check()?;
        lock(&self.bus).write(address, bytes, value)
    }

    fn write_sequence(&mut self, writes: &[RegWrite]) -> io::Result<()> {
        writes
            .iter()
            .try_for_each(|w| self.write(w.address, w.bytes, w.value))
    }
}

pub(crate) type Control = SensorControl<FaultBus, BridgePins<Arc<FakeBridge>>>;

/// A configured camera over fakes.
pub(crate) struct Rig {
    pub(crate) bridge: Arc<FakeBridge>,
    pub(crate) queue: Arc<FakeQueue>,
    pub(crate) bus: FaultBus,
    pub(crate) control: Arc<Mutex<Control>>,
    pub(crate) session: Session,
}

pub(crate) fn template() -> StreamRequest {
    StreamRequest {
        action: StreamAction::Start,
        sequence: 0,
        timeout: Duration::ZERO,
        link_freq: 400_000_000,
        pixel_rate: 160_000_000,
        code: 0x3007,
        width: 1280,
        height: 800,
        hblank: 176,
        vblank: 1022,
        data_lanes: 2,
        continuous_clock: true,
    }
}

pub(crate) const FORMAT: StreamFormat = StreamFormat {
    fourcc: FourCc::new(b"pBAA"),
    width: 1280,
    height: 800,
    stride: 1600,
    size_image: BUFFER_LEN as u32,
};

fn options(source: BufferSource, max_error_frames: u32) -> SessionOptions {
    SessionOptions {
        buffers: 4,
        source,
        max_error_frames,
    }
}

impl Rig {
    pub(crate) fn new() -> Self {
        Self::with(BufferSource::Memory(Default::default()), 30)
    }

    pub(crate) fn with(source: BufferSource, max_error_frames: u32) -> Self {
        let bridge = FakeBridge::new(template());
        let queue = FakeQueue::new(Arc::clone(&bridge), BUFFER_LEN);
        let desc = Arc::new(
            SensorDescription::from_file(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../sensor/sensors/ov9782.toml"
            ))
            .unwrap(),
        );
        let bus = FaultBus::default();
        *lock(&bus.bus) = MockBus::new().with_register(0x300a, 2, 0x9782);
        let pins = BridgePins::new(
            Arc::clone(&bridge),
            &["avdd", "dovdd", "dvdd"],
            &[("xvclk", 24_000_000)],
            Duration::ZERO,
        );
        let mut c = SensorControl::new(SensorDriver::new(desc, bus.clone(), pins));
        c.bring_up("1280x800", "raw10").unwrap();
        c.expect_start(ExpectedStart {
            code: 0x3007,
            width: 1280,
            height: 800,
            link_freq: 400_000_000,
        });
        let control = Arc::new(Mutex::new(c));
        let sensor: Arc<dyn SensorSide> = control.clone();
        let b: Arc<dyn BridgeDevice> = bridge.clone();
        let mut session = Session::new(b, sensor, options(source, max_error_frames));
        let video: Arc<dyn CaptureDevice> = queue.open();
        session.attach_video(video, true).unwrap();
        Self {
            bridge,
            queue,
            bus,
            control,
            session,
        }
    }

    pub(crate) fn start(&mut self) -> Result<FrameStream, NativeError> {
        self.session.start(FORMAT)
    }

    pub(crate) fn driver_state(&self) -> DriverState {
        lock(&self.control).driver().state()
    }

    /// Stopped cleanly: sensor in standby, bridge idle, queue stopped without buffers.
    pub(crate) fn assert_stopped(&self) {
        assert!(!self.bus.streaming(), "sensor still streams");
        assert_ne!(self.driver_state(), DriverState::Streaming);
        assert_eq!(self.bridge.state(), BridgeState::Idle);
        assert!(!self.queue.is_streaming());
        assert_eq!(self.queue.num_buffers(), 0, "buffers not released");
        assert!(!self.session.is_streaming());
    }

    /// Shut down cleanly: stopped, sensor off, bridge off.
    pub(crate) fn assert_shut_down(&self) {
        self.assert_stopped();
        assert_eq!(self.driver_state(), DriverState::Off);
        assert!(!self.bridge.powered(), "bridge still powered");
    }
}

/// Produces one frame and takes it.
pub(crate) fn frame(rig: &Rig, stream: &mut FrameStream) -> NativeFrame {
    rig.queue.tick().expect("streaming");
    stream.next_blocking(WAIT).unwrap().expect("a frame")
}

pub(crate) fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + WAIT;
    while !f() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn frames_flow_and_stop_releases_the_buffers() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    assert!(rig.bus.streaming());
    assert_eq!(rig.bridge.state(), BridgeState::Streaming);
    for i in 0..10 {
        let f = frame(&rig, &mut stream);
        assert_eq!(f.sequence, i);
        assert_eq!(pattern_of(f.data()), Some(i));
    }
    assert_eq!(stream.stats().frames, 10);
    rig.session.stop().unwrap();
    assert!(stream.next_blocking(WAIT).unwrap().is_none());
    rig.assert_stopped();
    assert_eq!(rig.queue.counters().bad_qbufs, 0);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

/// The bug this fixes: restarting while frames are held failed with `EBUSY` on `REQBUFS`
/// (another descriptor, the old one kept open by the frames, owned the queue), and a held
/// frame dropped later freed or queued the new stream's buffers.
#[test]
fn restart_on_the_same_camera_with_frames_held() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let held: Vec<NativeFrame> = (0..3).map(|_| frame(&rig, &mut stream)).collect();
    rig.session.stop().unwrap();
    rig.assert_stopped();
    // Held frames stay readable, with their own data, after the queue let go of them.
    for (i, f) in held.iter().enumerate() {
        assert_eq!(pattern_of(f.data()), Some(i as u32));
        assert!(f.dmabuf().is_some());
    }
    let mut stream = rig.start().unwrap();
    assert_eq!(rig.queue.num_buffers(), 4);
    let first = frame(&rig, &mut stream);
    assert_eq!(first.sequence, 0);
    drop(first);
    // Old frames dropped now must not touch the new stream's queue.
    drop(held);
    assert_eq!(rig.queue.counters().bad_qbufs, 0);
    assert_eq!(rig.queue.queued(), 4, "every new buffer is queued");
    for i in 1..20 {
        assert_eq!(frame(&rig, &mut stream).sequence, i);
    }
    assert_eq!(rig.queue.counters().dropped, 0);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

/// Reopening (a new descriptor on the same queue, as the supervisor does after a reconnect)
/// while the old camera's frames are still in a consumer's queue.
#[test]
fn reopen_with_frames_of_the_closed_camera_held() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let held: Vec<NativeFrame> = (0..4).map(|_| frame(&rig, &mut stream)).collect();
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
    // Bring the sensor up again and start through a new handle.
    lock(&rig.control).bring_up("1280x800", "raw10").unwrap();
    let mut session = Session::new(
        rig.bridge.clone(),
        rig.control.clone(),
        options(BufferSource::Memory(Default::default()), 0),
    );
    session.attach_video(rig.queue.open(), true).unwrap();
    let mut stream = session.start(FORMAT).expect("no EBUSY from REQBUFS");
    rig.queue.tick();
    assert_eq!(stream.next_blocking(WAIT).unwrap().unwrap().sequence, 0);
    for (i, f) in held.iter().enumerate() {
        assert_eq!(pattern_of(f.data()), Some(i as u32));
    }
    drop(held);
    assert_eq!(rig.queue.counters().reqbufs_busy, 0);
    assert_eq!(rig.queue.counters().bad_qbufs, 0);
    session.shutdown().unwrap();
    assert!(!rig.bridge.powered());
}

#[test]
fn restart_with_imported_buffers_held() {
    let mut rig = Rig::with(BufferSource::Memfd, 0);
    for round in 0..3 {
        let mut stream = rig.start().unwrap();
        let held: Vec<NativeFrame> = (0..4).map(|_| frame(&rig, &mut stream)).collect();
        // All buffers are held: the queue starves (frames dropped), nothing breaks.
        assert!(rig.queue.tick().is_some());
        assert_eq!(rig.queue.counters().dropped, round + 1);
        rig.session.stop().unwrap();
        rig.assert_stopped();
        for (i, f) in held.iter().enumerate() {
            assert_eq!(pattern_of(f.data()), Some(i as u32));
        }
    }
    assert_eq!(rig.queue.counters().bad_qbufs, 0);
}

#[test]
fn a_start_nobody_acknowledges_times_out_and_leaves_a_clean_state() {
    let mut rig = Rig::new();
    rig.bridge.set_timeout(Duration::from_millis(50));
    rig.bridge.set_deaf(true);
    let err = rig.start().unwrap_err();
    assert_eq!(err.errno(), Some(libc::ETIMEDOUT), "{err}");
    assert!(err.to_string().contains("timed out"), "{err}");
    rig.assert_stopped();
    // The fault is gone: the same session starts.
    rig.bridge.set_deaf(false);
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn a_start_acknowledged_too_late_puts_the_sensor_back_in_standby() {
    let mut rig = Rig::new();
    rig.bridge.set_timeout(Duration::from_millis(30));
    // Each register write takes 60 ms: stream-on is served after the bridge gave up.
    *lock(&rig.bus.delay) = Duration::from_millis(60);
    let err = rig.start().unwrap_err();
    assert_eq!(err.errno(), Some(libc::ETIMEDOUT), "{err}");
    *lock(&rig.bus.delay) = Duration::ZERO;
    assert_eq!(rig.bridge.stale_acks(), 1);
    rig.assert_stopped();
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn a_sensor_that_does_not_answer_fails_the_start_clearly() {
    let mut rig = Rig::new();
    rig.bus.dead.store(true, Ordering::Release);
    let err = rig.start().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("the sensor did not start"), "{msg}");
    assert!(msg.contains("0x0100"), "{msg}");
    assert_eq!(err.errno(), Some(libc::EIO));
    assert!(!rig.session.is_streaming());
    assert_eq!(rig.bridge.state(), BridgeState::Idle);
    assert_eq!(rig.queue.num_buffers(), 0);
    // Shut down without a sensor that answers: still powered down.
    let r = rig.session.shutdown();
    assert!(r.is_err(), "the failed stream-off is reported");
    assert_eq!(rig.driver_state(), DriverState::Off);
    assert!(!rig.bridge.powered());
}

/// With `report_start_errors=1` the bridge fails `STREAMON` itself; the same clean state.
#[test]
fn failed_starts_reported_to_the_receiver_leave_a_clean_state() {
    let mut rig = Rig::new();
    rig.bridge.set_report_errors(true);
    rig.bridge.set_timeout(Duration::from_millis(50));
    rig.bridge.set_deaf(true);
    let err = rig.start().unwrap_err();
    assert_eq!(err.errno(), Some(libc::ETIMEDOUT), "{err}");
    rig.assert_stopped();
    rig.bridge.set_deaf(false);
    rig.bus.dead.store(true, Ordering::Release);
    let err = rig.start().unwrap_err();
    assert!(
        err.to_string().contains("the sensor did not start"),
        "{err}"
    );
    assert_eq!(err.errno(), Some(libc::EIO));
    rig.bus.dead.store(false, Ordering::Release);
    rig.assert_stopped();
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn a_sensor_that_stops_answering_mid_stream_ends_the_stream() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let controls = crate::ControlHandle::new(rig.control.clone(), None);
    frame(&rig, &mut stream);
    rig.bus.dead.store(true, Ordering::Release);
    let mut result = None;
    for i in 0..40 {
        // A new exposure every frame: a write is due at every frame start.
        let _ = controls.set_exposure(Duration::from_micros(1000 + i * 10));
        rig.queue.tick();
        std::thread::sleep(Duration::from_millis(2));
        match stream.next_blocking(WAIT) {
            Ok(Some(_)) => {}
            other => {
                result = Some(other);
                break;
            }
        }
    }
    let err = result.expect("the stream ended").unwrap_err();
    assert!(err.to_string().contains("stopped answering"), "{err}");
    assert!(stream.next_blocking(WAIT).unwrap().is_none(), "ended");
    let _ = rig.session.shutdown();
    assert_eq!(rig.driver_state(), DriverState::Off);
    assert!(!rig.bridge.powered());
    assert_eq!(rig.bridge.state(), BridgeState::Idle);
    assert_eq!(rig.queue.num_buffers(), 0);
}

#[test]
fn corrupted_frames_in_a_row_end_the_stream() {
    let mut rig = Rig::with(BufferSource::Memory(Default::default()), 5);
    let mut stream = rig.start().unwrap();
    rig.queue.set_corrupt(true);
    // A few corrupted frames are delivered, flagged.
    for _ in 0..4 {
        assert!(frame(&rig, &mut stream).error);
    }
    rig.queue.set_corrupt(false);
    assert!(!frame(&rig, &mut stream).error);
    rig.queue.set_corrupt(true);
    let mut ended = None;
    for _ in 0..10 {
        rig.queue.tick();
        match stream.next_blocking(WAIT) {
            Ok(Some(f)) => assert!(f.error),
            other => {
                ended = Some(other);
                break;
            }
        }
    }
    let err = ended.unwrap().unwrap_err();
    assert!(err.to_string().contains("5 frames in a row"), "{err}");
    assert_eq!(stream.stats().errors, 9);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn a_receiver_error_ends_the_stream() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.queue.fail();
    let err = stream.next_blocking(WAIT).unwrap_err();
    assert_eq!(err.errno(), Some(libc::EIO), "{err}");
    assert!(stream.next_blocking(WAIT).unwrap().is_none());
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn missing_frame_start_events_fall_back_to_dequeues() {
    let mut rig = Rig::new();
    rig.queue.set_frame_sync(false);
    let mut stream = rig.start().unwrap();
    let starts = |rig: &Rig| lock(&rig.control).frame_starts();
    let before = starts(&rig);
    for _ in 0..3 {
        frame(&rig, &mut stream);
    }
    assert!(stream.frame_sync_fallback());
    for _ in 0..5 {
        frame(&rig, &mut stream);
    }
    // The schedule still advances: every dequeue counts as the next frame's start.
    let after = starts(&rig);
    assert!(after >= before + 6, "{before} -> {after}");
    // Events come back: they drive the schedule again.
    rig.queue.set_frame_sync(true);
    for n in 1..=3 {
        rig.queue.tick();
        wait_until("the frame-start event", || {
            rig.session.event_counts().is_some_and(|(s, _)| s >= n)
        });
        stream.next_blocking(WAIT).unwrap().unwrap();
    }
    assert!(!stream.frame_sync_fallback());
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn the_bridge_going_away_mid_stream_disconnects() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.bridge.unplug();
    let err = stream.next_blocking(WAIT).unwrap_err();
    assert!(matches!(err, NativeError::Disconnected), "{err}");
    assert!(err.is_disconnect());
    assert!(stream.is_disconnected());
    assert!(stream.next_blocking(WAIT).unwrap().is_none());
    // Stopping still works (the receiver stops without the bridge).
    let _ = rig.session.shutdown();
    assert!(!rig.queue.is_streaming());
    assert_eq!(rig.queue.num_buffers(), 0);
    assert_eq!(rig.driver_state(), DriverState::Off);
}

#[test]
fn the_capture_node_going_away_mid_stream_disconnects() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let held = frame(&rig, &mut stream);
    rig.queue.unplug();
    let err = stream.next_blocking(WAIT).unwrap_err();
    assert!(matches!(err, NativeError::Disconnected), "{err}");
    let stopped = rig.session.stop();
    assert!(
        matches!(stopped, Err(NativeError::Disconnected)),
        "{stopped:?}"
    );
    // The held frame is still readable after the node went away.
    assert_eq!(pattern_of(held.data()), Some(0));
    drop(held);
    rig.session.shutdown().unwrap();
    assert_eq!(rig.driver_state(), DriverState::Off);
    assert!(!rig.bridge.powered());
}

/// On V4L2 nodes frame-start events come on the capture descriptor, which the frame stream
/// registered with the reactor already (a second registration fails with `EEXIST`).
#[test]
fn events_on_the_capture_descriptor_share_its_registration() {
    let mut rig = Rig::new();
    let mut session = Session::new(
        rig.bridge.clone(),
        rig.control.clone(),
        options(BufferSource::Memory(Default::default()), 0),
    );
    session.attach_video(rig.queue.open_shared(), true).unwrap();
    // The rig's own session holds no buffers: use the shared one.
    rig.session.shutdown().unwrap();
    lock(&rig.control).bring_up("1280x800", "raw10").unwrap();
    for _ in 0..2 {
        let mut stream = session.start(FORMAT).unwrap();
        for i in 0..5 {
            rig.queue.tick();
            assert_eq!(stream.next_blocking(WAIT).unwrap().unwrap().sequence, i);
        }
        session.stop().unwrap();
    }
    session.shutdown().unwrap();
    assert!(!rig.bridge.powered());
}

#[test]
fn starting_without_a_bridge_is_a_disconnect() {
    let mut rig = Rig::new();
    rig.bridge.unplug();
    let err = rig.start().unwrap_err();
    assert!(err.is_disconnect(), "{err}");
    assert!(!rig.session.is_streaming());
    assert_eq!(rig.queue.num_buffers(), 0);
}

#[test]
fn dropping_things_mid_stream_leaves_a_clean_state() {
    let mut rig = Rig::new();
    let mut stream = rig.start().unwrap();
    let held = frame(&rig, &mut stream);
    rig.queue.tick();
    rig.queue.tick();
    // The consumer goes away with frames pending in the queue.
    drop(stream);
    // The camera is dropped without stop or close.
    let Rig {
        bridge,
        queue,
        bus,
        control,
        session,
    } = rig;
    drop(session);
    assert!(!bus.streaming());
    assert_eq!(lock(&control).driver().state(), DriverState::Off);
    assert_eq!(bridge.state(), BridgeState::Idle);
    assert!(!bridge.powered());
    assert!(!queue.is_streaming());
    assert_eq!(queue.num_buffers(), 0);
    // The frame outlives everything.
    assert_eq!(pattern_of(held.data()), Some(0));
    drop(held);
    assert_eq!(queue.counters().bad_qbufs, 0);
}

/// The receiver holds the node's lock through `STREAMON`/`STREAMOFF` while the bridge waits for
/// the acknowledgement; frame-start events keep arriving meanwhile. Touching the node then
/// (a `DQEVENT`) would block the acknowledgement until the bridge times out.
#[test]
fn start_and_stop_are_acknowledged_while_events_arrive() {
    let mut rig = Rig::new();
    let producer = rig.queue.run(Duration::from_micros(300));
    for _ in 0..20 {
        let mut stream = rig.start().unwrap();
        for _ in 0..3 {
            stream.next_blocking(WAIT).unwrap().unwrap();
        }
        let t = std::time::Instant::now();
        rig.session.stop().unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "stop took {:?}",
            t.elapsed()
        );
    }
    drop(producer);
    assert_eq!(rig.bridge.timeouts(), 0);
    assert_eq!(rig.bridge.stale_acks(), 0);
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn many_restarts_keep_no_resources() {
    let mut rig = Rig::new();
    let fds = || {
        std::fs::read_dir("/proc/self/fd")
            .map(|d| d.count())
            .unwrap_or(0)
    };
    // Warm up (reactor, threads).
    let mut stream = rig.start().unwrap();
    frame(&rig, &mut stream);
    rig.session.stop().unwrap();
    drop(stream);
    let before = fds();
    for _ in 0..30 {
        let mut stream = rig.start().unwrap();
        let held = frame(&rig, &mut stream);
        rig.session.stop().unwrap();
        drop((held, stream));
    }
    // Other tests run in parallel and open descriptors too: allow some slack.
    let after = fds();
    assert!(after <= before + 16, "{before} -> {after} descriptors");
    rig.session.shutdown().unwrap();
    rig.assert_shut_down();
}

#[test]
fn capture_buffers_smaller_than_the_format_are_refused() {
    use crate::buffers::{Allocator, BufferSet};
    let queue = FakeQueue::new(FakeBridge::new(template()), BUFFER_LEN);
    let video = || -> Arc<dyn CaptureDevice> { queue.open() };
    // The driver's buffers are a page; a format that needs more is refused before any QBUF.
    let e = BufferSet::allocate_with(video(), None, 4, BUFFER_LEN + 1).err();
    assert!(matches!(e, Some(NativeError::InvalidConfig(_))), "{e:?}");
    // An imported dma-buf is checked at the size the kernel allocated, not the size asked for.
    let e = BufferSet::allocate_with(video(), Some(Allocator::MemfdOf(1000)), 4, 1024).err();
    assert!(matches!(e, Some(NativeError::InvalidConfig(_))), "{e:?}");
    let set = BufferSet::allocate_with(video(), Some(Allocator::MemfdOf(2048)), 4, 1024).unwrap();
    assert_eq!(set.len(), 1024);
    assert_eq!(queue.counters().bad_qbufs, 0);
}
