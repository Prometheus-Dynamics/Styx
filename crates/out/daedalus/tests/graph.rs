//! A graph of `styx:frame` values: a producer in the dma-buf location, a consumer on a device
//! that is not the producer's backend, and the planner's choice of moves between them.
//!
//! No GPU is used here. The device is a test backend: it runs the consumer's CPU body in its own
//! memory space, and it lists the dma-buf import a GPU would offer (a handoff of the same value,
//! so the graph runs without a device). The planner's choice of moves is the thing under test.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

mod support;

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, OnceLock};

use daedalus::layers::graph::{
    EdgeDoc, EdgePolicyDoc, GraphBody, NodeDoc, NodeKindDoc, PortRefDoc,
};
use daedalus::layers::planner::{Config, plan};
use daedalus::layers::registry::{NodeRecord, Registry};
use daedalus::layers::types::Typed;
use daedalus::prelude::*;
use daedalus_abi::{OwnedStep, Slots, Step, StepCx, StepEnd};
use daedalus_backend::{
    Backend, CompileCx, Compiled, MemorySpace, NodeRef, Offer, Refusal, Region, SpaceKind,
    Transfer, TransferCost,
};
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::{DmaBufMemory, to_host_id};
use daedalus_runner::{UNSTAMPED, put_typed, share_typed};
use styx_daedalus::{StyxFrame, dmabuf_memory};
use styx_frame::FramePool;

const DEVICE: &str = "test.device";
/// The backend id of the device, and of the consumer's body under it.
const DEVICE_BACKEND: &str = "test.device";
const IMPORT_ID: &str = "test.import:styx:frame";
const COPY_ID: &str = "test.copy:u64";

/// The pool the producer takes its buffers from. Set by the test that runs the graph.
static POOL: OnceLock<Mutex<Option<FramePool<()>>>> = OnceLock::new();

fn pool_slot() -> &'static Mutex<Option<FramePool<()>>> {
    POOL.get_or_init(|| Mutex::new(None))
}

/// Writes `seq` into a buffer of the pool and gives the frame, in the dma-buf location.
#[node(
    id = "styx_test.frame_make",
    inputs(seq),
    outputs(frame),
    memory(frame = "dmabuf")
)]
fn frame_make(seq: u32) -> StyxFrame {
    let guard = pool_slot().lock().unwrap();
    let pool = guard.as_ref().expect("a pool");
    let mut lease = pool
        .acquire(support::entry(u64::from(seq)))
        .expect("a free buffer");
    lease.write().expect("sole owner").bytes().fill(seq as u8);
    StyxFrame::from_lease(lease, pool.buffer_size()).expect("an allocation")
}

/// The consumer: the frame's id.
#[node(id = "styx_test.frame_check", inputs(frame), outputs(id))]
fn frame_check(frame: &StyxFrame) -> u64 {
    frame.id()
}

const FRAME_MAKE: &NodeRecord = &FRAME_MAKE_NODE;
const FRAME_CHECK: &NodeRecord = &FRAME_CHECK_NODE;

/// Passes the input value on, as the same value (a handoff, not a copy of its contents).
struct Handoff<T> {
    of: PhantomData<fn() -> T>,
}

impl<T: Typed + Send + Sync + 'static> OwnedStep for Handoff<T> {
    fn run(&mut self, slots: &mut Slots, cx: &mut StepCx) -> StepEnd {
        let fault = |cx: &mut StepCx, why: &str| {
            cx.fault(None, why);
            StepEnd::Fault
        };
        let Some(input) = slots.cell(0) else {
            return fault(cx, "a move has no input");
        };
        let Some(value) = share_typed(input).and_then(|shared| shared.downcast::<T>().ok()) else {
            return fault(cx, "a move was given a value of another type");
        };
        let Some(out) = slots.cell(1) else {
            return fault(cx, "a move has no output");
        };
        put_typed(out, UNSTAMPED.0, UNSTAMPED.1, value);
        StepEnd::Done
    }
}

fn make_frame_handoff(_: &CompileCx<'_>) -> Result<Step, Refusal> {
    Ok(Step::boxed(Handoff::<StyxFrame> { of: PhantomData }))
}

fn make_u64_handoff(_: &CompileCx<'_>) -> Result<Step, Refusal> {
    Ok(Step::boxed(Handoff::<u64> { of: PhantomData }))
}

/// A device that runs the nodes with a `frame` input. Its memory is its own, and a value in any
/// other space has to be moved into it. With `import`, it lists the dma-buf import a GPU offers.
struct Device {
    import: bool,
}

impl Backend for Device {
    fn id(&self) -> &str {
        DEVICE_BACKEND
    }

    fn memory(&self) -> Vec<MemorySpace> {
        vec![MemorySpace {
            id: Cow::Borrowed(DEVICE),
            kind: SpaceKind::Device,
            // The device reads its own memory; it can't read the host's or the dma-buf's.
            readable: true,
        }]
    }

