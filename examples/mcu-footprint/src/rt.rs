//! What a Cortex-M firmware image adds around a configuration: a heap (embedded-alloc's
//! linked-list allocator over a static arena left out of `.bss`), a panic handler that stops
//! the core (no message formatting), and the idle loop the result is parked in.

use core::mem::MaybeUninit;

use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// The heap arena: 512 KiB, in cortex-m-rt's `.uninit` section (not zeroed at start-up, and
/// reported apart from `.bss` by the size tools; docs/mcu.md sizes it per configuration).
pub const HEAP_SIZE: usize = 512 * 1024;

#[unsafe(link_section = ".uninit.HEAP")]
static mut ARENA: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

/// Gives the allocator its arena. Call once, first.
pub fn init_heap() {
    // SAFETY: called once at start-up, before any allocation; the arena is used by nothing
    // else.
    unsafe { HEAP.init(&raw mut ARENA as usize, HEAP_SIZE) }
}

/// Heap bytes in use now.
pub fn heap_used() -> usize {
    HEAP.used()
}

/// Parks the core with `result` kept alive (so nothing the configuration computed is dead).
pub fn finish(result: u64) -> ! {
    core::hint::black_box(result);
    loop {
        cortex_m::asm::wfi();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::asm::udf()
}
