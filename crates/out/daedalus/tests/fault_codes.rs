//! Styx's fault codes: each code is produced by its real cause where a test can make the cause, and
//! by a constructed underlying error where it can't (the stand-ins are marked). Each test checks
//! the code the step returns and that the detail carries the underlying words intact.
//!
//! The table and the docs are checked here too, so the enum, [`TABLE`] and `src/fault_table.md`
//! cannot drift apart.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::ptr::NonNull;

use daedalus::prelude::*;
use daedalus_abi::{StepCx, StepEnd, keep_detail};
use daedalus_cpu_author::{Ctx, Fault};
use daedalus_wgpu::OpenError;
use lemnos_linux_sys::dma_buf;
use styx_daedalus::StyxFrame;
use styx_daedalus::fault::{
    FaultCode, OrionRefusal, StyxFault, TABLE, check_bytes, check_format, check_frame,
    check_planes, check_writable, device_fault, fail, layout_fault, open_fault, orion_fault,
    orion_refusal_fault, socket_fault,
};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FramePool, MemoryKind};

mod support;

/// GPU memory: no CPU mapping, no descriptor. A real cause for a step that needs the CPU's bytes.
struct GpuBuffer {
    len: usize,
}

// SAFETY: `cpu_ptr` is `None`, so there are no bytes for the CPU to read or write and nothing to
// keep valid; the other methods only report what the buffer is.
unsafe impl BufferMemory for GpuBuffer {
    fn kind(&self) -> MemoryKind {
        MemoryKind::Gpu
    }

    fn len(&self) -> usize {
        self.len
    }

    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        None
    }
}

/// Runs a fault through a step's context, as a step does, and gives the code, the reason and the
/// detail the host keeps.
fn step_fault(fault: &StyxFault) -> (u32, String, String) {
    let mut cx = StepCx::new();
    let kept = keep_detail(&mut cx);
    let end = fail(&mut cx, 0, fault);
    assert!(
        matches!(end, StepEnd::Fault),
        "a fault returns StepEnd::Fault"
    );
    (cx.code, cx.reason().to_string(), kept.text())
}

fn grey_64() -> MediaFormat {
    MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 64).unwrap())
}

#[test]
fn other_is_zero_and_keeps_its_words() {
    let fault = StyxFault::other("a fault with no kind", "the raw error: 42 widgets");
    let (code, reason, detail) = step_fault(&fault);
    assert_eq!(code, 0);
    assert_eq!(FaultCode::Other as u32, 0);
    assert_eq!(reason, "a fault with no kind");
    assert!(detail.contains("the raw error: 42 widgets"), "{detail}");
}

// Stand-in: no layout error is a type at the current locks, so the words and the kind are given.
#[test]
fn a_layout_refusal_at_attach_is_layout_refused() {
    let fault = layout_fault(3, 0xabc, 0xdef);
    let (code, reason, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::LayoutRefused as u32);
    assert_eq!(code, 1);
    assert!(reason.contains("layout"), "{reason}");
    assert!(detail.contains("kind 3"), "the kind is named: {detail}");
    assert!(detail.contains("expected 0xabc, found 0xdef"), "{detail}");
}

/// A layout refusal from the real handshake type: the socket's `SocketError::Layout` is what a
/// frame socket's attach returns, so the code is produced from it, not from words.
#[test]
fn a_socket_handshake_layout_refusal_is_layout_refused_with_both_values() {
    let error = styx_socket::SocketError::Layout {
        which: styx_socket::SharedLayout::Marks,
        found: 1,
        expected: 2,
    };
    let fault = socket_fault(&error).expect("a layout refusal has a code");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::LayoutRefused as u32);
    assert!(detail.contains("this marks page is layout 1"), "{detail}");
    assert!(detail.contains("reads layout 2"), "{detail}");
    // A version refusal is not a layout refusal: the caller reports it with its own code.
    let version = styx_socket::SocketError::Version { ours: 5, theirs: 4 };
    assert!(socket_fault(&version).is_none());
}

