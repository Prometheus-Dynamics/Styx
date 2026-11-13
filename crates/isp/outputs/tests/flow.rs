//! The proof: two consumers at two sizes from one capture through the soft ISP, each receiving
//! its size, with no copy; a third consumer at one of those sizes shares the lease; after warm-up
//! a frame allocates nothing. A conversion fallback is checked the same way.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use styx_format::FourCc;
use styx_frame::copy_counters;
use styx_outputs::{Ask, Delivery, Distributor, Route, plan};
use styx_settings::IspSettings;
use styx_soft::{Options, SoftIspStage};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; it only counts this thread's allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's contract for `alloc` is passed on as it is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const WARM_UP: u64 = 8;
const FRAMES: u64 = 300;

fn copies() -> Vec<usize> {
    copy_counters().iter().map(|c| c.copies()).collect()
}

fn stage() -> SoftIspStage {
    let mut settings = IspSettings::neutral(64.0 / 1023.0);
    settings.wb = [1.9, 1.0, 1.6];
    SoftIspStage::new(RAW, W, H, &settings, Options::default()).unwrap()
}

#[test]
fn two_sizes_from_one_capture_each_get_their_size_with_no_copy_and_a_third_shares() {
    let asks = [
        Ask::new(FourCc::RGB3, res(64, 48)),
        Ask::new(FourCc::RGB3, res(32, 24)),
        Ask::new(FourCc::RGB3, res(64, 48)),
    ];
    let plan = plan(&Soft, capture(), &asks).unwrap();
    let mut settings = IspSettings::neutral(0.0);
    plan.apply(&mut settings);
    assert_eq!(
        settings.outputs.len(),
        2,
        "IspSettings::outputs carries both outputs"
    );

    let mut distributor: Distributor<()> = Distributor::new(plan, 4).unwrap();
    let mut stage = stage();
    let raw = raw_lease(1);
    let copies_before = copies();

    let mut before = 0;
    for n in 0..FRAMES {
        if n == WARM_UP {
            before = allocations();
        }
        let delivery = distributor
            .deliver(&raw, |_, input, out| stage.process(input, out, None))
            .unwrap();
        assert_eq!(
            delivery,
            Delivery {
                delivered: 3,
                dropped: 0
            }
        );
        let full = distributor.take(0).unwrap();
        let half = distributor.take(1).unwrap();
        let shared = distributor.take(2).unwrap();
        if n == 0 {
            assert_eq!(full.format().resolution, res(64, 48));
            assert_eq!(half.format().resolution, res(32, 24));
            assert_eq!(full.format().code, FourCc::RGB3);
            assert_eq!(half.format().code, FourCc::RGB3);
            let at =
                |l: &styx_frame::FrameLease<()>| l.memory().cpu_ptr().unwrap().as_ptr() as usize;
            assert_ne!(at(&full), at(&half), "two outputs, two buffers");
            assert_eq!(
                at(&full),
                at(&shared),
                "the third consumer shares the first's lease"
            );
            assert!(
                full.holders() >= 2,
                "one frame, shared between two consumers"
            );
            assert_eq!(full.id(), 1);
        }
        drop((full, half, shared));
    }
    let allocated = allocations() - before;
    assert_eq!(
        allocated,
        0,
        "allocations over {} warm frames",
        FRAMES - WARM_UP
    );
    assert_eq!(copies_before, copies(), "whole-frame copies");
    eprintln!(
        "proof: {FRAMES} frames, {} warm frames allocated {allocated} times, copies before {copies_before:?} after {:?}",
        FRAMES - WARM_UP,
        copies()
    );
    assert_eq!(
        distributor.plan().route(2),
        Some(Route::Isp {
            output: 0,
            shared: true
        })
    );
}

#[test]
fn a_converted_consumer_gets_the_isp_luma_in_its_format_with_no_allocation() {
    let asks = [
        Ask::new(FourCc::NV12, res(64, 48)),
        Ask::new(FourCc::YU12, res(64, 48)),
    ];
    let plan = plan(&Soft, capture(), &asks).unwrap();
    let mut distributor: Distributor<()> = Distributor::new(plan, 4).unwrap();
    let mut stage = stage();
    let raw = raw_lease(2);

    let mut before = 0;
    for n in 0..FRAMES {
        if n == WARM_UP {
            before = allocations();
        }
        let delivery = distributor
            .deliver(&raw, |_, input, out| stage.process(input, out, None))
            .unwrap();
        assert_eq!(delivery.delivered, 2);
        let direct = distributor.take(0).unwrap();
        let converted = distributor.take(1).unwrap();
        if n == 0 {
            assert_eq!(converted.format().code, FourCc::YU12);
            assert_eq!(converted.format().resolution, res(64, 48));
            let luma = (W * H) as usize;
            let a = direct.read().unwrap();
            let b = converted.read().unwrap();
            assert_eq!(
                &a.plane(0).unwrap().data[..luma],
                &b.plane(0).unwrap().data[..luma],
                "the converter copies luma exactly"
            );
        }
        drop((direct, converted));
    }
    assert_eq!(
        allocations() - before,
        0,
        "allocations over the warm converted frames"
    );
}
