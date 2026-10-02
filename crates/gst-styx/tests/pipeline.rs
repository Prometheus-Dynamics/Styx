//! `styxsrc` in real pipelines, with Styx's virtual camera (no hardware needed).

use std::time::Duration;

use gst::prelude::*;

fn init() {
    gst::init().unwrap();
    gststyx::register_static().unwrap();
}

fn pipeline(desc: &str) -> (gst::Pipeline, gst_app::AppSink) {
    let pipeline = gst::parse::launch(desc)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    (pipeline, sink)
}

fn pull(sink: &gst_app::AppSink) -> gst::Sample {
    sink.try_pull_sample(gst::ClockTime::from_seconds(5))
        .expect("a frame within 5 s")
}

#[test]
fn planner_converts_to_negotiated_format() {
    init();
    let (pipeline, sink) = pipeline(
        "styxsrc name=src camera=virtual:320x240:YUYV ! video/x-raw,format=GRAY8 ! appsink name=sink sync=false",
    );
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut last = None;
    for _ in 0..10 {
        let sample = pull(&sink);
        let caps = sample.caps().unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("format").unwrap(), "GRAY8");
        assert_eq!(s.get::<i32>("width").unwrap(), 320);
        let buffer = sample.buffer().unwrap();
        assert!(buffer.size() >= 320 * 240);
        let pts = buffer.pts().unwrap();
        assert!(last.is_none_or(|l| pts > l), "timestamps increase");
        assert_eq!(
            buffer.duration(),
            Some(gst::ClockTime::from_nseconds(33_333_333))
        );
        last = Some(pts);
    }
    let plan: Option<String> = pipeline.by_name("src").unwrap().property("plan");
    assert!(plan.unwrap().contains("virtual"));
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn renegotiates_when_downstream_caps_change() {
    init();
    let (pipeline, sink) = pipeline(
        "styxsrc camera=virtual:320x240:YUYV ! capsfilter name=filter caps=video/x-raw,format=RGB \
         ! appsink name=sink sync=false",
    );
    pipeline.set_state(gst::State::Playing).unwrap();
    let rgb = pull(&sink);
    assert_eq!(
        rgb.caps()
            .unwrap()
            .structure(0)
            .unwrap()
            .get::<&str>("format")
            .unwrap(),
        "RGB"
    );
    let filter = pipeline.by_name("filter").unwrap();
    filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "GRAY8")
            .build(),
    );
    let mut switched = false;
    for _ in 0..60 {
        let sample = pull(&sink);
        let format = sample
            .caps()
            .unwrap()
            .structure(0)
            .unwrap()
            .get::<String>("format");
        if format.as_deref() == Ok("GRAY8") {
            switched = true;
            break;
        }
    }
    assert!(switched, "frames switched to GRAY8 after renegotiation");
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn missing_camera_fails_with_an_error_message() {
    init();
    let (pipeline, _sink) = pipeline("styxsrc camera=no-such-camera-anywhere ! appsink name=sink");
    assert!(pipeline.set_state(gst::State::Playing).is_err());
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            Duration::from_secs(5).try_into().ok(),
            &[gst::MessageType::Error],
        )
        .expect("an error message");
    let gst::MessageView::Error(err) = msg.view() else {
        unreachable!()
    };
    assert!(err.error().to_string().contains("no camera matches"));
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn caps_query_in_ready_lists_the_camera() {
    init();
    let src = gst::ElementFactory::make("styxsrc")
        .property("camera", "virtual:160x120:YUYV")
        .build()
        .unwrap();
    src.set_state(gst::State::Ready).unwrap();
    let caps = src.static_pad("src").unwrap().query_caps(None);
    let first = caps.structure(0).unwrap();
    assert_eq!(
        first.get::<&str>("format").unwrap(),
        "YUY2",
        "camera format first"
    );
    assert_eq!(first.get::<i32>("width").unwrap(), 160);
    assert!(caps.iter().any(|s| s.get::<&str>("format") == Ok("RGB")));
    assert!(caps.iter().any(|s| s.get::<&str>("format") == Ok("GRAY8")));
    src.set_state(gst::State::Null).unwrap();
}
