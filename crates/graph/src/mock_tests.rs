use std::time::{Duration, Instant};

use super::*;
use crate::rt::{block_on, next, timeout};

const LONG: Duration = Duration::from_secs(5);

fn output(device: &dyn Device<Frame = MockFrame>, sink: &str) -> crate::GraphPath {
    let graph = device.graph();
    let sensor = graph.find("mock-sensor").unwrap().id;
    let sink = graph.find(sink).unwrap().id;
    graph
        .paths_from(sensor)
        .into_iter()
        .find(|p| p.sink() == sink)
        .unwrap()
}

fn recv(frames: &mut crate::FrameStream<MockFrame>) -> Option<Result<MockFrame, ProviderError>> {
    block_on(timeout(LONG, next(frames))).expect("frame in time")
}

#[test]
fn discovers_a_valid_graph() {
    let provider = MockProvider::new();
    assert_eq!(provider.name(), "mock");
    let devices = provider.discover().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].key.to_string(), "mock:mock0");
    let graph = &devices[0].graph;
    graph.validate().unwrap();
    let sensor = graph.sensors().next().unwrap().id;
    assert_eq!(graph.output_paths(sensor).len(), 2);
    assert_eq!(graph.paths_from(sensor).len(), 3);
    assert!(MockProvider::empty().discover().unwrap().is_empty());
}

#[test]
fn streams_two_outputs_at_the_configured_rate() {
    let provider = MockProvider::default();
    let key = provider.discover().unwrap()[0].key.clone();
    let mut device = provider.open(&key).unwrap();
    assert_eq!(device.info().name, "mock0");
    let main = StreamConfig::new(output(&*device, "main"), FourCc::NV12, Size::new(64, 32))
        .interval(Fraction::from_fps(200));
    let preview = StreamConfig::new(output(&*device, "preview"), FourCc::GREY, Size::new(32, 16))
        .interval(Fraction::from_fps(200));
    device.configure(&[main, preview]).unwrap();
    let mut streams = device.start().unwrap();
    assert_eq!(streams.len(), 2);
    assert!(format!("{:?}", streams[0]).contains("OutputStream"));

    let started = Instant::now();
    for expected in 0..20u64 {
        for stream in &mut streams {
            let frame = recv(&mut stream.frames).unwrap().unwrap();
            assert_eq!(frame.sequence, expected);
            assert_eq!(frame.output, stream.output);
            assert_eq!(frame.timestamp, Duration::from_millis(5) * expected as u32);
            let bytes = frame
                .format
                .frame_bytes(frame.size.width, frame.size.height);
            assert_eq!(Some(frame.data.len()), bytes);
            assert_eq!(frame.data[0], expected as u8);
        }
    }
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(90), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");

    device.stop().unwrap();
    for stream in &mut streams {
        assert!(recv(&mut stream.frames).is_none());
    }
}

#[test]
fn rejects_bad_configurations() {
    let provider = MockProvider::new();
    let mut device = provider.open(&DeviceKey("mock:mock0".into())).unwrap();
    let path = output(&*device, "preview");
    let too_big = StreamConfig::new(path.clone(), FourCc::NV12, Size::new(1920, 1080));
    assert!(matches!(
        device.configure(&[too_big]),
        Err(ProviderError::InvalidConfig(_))
    ));
    let wrong_format = StreamConfig::new(path.clone(), FourCc::RGB3, Size::new(640, 360));
    assert!(device.configure(&[wrong_format]).is_err());
    let too_fast = StreamConfig::new(path.clone(), FourCc::NV12, Size::new(640, 360))
        .interval(Fraction::from_fps(1000));
    let err = device.configure(&[too_fast]).unwrap_err();
    assert!(err.to_string().contains("mock-sensor"), "{err}");
    let ok = StreamConfig::new(path, FourCc::NV12, Size::new(640, 360));
    assert!(device.configure(&[ok.clone(), ok.clone()]).is_err());
    assert!(device.configure(&[]).is_err());
    assert!(matches!(
        device.start(),
        Err(ProviderError::InvalidConfig(_))
    ));
    device.configure(std::slice::from_ref(&ok)).unwrap();
    let _streams = device.start().unwrap();
    assert!(matches!(device.start(), Err(ProviderError::Busy(_))));
    assert!(matches!(
        device.configure(&[ok]),
        Err(ProviderError::Busy(_))
    ));
}

#[test]
fn open_is_exclusive_until_dropped() {
    let provider = MockProvider::new();
    let key = DeviceKey("mock:mock0".into());
    let device = provider.open(&key).unwrap();
    assert!(matches!(provider.open(&key), Err(ProviderError::Busy(_))));
    drop(device);
    assert!(provider.open(&key).is_ok());
    assert!(matches!(
        provider.open(&DeviceKey("mock:nope".into())),
        Err(ProviderError::NotFound(_))
    ));
}

#[test]
fn hotplug_reports_and_unplug_ends_streams() {
    let provider = MockProvider::empty();
    let mut events = provider.hotplug();
    let key = provider.plug("cam");
    match block_on(timeout(LONG, next(&mut events))).unwrap() {
        Some(HotplugEvent::Added(info)) => assert_eq!(info.key, key),
        other => panic!("{other:?}"),
    }
    let mut device = provider.open(&key).unwrap();
    let path = output(&*device, "main");
    device
        .configure(&[StreamConfig::new(path, FourCc::GREY, Size::new(16, 16))
            .interval(Fraction::from_fps(100))])
        .unwrap();
    let mut streams = device.start().unwrap();
    assert!(recv(&mut streams[0].frames).unwrap().is_ok());

    assert!(provider.unplug(&key));
    assert!(!provider.unplug(&key));
    match block_on(timeout(LONG, next(&mut events))).unwrap() {
        Some(HotplugEvent::Removed(removed)) => assert_eq!(removed, key),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        recv(&mut streams[0].frames),
        Some(Err(ProviderError::Disconnected))
    ));
    assert!(recv(&mut streams[0].frames).is_none());
    assert!(matches!(device.start(), Err(ProviderError::Disconnected)));
    assert!(provider.discover().unwrap().is_empty());
}

#[tokio::test]
async fn frames_stream_under_tokio() {
    let provider = MockProvider::new();
    let mut device = provider.open(&DeviceKey("mock:mock0".into())).unwrap();
    let path = output(&*device, "main");
    device
        .configure(&[StreamConfig::new(path, FourCc::RGB3, Size::new(16, 16))
            .interval(Fraction::from_fps(200))])
        .unwrap();
    let mut streams = device.start().unwrap();
    for expected in 0..5 {
        let frame = timeout(LONG, next(&mut streams[0].frames))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.sequence, expected);
        assert_eq!(frame.data.len(), 16 * 16 * 3);
    }
}