/// Orion's refusals as the stand-in carries them: each has its own code, named with the node.
#[test]
fn orion_revision_and_feature_refusals_have_their_own_codes() {
    let revision = OrionRefusal::Revision {
        node: "hdr-meta".to_string(),
        client: "0.9.4".to_string(),
    };
    let (code, reason, detail) = step_fault(&orion_refusal_fault(&revision));
    assert_eq!(code, FaultCode::OrionRevision as u32);
    assert_eq!(code, 10);
    assert!(reason.contains("revision"), "{reason}");
    assert!(
        detail.contains("node hdr-meta") && detail.contains("0.9.4"),
        "{detail}"
    );

    let level = OrionRefusal::Level {
        feature: "tone-map".to_string(),
        needs: "2".to_string(),
        node: "hdr-meta".to_string(),
    };
    let (code, reason, detail) = step_fault(&orion_refusal_fault(&level));
    assert_eq!(code, FaultCode::OrionFeature as u32);
    assert_eq!(code, 11);
    assert!(reason.contains("feature"), "{reason}");
    assert!(
        detail.contains("feature tone-map needs level 2") && detail.contains("hdr-meta"),
        "{detail}"
    );
}

/// The real refusal types are Orion's `ClientError::Revision` and `ClientError::Level`. They are
/// not at the pinned Orion revision, so this test cannot name them yet.
#[test]
#[ignore = "needs Orion's rev with ClientError::Revision/Level"]
fn orion_s_own_refusals_map_to_the_revision_and_feature_codes() {
    unimplemented!("map Orion's ClientError::Revision and ClientError::Level into OrionRefusal");
}

// Stand-in: the pre-existing code for an Orion refusal with no kind of its own.
#[test]
fn an_orion_refusal_is_orion_refused() {
    let fault = orion_fault(7, "0.9.4", "hdr-meta");
    let (code, reason, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::OrionRefused as u32);
    assert_eq!(code, 2);
    assert!(reason.contains("Orion"), "{reason}");
    assert!(detail.contains("kind 7"), "{detail}");
    assert!(
        detail.contains("revision 0.9.4, feature hdr-meta"),
        "{detail}"
    );
}

#[test]
fn a_step_that_needs_the_cpu_bytes_of_gpu_memory_is_stream_not_mapped() {
    let memory = GpuBuffer { len: 4096 };
    let fault = check_bytes(&memory).expect_err("GPU memory has no CPU bytes");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::StreamNotMapped as u32);
    assert_eq!(code, 3);
    assert!(detail.contains("Gpu memory of 4096 bytes"), "{detail}");
}

#[test]
fn a_step_that_writes_a_read_only_mapping_is_stream_read_only() {
    let len = 4096;
    let fd = dma_buf::sealed_memfd(c"styx-daedalus-readonly", len as u64).expect("a memfd");
    let memory = FdMemory::memfd_at(fd, 0, len, false).expect("mapped read-only");
    assert!(memory.cpu_ptr().is_some(), "it is mapped, for reading");
    let fault = check_writable(&memory).expect_err("the mapping is read-only");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::StreamReadOnly as u32);
    assert_eq!(code, 4);
    assert!(detail.contains("mapped for reading only"), "{detail}");
}

#[test]
fn a_refused_gpu_is_no_gpu_access_with_the_nodes_and_the_error_text() {
    // Constructed: this user can open the GPU here, so the refusal is made as Daedalus makes it.
    let error = OpenError::NoAccess("/dev/dri/card0, /dev/dri/card1".into());
    let fault = open_fault(&error);
    let (code, reason, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::NoGpuAccess as u32);
    assert_eq!(code, 5);
    assert!(reason.contains("refused"), "{reason}");
    assert!(
        detail.contains("this user can't open /dev/dri/card0, /dev/dri/card1"),
        "the error's own words: {detail}"
    );
    assert!(
        detail.contains("user lacks group"),
        "the group line: {detail}"
    );
}

#[test]
fn a_device_failure_is_device_failed_with_the_devices_words() {
    let error = OpenError::Device("the queue was lost".into());
    let (code, _, detail) = step_fault(&open_fault(&error));
    assert_eq!(code, FaultCode::DeviceFailed as u32);
    assert_eq!(code, 6);
    assert!(
        detail.contains("no adapter gave a device: the queue was lost"),
        "{detail}"
    );

    // A job the device refused, as a step reports it.
    let (code, _, detail) = step_fault(&device_fault("the job was refused: device gone"));
    assert_eq!(code, FaultCode::DeviceFailed as u32);
    assert!(
        detail.contains("the job was refused: device gone"),
        "{detail}"
    );
}

