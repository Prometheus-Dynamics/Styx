use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;

use smallvec::smallvec;

use super::*;
use crate::buffer::Hop;
use crate::prelude::*;

fn memfd(bytes: &[u8]) -> OwnedFd {
    // SAFETY: creating an anonymous memory file; a non-negative result is ours.
    let fd = unsafe { libc::memfd_create(c"lease-codec".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: `fd` was just created.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let file = File::from(fd.try_clone().unwrap());
    file.set_len(bytes.len() as u64).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    fd
}

fn nv12_meta() -> FrameMeta {
    let res = Resolution::new(64, 32).unwrap();
    FrameMeta::new(MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709), 7)
}

fn layout(offset: usize, len: usize) -> PlaneLayout {
    PlaneLayout {
        offset,
        len,
        stride: 64,
    }
}

/// NV12 64x32 in one memfd: luma 1s then chroma 2s.
fn memfd_frame() -> FrameLease {
    let mut bytes = vec![1u8; 2048];
    bytes.extend_from_slice(&[2; 1024]);
    FrameLease::from_memfd(
        nv12_meta(),
        smallvec![layout(0, 2048), layout(2048, 1024)],
        memfd(&bytes),
    )
}

/// NV12 64x32, each plane in its own descriptor.
fn dmabuf_frame() -> FrameLease {
    let planes = vec![
        FrameFdPlane {
            fd: memfd(&[3; 2048]),
            offset: 0,
            len: 2048,
        },
        FrameFdPlane {
            fd: memfd(&[4; 1024]),
            offset: 0,
            len: 1024,
        },
    ];
    FrameLease::from_dmabuf(
        nv12_meta(),
        smallvec![layout(0, 2048), layout(0, 1024)],
        planes,
    )
    .unwrap()
}

const DESCRIPTOR_MEMFD: &str = r#"{"width":64,"height":32,"fourcc":"NV12","timestamp":7,"color":"Bt709","planes":[{"offset":0,"len":2048,"stride":64},{"offset":2048,"len":1024,"stride":64}]}"#;
const DESCRIPTOR_DMABUF: &str = r#"{"width":64,"height":32,"fourcc":"NV12","timestamp":7,"color":"Bt709","planes":[{"offset":0,"len":2048,"stride":64},{"offset":0,"len":1024,"stride":64}]}"#;

/// The JSON bytes peers already read and write: they must not change.
#[test]
fn the_wire_bytes_are_the_frame_sockets() {
    let (payload, fds) = encode(&memfd_frame()).unwrap();
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        format!(r#"{{"descriptor":{DESCRIPTOR_MEMFD},"backing":{{"kind":"memfd","len":3072}}}}"#)
    );
    assert_eq!(fds.len(), 1);

    let (payload, fds) = encode(&dmabuf_frame()).unwrap();
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        format!(
            r#"{{"descriptor":{DESCRIPTOR_DMABUF},"backing":{{"kind":"dmabuf_planes","planes":[{{"offset":0,"len":2048}},{{"offset":0,"len":1024}}]}}}}"#
        )
    );
    assert_eq!(fds.len(), 2);

    let (framed, _) = encode_framed(&memfd_frame()).unwrap();
    let len = payload_len(framed[..HEADER_LEN].try_into().unwrap()).unwrap();
    assert_eq!(len, framed.len() - HEADER_LEN);
}

#[test]
fn decode_gives_back_the_frame() {
    for (frame, luma, chroma) in [(memfd_frame(), 1, 2), (dmabuf_frame(), 3, 4)] {
        let (payload, fds) = encode(&frame).unwrap();
        let back = decode(&payload, fds).unwrap();
        assert_eq!(back.descriptor(), frame.descriptor());
        let planes = back.planes();
        assert!(planes[0].data().iter().all(|&b| b == luma));
        assert!(planes[1].data().iter().all(|&b| b == chroma));
    }
}

#[test]
fn heap_frames_are_copied_into_a_memfd() {
    let grey = FrameLease::from_visible_bytes(
        MediaFormat::new(
            FourCc::GREY,
            Resolution::new(16, 4).unwrap(),
            ColorSpace::Srgb,
        ),
        3,
        &[9; 64],
    )
    .unwrap();
    let (payload, fds) = encode(&grey).unwrap();
    let message = LeaseMessage::parse(&payload).unwrap();
    assert_eq!(message.backing, LeaseBacking::Memfd { len: 64 });
    let back = decode(&payload, fds).unwrap();
    assert_eq!(back.planes()[0].data(), &[9; 64]);
}

