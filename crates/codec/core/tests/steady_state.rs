//! The per-frame call allocates nothing: a converter over pool leases, run in a loop, under a
//! counting allocator. This file holds one test, so no other test's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use styx_core::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Converter, FourCc,
    FrameLease, Implementation, Registry, Request,
};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, packed_layouts};

struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call to the system allocator unchanged; only adds a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::SeqCst) {
            ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: the caller upholds GlobalAlloc::alloc's contract; System is the allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller passes back a pointer and layout System handed out.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::SeqCst) {
            ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: as dealloc; System reallocates the block the caller owns.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Copies the input's bytes into the output and sets the output's entry to the input's id.
struct GreyCopy {
    descriptor: CodecDescriptor,
}

impl Codec for GreyCopy {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl Converter for GreyCopy {
    fn convert(&mut self, input: &FrameLease, output: &mut FrameLease) -> Result<(), CodecError> {
        let src = input
            .read()
            .ok_or_else(|| CodecError::Rejected("no read".into()))?;
        let mut dst = output
            .write()
            .ok_or_else(|| CodecError::Failed("no write".into()))?;
        let n = src.bytes().len().min(dst.bytes().len());
        dst.bytes()[..n].copy_from_slice(&src.bytes()[..n]);
        Ok(())
    }
}

fn grey_entry(id: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 16, 16, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(16, 16).unwrap());
    FrameEntry::new(id, id, format, layouts)
}

#[test]
fn a_converter_call_over_pool_leases_allocates_nothing() {
    let descriptor = CodecDescriptor {
        kind: CodecKind::Converter,
        input: FourCc::GREY,
        output: FourCc::GREY,
        backend: Backend::Native,
        name: "grey-copy",
    };
    let mut registry = Registry::new();
    let built = descriptor.clone();
    registry.register(Candidate::new(descriptor, move || {
        Ok(Implementation::Converter(Box::new(GreyCopy {
            descriptor: built.clone(),
        })))
    }));
    let selection = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::GREY,
            FourCc::GREY,
        ))
        .unwrap();
    let Implementation::Converter(mut codec) = selection.into_implementation() else {
        panic!("expected a converter");
    };

    // Setup: pools, leases and the first call run before counting starts.
    let pool: FramePool = FramePool::host(4, BufferSize::new(256, 0)).unwrap();
    let input = pool.acquire(grey_entry(1)).unwrap();
    let mut output = pool.acquire(grey_entry(2)).unwrap();
    codec.convert(&input, &mut output).unwrap();

    const FRAMES: usize = 1000;
    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    for _ in 0..FRAMES {
        codec.convert(&input, &mut output).unwrap();
    }
    COUNTING.store(false, Ordering::SeqCst);

    assert_eq!(
        ALLOCATIONS.load(Ordering::SeqCst),
        0,
        "per-frame calls allocated"
    );
    assert_eq!(pool.stats().misses, 0);
}