#[test]
fn a_frame_with_neither_a_dma_buf_nor_bytes_is_no_data_path() {
    let pool: FramePool<()> = FramePool::new(
        BufferSize::new(4096, 0),
        vec![Box::new(GpuBuffer { len: 4096 }) as Box<dyn BufferMemory>],
    )
    .expect("a pool");
    let lease = pool.acquire(support::entry(7)).expect("a buffer");
    let frame = StyxFrame::from_lease(lease, pool.buffer_size()).expect("a frame");
    let fault = check_frame(&frame).expect_err("GPU memory is neither");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::NoDataPath as u32);
    assert_eq!(code, 7);
    assert!(detail.contains("frame 7 is in Gpu memory"), "{detail}");
}

#[test]
fn a_step_given_another_pixel_format_is_unsupported_format() {
    let yuyv = MediaFormat::with_defaults(FourCc::YUYV, Resolution::new(64, 64).unwrap());
    let fault = check_format(&yuyv, FourCc::GREY).expect_err("YUYV is not GREY");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::UnsupportedFormat as u32);
    assert_eq!(code, 8);
    assert!(
        detail.contains("takes GREY, and the frame is YUYV"),
        "{detail}"
    );
    check_format(&grey_64(), FourCc::GREY).expect("GREY is taken");
}

#[test]
fn a_buffer_smaller_than_the_frames_planes_is_plane_layout_mismatch() {
    // A 64x64 GREY frame needs 4096 bytes in its plane; a 100-byte buffer cannot hold it.
    let fault = check_planes(100, &grey_64()).expect_err("100 bytes is short");
    let (code, _, detail) = step_fault(&fault);
    assert_eq!(code, FaultCode::PlaneLayoutMismatch as u32);
    assert_eq!(code, 9);
    assert!(detail.contains("needs 4096 bytes"), "{detail}");
    assert!(detail.contains("the buffer holds 100"), "{detail}");
    check_planes(4096, &grey_64()).expect("a buffer of the plane's size holds it");
}

#[test]
fn the_published_numbers_and_names_do_not_change() {
    let published = [
        (FaultCode::Other, 0, "Other"),
        (FaultCode::LayoutRefused, 1, "LayoutRefused"),
        (FaultCode::OrionRefused, 2, "OrionRefused"),
        (FaultCode::StreamNotMapped, 3, "StreamNotMapped"),
        (FaultCode::StreamReadOnly, 4, "StreamReadOnly"),
        (FaultCode::NoGpuAccess, 5, "NoGpuAccess"),
        (FaultCode::DeviceFailed, 6, "DeviceFailed"),
        (FaultCode::NoDataPath, 7, "NoDataPath"),
        (FaultCode::UnsupportedFormat, 8, "UnsupportedFormat"),
        (FaultCode::PlaneLayoutMismatch, 9, "PlaneLayoutMismatch"),
        (FaultCode::OrionRevision, 10, "OrionRevision"),
        (FaultCode::OrionFeature, 11, "OrionFeature"),
        (FaultCode::ContractMismatch, 12, "ContractMismatch"),
    ];
    for (code, number, name) in published {
        assert_eq!(code as u32, number, "{name}");
        assert_eq!(format!("{code:?}"), name);
    }
    assert_eq!(
        FaultCode::ALL.len(),
        published.len(),
        "ALL lists every code"
    );
    for (index, code) in FaultCode::ALL.iter().enumerate() {
        assert_eq!(*code as u32, index as u32, "ALL is in number order");
    }
}

#[test]
fn the_table_is_the_enum() {
    assert_eq!(TABLE.len(), FaultCode::ALL.len());
    for code in FaultCode::ALL {
        let rows: Vec<_> = TABLE.iter().filter(|(c, _, _)| c == code).collect();
        assert_eq!(rows.len(), 1, "{code:?} has one row");
        let (_, name, meaning) = rows[0];
        assert_eq!(
            *name,
            format!("{code:?}"),
            "the row's name is the variant's"
        );
        assert!(!meaning.is_empty(), "{code:?} says what it means");
    }
}

/// The docs' table, rendered from [`TABLE`].
fn markdown() -> String {
    let mut out = String::from("| Code | Name | Meaning |\n|---|---|---|\n");
    for (code, name, meaning) in TABLE {
        out.push_str(&format!("| {} | {} | {} |\n", *code as u32, name, meaning));
    }
    out
}