#[test]
fn decode_checks_counts_and_sizes() {
    let (payload, _) = encode(&dmabuf_frame()).unwrap();
    let one = vec![memfd(&[0; 2048])];
    assert!(matches!(
        decode(&payload, one),
        Err(LeaseCodecError::FdCount {
            expected: 2,
            actual: 1,
            ..
        })
    ));
    let five = (0..5).map(|_| memfd(&[0; 2048])).collect();
    assert!(matches!(
        decode(&payload, five),
        Err(LeaseCodecError::TooManyFds(5))
    ));
    // The chroma plane's descriptor is smaller than the plane it should hold.
    let small = vec![memfd(&[0; 2048]), memfd(&[0; 512])];
    assert!(matches!(
        decode(&payload, small),
        Err(LeaseCodecError::Frame(_))
    ));

    let (payload, _) = encode(&memfd_frame()).unwrap();
    assert!(matches!(
        decode(&payload, vec![memfd(&[0; 3072]), memfd(&[0; 3072])]),
        Err(LeaseCodecError::FdCount { expected: 1, .. })
    ));
    // A memfd shorter than the message says.
    assert!(matches!(
        decode(&payload, vec![memfd(&[0; 2048])]),
        Err(LeaseCodecError::MemfdSize { len: 3072 })
    ));
    // A `len` that does not hold the planes.
    let mut message = LeaseMessage::parse(&payload).unwrap();
    message.backing = LeaseBacking::Memfd { len: 2048 };
    assert!(matches!(
        decode(&message.to_json().unwrap(), vec![memfd(&[0; 3072])]),
        Err(LeaseCodecError::MemfdSize { len: 2048 })
    ));

    assert!(matches!(
        decode(&vec![b' '; MAX_PAYLOAD + 1], Vec::new()),
        Err(LeaseCodecError::PayloadTooLarge(_))
    ));
    assert!(matches!(
        payload_len(((MAX_PAYLOAD + 1) as u32).to_le_bytes()),
        Err(LeaseCodecError::PayloadTooLarge(_))
    ));
    assert!(matches!(
        decode(b"{\"backing\":", vec![memfd(&[0; 1])]),
        Err(LeaseCodecError::Json(_))
    ));
}

#[test]
fn endpoint_records() {
    let path = Path::new("/run/styx/front.sock");
    assert_eq!(
        endpoint_uri(path).as_deref(),
        Some("styx-frame-lease+unix:///run/styx/front.sock")
    );
    assert_eq!(endpoint_payload(path), Some("/run/styx/front.sock"));
    assert_eq!(endpoint_uri(Path::new("run/front.sock")), None);
    assert_eq!(
        parse_endpoint_uri("STYX-Frame-Lease+Unix:///run/styx/front.sock").as_deref(),
        Some(path)
    );
    assert_eq!(
        parse_endpoint("styx-frame-lease+unix", "/run/styx/front.sock").as_deref(),
        Some(path)
    );
    assert_eq!(parse_endpoint_uri("unix:///run/styx/front.sock"), None);
    assert_eq!(parse_endpoint_uri("styx-frame-lease+unix://run/x"), None);
    assert_eq!(parse_endpoint_uri("/run/styx/front.sock"), None);
}

/// A frame with hops carries them as the last member; the other members' bytes are the same.
#[test]
fn hops_are_the_last_member_and_come_back() {
    let mut frame = memfd_frame();
    let hops = &mut frame.meta_mut().hops;
    hops.set_sequence(Some(42));
    hops.set(Hop::Sensor, 1_000);
    hops.set(Hop::Dequeued, 9_000);
    hops.set(Hop::IspDone, 11_000);
    hops.set(Hop::Queued, 11_200);
    hops.set(Hop::Taken, 11_300);
    let (payload, fds) = encode(&frame).unwrap();
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        format!(
            r#"{{"descriptor":{DESCRIPTOR_MEMFD},"backing":{{"kind":"memfd","len":3072}},"hops":{{"sequence":42,"sensor":1000,"dequeued":9000,"isp_done":11000,"queued":11200,"taken":11300}}}}"#
        )
    );
    let back = decode(&payload, fds).unwrap();
    assert_eq!(back.meta().hops, frame.meta().hops);
    assert_eq!(back.meta().sequence(), Some(42));
    assert_eq!(back.meta().hop_record().sent, None);

    // A transport adds its send time and writes the message framed, into a reused buffer.
    let (mut message, _fds) = encode_message(&frame).unwrap();
    message.hops.as_mut().unwrap().sent = Some(11_400);
    let mut out = Vec::new();
    write_framed(&message, &mut out).unwrap();
    let len = payload_len(out[..HEADER_LEN].try_into().unwrap()).unwrap();
    assert_eq!(len, out.len() - HEADER_LEN);
    assert!(
        std::str::from_utf8(&out[HEADER_LEN..])
            .unwrap()
            .ends_with(r#""taken":11300,"sent":11400}}"#)
    );

    // Copies count into the record; a peer's members it does not know are skipped.
    let parsed = LeaseMessage::parse(
        format!(
            r#"{{"descriptor":{DESCRIPTOR_MEMFD},"backing":{{"kind":"memfd","len":3072}},"hops":{{"sensor":5,"copies":1,"copied_bytes":3072,"future":true}},"other":1}}"#
        )
        .as_bytes(),
    )
    .unwrap();
    let record = parsed.hops.unwrap();
    assert_eq!(
        (record.sensor, record.copies, record.copied_bytes),
        (Some(5), 1, 3072)
    );
    // Old messages (no hops) parse with none.
    let (payload, _) = encode(&dmabuf_frame()).unwrap();
    assert!(LeaseMessage::parse(&payload).unwrap().hops.is_none());
}

/// A heap frame copied into a memfd says so in its record.
#[test]
fn memfd_copies_are_counted_in_the_record() {
    let grey = FrameLease::from_visible_bytes(
        MediaFormat::new(
            FourCc::GREY,
            Resolution::new(16, 4).unwrap(),
            ColorSpace::Srgb,
        ),
        3,
        &[9; 64],
    )
    .unwrap();
    let (message, _) = encode_message(&grey).unwrap();
    let record = message.hops.unwrap();
    assert_eq!((record.copies, record.copied_bytes), (1, 64));
}
