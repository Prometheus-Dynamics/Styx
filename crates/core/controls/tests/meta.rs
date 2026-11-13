//! Metadata round-trips: through a `FramePool<FrameMetadata>` lease, and through serde.

use styx_controls::{Control, ControlValue, FrameMetadata, Rect};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, packed_layouts};

fn entry(id: u64, timestamp_ns: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 8, 8, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(8, 8).unwrap());
    FrameEntry::new(id, timestamp_ns, format, layouts)
}

fn sample() -> FrameMetadata {
    let mut controls = [ControlValue::default(); Control::COUNT];
    controls[Control::Exposure.index()] = ControlValue::Manual(12_000);
    controls[Control::Gain.index()] = ControlValue::Manual(2_500);
    controls[Control::WhiteBalance.index()] = ControlValue::Manual(5_600);
    controls[Control::HFlip.index()] = ControlValue::Flag(true);
    controls[Control::Crop.index()] = ControlValue::Crop(Some(Rect {
        x: 0,
        y: 0,
        width: 640,
        height: 480,
    }));
    FrameMetadata {
        controls,
        sensor_timestamp_ns: 123_456_789,
        exposure_us: 12_000,
        analogue_gain_milli: 2_500,
        digital_gain_milli: 1_000,
        lux: Some(412.5),
        colour_temperature_k: Some(5_600),
        achieved_fps_milli: 0,
    }
}

#[test]
fn default_metadata_is_all_auto_and_zero() {
    let meta = FrameMetadata::default();
    assert_eq!(meta.sensor_timestamp_ns, 0);
    assert_eq!(meta.lux, None);
    assert_eq!(meta.colour_temperature_k, None);
    for control in Control::ALL {
        assert_eq!(meta.control(control), ControlValue::Auto);
    }
}

#[test]
fn metadata_is_plain_data_a_pool_can_hold() {
    fn plain<T: Default + Send + Sync + Copy>() {}
    plain::<FrameMetadata>();
}

#[test]
fn metadata_round_trips_through_a_pool_lease() {
    let pool: FramePool<FrameMetadata> = FramePool::host(2, BufferSize::new(64, 0)).unwrap();

    let mut lease = pool.acquire(entry(7, 555)).unwrap();
    assert_eq!(
        *lease.meta(),
        FrameMetadata::default(),
        "a fresh frame starts from default"
    );
    *lease.meta_mut().unwrap() = sample();

    let shared = lease.share();
    drop(lease);
    assert_eq!(*shared.meta(), sample());
    assert_eq!(shared.id(), 7);
    assert_eq!(shared.timestamp_ns(), 555);
    assert_eq!(
        shared.meta().control(Control::Exposure),
        ControlValue::Manual(12_000)
    );
    assert_eq!(shared.meta().control(Control::Focus), ControlValue::Auto);
}

#[test]
fn a_recycled_buffer_takes_the_new_frames_metadata() {
    let pool: FramePool<FrameMetadata> = FramePool::host(1, BufferSize::new(64, 0)).unwrap();
    let mut first = pool.acquire(entry(1, 10)).unwrap();
    *first.meta_mut().unwrap() = sample();
    drop(first);

    let mut second = pool.acquire(entry(2, 20)).unwrap();
    assert_eq!(second.id(), 2);
    *second.meta_mut().unwrap() = FrameMetadata {
        sensor_timestamp_ns: 20,
        ..FrameMetadata::default()
    };
    assert_eq!(second.meta().sensor_timestamp_ns, 20);
    assert_eq!(
        *second.meta(),
        FrameMetadata {
            sensor_timestamp_ns: 20,
            ..FrameMetadata::default()
        }
    );
}

#[cfg(feature = "serde")]
#[test]
fn metadata_round_trips_through_serde_json() {
    let meta = sample();
    let json = serde_json::to_string(&meta).unwrap();
    let back: FrameMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(back, meta);
}

#[cfg(feature = "serde")]
#[test]
fn control_values_round_trip_through_serde_json() {
    for value in [
        ControlValue::Auto,
        ControlValue::Manual(30_000),
        ControlValue::Flag(true),
        ControlValue::Crop(None),
        ControlValue::Crop(Some(Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        })),
    ] {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<ControlValue>(&json).unwrap(), value);
    }
    let json = serde_json::to_string(&Control::FrameRate).unwrap();
    assert_eq!(json, "\"frame_rate\"");
}
