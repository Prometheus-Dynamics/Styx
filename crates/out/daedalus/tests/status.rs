//! What Styx shows of a Daedalus fault, and of a GPU that didn't open. No GPU is needed: the faults
//! come from CPU nodes, and the open error is built as Daedalus builds it.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::sync::Arc;
use std::time::Duration;

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_cpu_author::{Ctx, Fault};
use daedalus_wgpu::OpenError;
use styx_daedalus::{fault_status, fault_text, open_error_status_with};

#[node(id = "styx_test.fail_long", inputs(x), outputs(out))]
fn fail_long(x: i64) -> Result<i64, &'static str> {
    if x < 0 {
        return Ok(x);
    }
    Err(
        "a reason that goes on well past the hundred and twenty bytes a fault keeps, so that what the host reads is only its start",
    )
}

#[node(id = "styx_test.fail_short", inputs(x), outputs(out))]
fn fail_short(x: i64) -> Result<i64, &'static str> {
    if x < 0 {
        return Ok(x);
    }
    Err("a short reason")
}

/// A Styx node that says more than its reason holds: which value it was given and what it expected.
/// The detail is what the host reads beside the reason.
#[node(id = "styx_test.fail_detail", inputs(x), outputs(out))]
fn fail_detail(x: i64, ctx: &Ctx) -> Result<i64, Fault> {
    if x < 0 {
        return Ok(x);
    }
    ctx.detail_fmt(format_args!(
        "value {x} is not in the range 0..=0: the frame is 64x64 GREY, the planes are 4096 and 2048 bytes, and the fd is read-only\n"
    ));
    Err(Fault {
        code: 9,
        reason: "a plane is short",
    })
}

/// A Styx node whose detail is longer than the host keeps (16 KiB by default), so it is cut.
#[node(id = "styx_test.fail_detail_long", inputs(x), outputs(out))]
fn fail_detail_long(x: i64, ctx: &Ctx) -> Result<i64, Fault> {
    if x < 0 {
        return Ok(x);
    }
    let line = "a line of detail that the host keeps only up to its bound\n";
    for _ in 0..(40 << 10) / line.len() {
        ctx.detail(line);
    }
    Err(Fault {
        code: 10,
        reason: "too much to say",
    })
}

/// One node between the host's `x` and `out`, both `i64`.
fn graph_of(node: &str) -> String {
    format!(
        r#"{{"format": "daedalus.graph", "schema_version": 1, "graph": {{
  "nodes": [
    {{"id": "host", "kind": "host_bridge", "port_types": {{"x": "i64", "out": "i64"}}}},
    {{"id": "{node}"}}
  ],
  "edges": [
    {{"from": {{"node": 0, "port": "x"}}, "to": {{"node": 1, "port": "x"}}}},
    {{"from": {{"node": 1, "port": "out"}}, "to": {{"node": 0, "port": "out"}}}}
  ]
}}}}"#
    )
}

/// Runs the one-node graph with a value that makes the node fault. Gives the fault's reason, its
/// cut flag, and the host's detail for it, as Styx would show them.
/// The fault's code (the number and whether it is Daedalus's own), reason, reason cut flag, and the
/// detail with its cut flag.
type Coded = (u32, bool, String, bool, Option<(String, bool)>);

fn fault_of(node: &'static NodeRecord, id: &str) -> Coded {
    let mut registry = Registry::new();
    registry.register_all(&[node]).expect("the node registers");
    let graph = GraphDoc::parse(&graph_of(id))
        .expect("the graph parses")
        .graph;
    let planned = plan(&graph, &registry, &[&CpuBackend], &Config::default()).expect("it plans");
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("it loads");
    loaded
        .host()
        .feed_typed("x", Arc::new(1_i64))
        .expect("the value is fed");
    let _ = loaded.host().step_until_settled(Duration::from_secs(5));
    let reports: Vec<(u32, String, bool, u32, bool)> = loaded
        .fault_reports()
        .map(|report| {
            (
                report.step,
                report.reason().to_string(),
                report.reason_cut,
                report.code,
                report.own_code,
            )
        })
        .collect();
    assert_eq!(reports.len(), 1, "one fault: {reports:?}");
    let (step, reason, cut, code, own) = reports.into_iter().next().unwrap();
    let detail = loaded
        .fault_detail(step)
        .map(|detail| (detail.text.to_string(), detail.cut));
    (code, own, reason, cut, detail)
}