    fn transfers(&self) -> Vec<Transfer> {
        let mut moves = vec![Transfer {
            id: COPY_ID,
            key: Some(<u64 as Typed>::KEY),
            from: Cow::Borrowed(DEVICE),
            to: Cow::Borrowed("cpu.host"),
            cost: TransferCost::Copy,
            make: make_u64_handoff,
        }];
        if self.import {
            moves.push(Transfer {
                id: IMPORT_ID,
                key: Some(StyxFrame::KEY),
                from: Cow::Borrowed(daedalus_dmabuf::SPACE),
                to: Cow::Borrowed(DEVICE),
                cost: TransferCost::Import,
                make: make_frame_handoff,
            });
        }
        moves
    }

    fn can_take(&self, offer: &Offer<'_>) -> Vec<NodeRef> {
        offer
            .nodes
            .iter()
            .filter(|node| node.inputs.iter().any(|port| port.name == "frame"))
            .map(|node| NodeRef {
                node: node.node,
                body: node.body,
            })
            .collect()
    }

    fn compile(&self, region: &Region<'_>, cx: &CompileCx<'_>) -> Result<Compiled, Refusal> {
        CpuBackend.compile(region, cx)
    }
}

fn edge(from: u32, from_port: &str, to: u32, to_port: &str) -> EdgeDoc {
    EdgeDoc {
        from: PortRefDoc {
            node: from,
            port: from_port.to_string(),
        },
        to: PortRefDoc {
            node: to,
            port: to_port.to_string(),
        },
        policy: EdgePolicyDoc::default(),
    }
}

/// `host.seq -> frame_make (dmabuf) -> frame_check (device) -> host.id`.
fn graph() -> GraphBody {
    let mut bridge = NodeDoc::new("host");
    bridge.kind = NodeKindDoc::HostBridge;
    bridge
        .port_types
        .insert("seq".to_string(), "u32".to_string());
    bridge
        .port_types
        .insert("id".to_string(), "u64".to_string());
    GraphBody {
        nodes: vec![
            bridge,
            NodeDoc::new("styx_test.frame_make"),
            NodeDoc::new("styx_test.frame_check"),
        ],
        edges: vec![
            edge(0, "seq", 1, "seq"),
            edge(1, "frame", 2, "frame"),
            edge(2, "id", 0, "id"),
        ],
        groups: Vec::new(),
    }
}

/// The consumer's record, with its body under the device's backend. The body is the CPU body the
/// macro wrote: the device runs it in its own memory, and the registry offers it to the device
/// only. Node bodies of a GPU backend would be written with its own authoring crate instead.
fn device_consumer() -> NodeRecord {
    let mut record = FRAME_CHECK.clone();
    record.body.backend = DEVICE_BACKEND;
    record
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register_all(&[FRAME_MAKE]).unwrap();
    registry
        .register(Box::leak(Box::new(device_consumer())))
        .unwrap();
    registry
}

/// Planning with merging off, so each node is its own step and the frame crosses from its
/// location to the device's.
fn plan_with(
    device: &Device,
) -> Result<daedalus::layers::planner::Planned, daedalus::layers::planner::PlanError> {
    let registry = registry();
    let dma: DmaBufMemory = dmabuf_memory();
    // The device comes first: it takes the consumer. The CPU takes the producer.
    let backends: [&dyn Backend; 3] = [device, &CpuBackend, &dma];
    let config = Config {
        merge: false,
        ..Config::default()
    };
    plan(&graph(), &registry, &backends, &config)
}

#[test]
fn the_planner_inserts_the_device_import_when_one_is_listed() {
    let planned = plan_with(&Device { import: true }).expect("the graph plans");
    let moves: Vec<&str> = planned
        .stitched
        .transfers
        .iter()
        .map(|used| used.id.as_str())
        .collect();
    assert!(moves.contains(&IMPORT_ID), "moves: {moves:?}");
    assert!(
        !moves.contains(&to_host_id::<StyxFrame>()),
        "the device reads it, not the host"
    );
}

#[test]
fn the_planner_refuses_the_frame_when_no_move_reaches_the_device() {
    let err = plan_with(&Device { import: false }).expect_err("no plan");
    let text = err.to_string();
    println!("refusal: {text}");
    assert!(text.contains("frame"), "{text}");
    assert!(text.contains("dmabuf"), "{text}");
    assert!(text.contains(DEVICE), "{text}");
}

#[test]
fn a_producer_and_a_consumer_of_styx_frame_run_and_the_buffers_come_back() {
    let Some(pool) = support::dma_pool(3) else {
        return;
    };
    *pool_slot().lock().unwrap() = Some(pool);

    let planned = plan_with(&Device { import: true }).expect("the graph plans");
    let mut host = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    for seq in 0..5u32 {
        let io = host.host();
        io.feed_typed("seq", Arc::new(seq)).unwrap();
        io.step().unwrap();
        assert_eq!(io.take_copy::<u64>("id").unwrap(), Some(u64::from(seq)));
    }
    drop(host);

    let guard = pool_slot().lock().unwrap();
    let stats = guard.as_ref().unwrap().stats();
    assert_eq!(
        stats.in_use, 0,
        "every frame's buffer went back to the pool: {stats:?}"
    );
}