#[test]
fn the_docs_table_is_the_table() {
    let docs = include_str!("../src/fault_table.md");
    assert_eq!(
        docs,
        markdown(),
        "src/fault_table.md is the table; regenerate it from TABLE"
    );
    let readme = include_str!("../README.md");
    assert!(readme.contains(docs), "README.md carries the same table");
}

/// Host-side: the code a step returns should reach the host's fault report. At the lock this was
/// written at, the runner drops the code, so this only checks the reason and the detail. At the
/// rev that carries the code, the same test also asserts the code (the note at the end).
#[node(id = "styx_test.coded_fault", inputs(x), outputs(out))]
fn coded_fault(x: i64, ctx: &Ctx) -> Result<i64, Fault> {
    if x < 0 {
        return Ok(x);
    }
    ctx.detail_fmt(format_args!(
        "the mapping is read-only: GPU memory of 4096 bytes, mapped for reading only\n"
    ));
    Err(Fault {
        code: FaultCode::StreamReadOnly as u32,
        reason: "a step wrote to a read-only buffer",
    })
}

#[test]
fn a_steps_code_reaches_the_host() {
    let mut registry = Registry::new();
    registry
        .register_all(&[&CODED_FAULT_NODE])
        .expect("the node registers");
    let graph = GraphDoc::parse(
        r#"{"format": "daedalus.graph", "schema_version": 1, "graph": {
  "nodes": [
    {"id": "host", "kind": "host_bridge", "port_types": {"x": "i64", "out": "i64"}},
    {"id": "styx_test.coded_fault"}
  ],
  "edges": [
    {"from": {"node": 0, "port": "x"}, "to": {"node": 1, "port": "x"}},
    {"from": {"node": 1, "port": "out"}, "to": {"node": 0, "port": "out"}}
  ]
}}"#,
    )
    .expect("the graph parses")
    .graph;
    let planned = plan(
        &graph,
        &registry,
        &[&daedalus_cpu::CpuBackend],
        &Config::default(),
    )
    .expect("it plans");
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("it loads");
    loaded
        .host()
        .feed_typed("x", std::sync::Arc::new(1_i64))
        .expect("fed");
    let _ = loaded
        .host()
        .step_until_settled(std::time::Duration::from_secs(5));
    let (reason, reason_cut, code, own_code, has_detail, step) = {
        let report = loaded.fault_reports().next().expect("one fault");
        (
            report.reason().to_string(),
            report.reason_cut,
            report.code,
            report.own_code,
            report.detail,
            report.step,
        )
    };
    assert_eq!(reason, "a step wrote to a read-only buffer");
    assert!(!reason_cut, "the reason is short: not cut");
    // The node's own code, passed through untouched (not one of Daedalus's own numbers).
    assert_eq!(
        code,
        FaultCode::StreamReadOnly as u32,
        "the code reaches the host"
    );
    assert!(!own_code, "the code is the node's, not Daedalus's");
    assert!(has_detail, "the step gave a detail");
    let detail = loaded
        .fault_detail(step)
        .expect("the detail reached the host");
    assert!(!detail.cut, "the detail is short: not cut");
    assert!(
        detail.text.contains("mapped for reading only"),
        "{}",
        detail.text
    );
}

#[test]
fn a_contract_mismatch_has_its_own_code_and_keeps_both_hashes() {
    // The board's Lemnos refuses a bind for a contract another build lays out differently: its own
    // code (12), and the detail keeps the Lemnos error's words with both base hashes.
    let fault = StyxFault::new(
        FaultCode::ContractMismatch,
        "the board's Lemnos is another build",
        "contract camera.sensor/1: a base method is laid out differently (the device's base hash 0x4d6f4b4bddf78636, this client's 0x33b860b02987233d)",
    );
    let (code, reason, detail) = step_fault(&fault);
    assert_eq!(FaultCode::ContractMismatch as u32, 12);
    assert_eq!(code, 12);
    assert_eq!(reason, "the board's Lemnos is another build");
    assert!(
        detail.contains("0x4d6f4b4bddf78636") && detail.contains("0x33b860b02987233d"),
        "{detail}"
    );
}