#[test]
fn a_fault_reason_longer_than_a_fault_keeps_is_shown_cut() {
    let (code, own, reason, cut, detail) = fault_of(&FAIL_LONG_NODE, "styx_test.fail_long");
    assert!(cut, "the host says the reason was cut");
    let line = fault_status(code, own, &reason, cut, None);
    assert!(
        line.starts_with("code 1: a reason that goes on well past"),
        "{line}"
    );
    // The reason kept is 120 bytes, and the flag adds " (cut)"; the code is "code 1: ".
    assert_eq!(
        line.len(),
        "code 1: ".len() + 120 + " (cut)".len(),
        "{line}"
    );
    assert!(detail.is_none(), "the node wrote no detail");
}

#[test]
fn a_fault_reason_that_fits_is_shown_whole() {
    let (code, own, reason, cut, _) = fault_of(&FAIL_SHORT_NODE, "styx_test.fail_short");
    assert!(!cut);
    assert_eq!(
        (code, own),
        (1, false),
        "the rev's code for a plain error string"
    );
    assert_eq!(
        fault_status(code, own, &reason, cut, None),
        "code 1: a short reason"
    );
}

#[test]
fn a_styx_nodes_detail_arrives_at_the_host_beside_its_reason() {
    let (code, own, reason, cut, detail) = fault_of(&FAIL_DETAIL_NODE, "styx_test.fail_detail");
    assert_eq!(reason, "a plane is short");
    assert_eq!(
        (code, own),
        (9, false),
        "the node's code 9 reaches the host"
    );
    assert!(!cut);
    let (text, detail_cut) = detail.expect("the node's detail reached the host");
    assert!(
        text.contains("the planes are 4096 and 2048 bytes"),
        "{text}"
    );
    assert!(!detail_cut, "the detail fits the host's bound");
    let line = fault_status(
        code,
        own,
        &reason,
        cut,
        Some(daedalus::layers::host::FaultDetail {
            text: &text,
            cut: detail_cut,
            tick: 0,
        }),
    );
    assert!(
        line.starts_with("code 9: a plane is short; detail: value 1 is not"),
        "{line}"
    );
    assert!(!line.contains('\n'), "{line:?}");
}

#[test]
fn a_detail_longer_than_the_host_keeps_is_shown_cut() {
    let (_code, _own, reason, cut, detail) =
        fault_of(&FAIL_DETAIL_LONG_NODE, "styx_test.fail_detail_long");
    assert!(!cut);
    let (text, detail_cut) = detail.expect("the detail reached the host");
    assert!(detail_cut, "the host says the detail was cut");
    assert!(
        text.len() <= 16 << 10,
        "kept at most the host's bound: {}",
        text.len()
    );
    let _ = fault_text(&reason, cut);
}

#[test]
fn the_cut_flag_is_shown_after_the_reason() {
    assert_eq!(fault_text("a reason", false), "a reason");
    assert_eq!(fault_text("a reason", true), "a reason (cut)");
}

#[test]
fn a_refused_gpu_node_is_named_with_the_group_that_owns_it() {
    // The error as Daedalus builds it when this user is refused the GPU's nodes.
    let error = OpenError::NoAccess("/dev/dri/card0, /dev/dri/card1".into());
    let text = open_error_status_with(&error, |node| {
        assert!(node.starts_with("/dev/dri/card"), "{node}");
        Some("video".to_string())
    });
    assert_eq!(
        text,
        "GPU not usable: user lacks group video for /dev/dri/card0, /dev/dri/card1"
    );
}

#[test]
fn a_refused_gpu_node_with_no_known_group_says_so() {
    let error = OpenError::NoAccess("/dev/dri/renderD128".into());
    let text = open_error_status_with(&error, |_| None);
    assert_eq!(
        text,
        "GPU not usable: user lacks group (unknown) for /dev/dri/renderD128"
    );
}

#[test]
fn another_open_error_keeps_its_own_text() {
    let text = open_error_status_with(&OpenError::NoAdapter, |_| None);
    assert_eq!(text, "GPU not usable: no adapter matches");
}
