use std::sync::Arc;

use ::daedalus::runtime::plugins::PluginRegistry;
use ::daedalus::transport::Residency;

use super::*;
use crate::prelude::*;

fn grey(width: u32, height: u32, timestamp: u64) -> FrameLease {
    let len = (width * height) as usize;
    let mut buf = BufferPool::with_limits(1, len, 1).lease();
    buf.resize(len);
    let res = Resolution::new(width, height).unwrap();
    let mut meta = FrameMeta::new(
        MediaFormat::new(FourCc::GREY, res, ColorSpace::Bt709),
        timestamp,
    );
    meta.clock = Some(TimestampClock::Monotonic);
    FrameLease::single_plane(meta, buf, len, width as usize)
}

#[test]
fn descriptors_describe_frames_without_their_pixels() {
    let frame = grey(64, 32, 7)
        .with_companion(CompanionKind::Pyramid { level: 1 }, grey(32, 16, 7))
        .unwrap()
        .crop_view(FrameRect::new(8, 4, 32, 16))
        .unwrap();
    let d = FrameDescriptor::of(&frame);
    assert_eq!((d.format.as_str(), d.width, d.height), ("GREY", 32, 16));
    assert_eq!(d.color, "bt709");
    assert_eq!((d.timestamp_ns, d.clock.as_deref()), (7, Some("monotonic")));
    assert_eq!(
        d.crop,
        Some(RegionDescriptor {
            x: 8,
            y: 4,
            width: 32,
            height: 16
        })
    );
    assert_eq!(d.planes.len(), 1);
    assert_eq!(d.planes[0].stride, 64);
    assert_eq!(
        (d.residency.as_str(), d.cpu_access.as_str()),
        ("host_owned", "cached")
    );
    assert_eq!(d.companions.len(), 1);
    let c = &d.companions[0];
    assert_eq!(
        (c.kind.as_str(), c.level, c.width, c.height),
        ("pyramid", 1, 16, 8)
    );
}

#[test]
fn payloads_share_the_frame_and_say_where_it_lives() {
    let frame = Arc::new(grey(16, 8, 1));
    let payload = shared_frame_payload(frame.clone());
    assert_eq!(payload.type_key().as_str(), FRAME_TYPE_KEY);
    assert_eq!(payload.residency(), Residency::Cpu);
    assert_eq!(payload.bytes_estimate(), Some(128));
    // The same frame, not a copy.
    let inside = payload.get_arc::<FrameLease>().unwrap();
    assert!(Arc::ptr_eq(&inside, &frame));

    // Memory owned elsewhere (a dma-buf here) is External.
    let mut external = grey(16, 8, 1).into_shareable();
    external.meta_mut().residency = Some(FrameResidency::Dmabuf);
    assert_eq!(frame_payload(external).residency(), Residency::External);
}

#[test]
fn the_plugin_installs_once_per_registry() {
    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new()).unwrap();
    assert!(registry.installed_plugin_version("styx.frames").is_some());
}

#[test]
fn the_plugin_registers_this_build_so_conflicts_name_the_feature_difference() {
    use ::daedalus::runtime::plugins::CrateBuildInfo;

    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new()).unwrap();
    let host = registry.crate_builds()["styx_core"];
    assert_eq!(host.version, env!("CARGO_PKG_VERSION"));
    assert!(host.feature_list().contains(&"daedalus"));

    // A dynamic plugin whose styx-core build lacks `std` and has a feature the host lacks:
    // the diff names exactly those two.
    let mut features: Vec<&str> = host.feature_list();
    features.retain(|feature| *feature != "std");
    features.push("plugin-only");
    let plugin = CrateBuildInfo {
        features: String::leak(features.join(",")),
        ..host
    };
    let diffs = registry.crate_build_diffs([&plugin]);
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].missing_in_plugin(), ["std"]);
    assert_eq!(diffs[0].extra_in_plugin(), ["plugin-only"]);
    let text = diffs[0].to_string();
    assert!(text.starts_with("crate `styx_core`"), "{text}");
    assert!(
        text.contains("missing in plugin: std; only in plugin: plugin-only"),
        "{text}"
    );
    // The same build is no conflict.
    assert!(registry.crate_build_diffs([&host]).is_empty());
}
