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

mod frame_views {
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};

    use ::daedalus::transport::{
        DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, FrameInterface, FrameResidency as View,
        TypeKey, fourcc,
    };
    use smallvec::smallvec;

    use super::*;

    /// A dma-buf stand-in (any descriptor works): `cpu` decides whether it is mapped. Its
    /// planes start `BASE` bytes into the buffer.
    struct Dmabuf {
        fd: OwnedFd,
        bytes: Vec<u8>,
        cpu: CpuAccess,
    }

    const BASE: usize = 4096;

    impl ExternalBacking for Dmabuf {
        fn plane_data(&self, _index: usize) -> Option<&[u8]> {
            // Bytes even when "unmapped": the view must not hand them out then.
            Some(&self.bytes)
        }
        fn residency(&self) -> FrameResidency {
            FrameResidency::Dmabuf
        }
        fn cpu_access(&self) -> CpuAccess {
            self.cpu
        }
        fn dmabuf_plane(&self, _index: usize) -> Option<DmabufPlane<'_>> {
            Some(DmabufPlane {
                fd: self.fd.as_fd(),
                offset: BASE,
            })
        }
    }

    fn nv12_dmabuf(cpu: CpuAccess) -> FrameLease {
        let (w, h) = (8usize, 4usize);
        let res = Resolution::new(w as u32, h as u32).unwrap();
        let mut meta = FrameMeta::new(MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709), 99)
            .with_backend(BackendFrameMeta::Native(NativeFrameMeta {
                sequence: 5,
                ..Default::default()
            }));
        meta.clock = Some(TimestampClock::Monotonic);
        let layout = |offset, len| PlaneLayout {
            offset,
            len,
            stride: w,
        };
        let backing = Dmabuf {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            bytes: (0..w * h * 3 / 2).map(|i| i as u8).collect(),
            cpu,
        };
        FrameLease::from_external(
            meta,
            smallvec![layout(0, w * h), layout(w * h, w * h / 2)],
            crate::buffer::shared_backing(backing),
        )
    }

    #[test]
    fn host_frames_are_viewed_in_place() {
        let frame = grey(16, 8, 3);
        let view = frame_view(&frame);
        assert_eq!((view.width(), view.height()), (16, 8));
        assert_eq!(view.format(), fourcc(b"GREY"));
        assert_eq!(view.modifier(), DRM_FORMAT_MOD_LINEAR);
        assert_eq!((view.timestamp_ns(), view.sequence()), (3, 0));
        assert_eq!(view.residency(), View::Cpu);
        assert_eq!(view.plane_count(), 1);
        let plane = view.plane(0).unwrap();
        let data = plane.data.unwrap();
        // The lease's own bytes, not a copy.
        assert_eq!(data.as_ptr(), frame.planes()[0].data().as_ptr());
        assert_eq!((data.len(), plane.len, plane.stride), (128, 128, 16));
        assert_eq!((plane.offset, plane.dmabuf_fd), (0, None));
        assert!(view.plane(1).is_none());
    }

    #[test]
    fn unmapped_dmabufs_give_descriptors_not_pointers() {
        let frame = nv12_dmabuf(CpuAccess::None);
        let fd = frame.dmabuf_plane(0).unwrap().fd.as_raw_fd();
        let view = frame_view(&frame);
        assert_eq!(view.format(), fourcc(b"NV12"));
        assert_eq!(view.sequence(), 5);
        assert_eq!(view.residency(), View::External);
        let planes: Vec<_> = view.planes().collect();
        assert_eq!(planes.len(), 2);
        for (plane, (offset, len)) in planes.iter().zip([(0, 32), (32, 16)]) {
            assert_eq!(plane.data, None, "a host pointer into unmapped memory");
            assert_eq!(plane.dmabuf_fd, Some(fd));
            assert_eq!(plane.offset as usize, BASE + offset);
            assert_eq!((plane.len, plane.stride), (len, 8));
        }
    }

    #[test]
    fn mapped_dmabufs_give_both() {
        let frame = nv12_dmabuf(CpuAccess::Uncached);
        let view = frame_view(&frame);
        assert_eq!(view.residency(), View::External);
        let chroma = view.plane(1).unwrap();
        assert_eq!(
            chroma.data.unwrap().as_ptr(),
            frame.planes()[1].data().as_ptr()
        );
        assert_eq!(chroma.data.unwrap()[0], 32);
        assert!(chroma.dmabuf_fd.is_some());
    }

    #[test]
    fn gpu_textures_and_packets_say_what_they_are() {
        let mut gpu = nv12_dmabuf(CpuAccess::Cached);
        gpu.meta_mut().residency = Some(FrameResidency::GpuTexture);
        let view = frame_view(&gpu);
        assert_eq!(view.residency(), View::Gpu);
        assert!(view.planes().all(|plane| plane.data.is_none()));

        let mut packet = grey(16, 8, 1);
        packet.meta_mut().format.code = FourCc::H264;
        let view = frame_view(&packet);
        assert_eq!(
            (view.format(), view.modifier()),
            (0, DRM_FORMAT_MOD_INVALID)
        );
    }

    #[test]
    fn payloads_are_viewed_through_the_registered_provider() {
        let mut registry = PluginRegistry::new();
        registry.install(&StyxFramesPlugin::new()).unwrap();
        assert!(
            registry
                .foreign_interfaces()
                .contains_key(&TypeKey::new("daedalus:frame"))
        );

        let frame = Arc::new(grey(16, 8, 1));
        let payload = shared_frame_payload(frame.clone())
            .provide_foreign::<FrameLease, FrameInterface>()
            .unwrap();
        let view = payload
            .foreign_borrow()
            .unwrap()
            .view::<FrameInterface>()
            .unwrap();
        let data = view.plane(0).unwrap().data.unwrap();
        assert_eq!(data.as_ptr(), frame.planes()[0].data().as_ptr());
        // The payload still holds the lease itself, and keeps it alive.
        assert_eq!(Arc::strong_count(&frame), 2);
        drop(payload);
        assert_eq!(Arc::strong_count(&frame), 1);
    }
}
