//! The smoke image. On a host it prints what [`styx_nostd_smoke::run`] found; on a bare-metal
//! target it is a `no_std`, `no_main` image whose entry runs the same code and parks the core.
//! Its sizes are what `docs/mcu.md` measures for the frame crates: `llvm-size -A` on the
//! release build.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    match styx_nostd_smoke::run() {
        Ok(report) => println!("{report:?}"),
        Err(error) => {
            eprintln!("styx-nostd-smoke: {error:?}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "none")]
mod firmware {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::mem::MaybeUninit;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// The heap arena: a frame pool of two NV12 QQVGA buffers, with slack, needs well under this.
    const ARENA_LEN: usize = 64 * 1024;

    /// A bump allocator over one static arena. Nothing is freed: the pool is made once, at
    /// start-up, and the smoke run allocates nothing after that.
    struct Bump {
        next: AtomicUsize,
        arena: UnsafeCell<[MaybeUninit<u8>; ARENA_LEN]>,
    }

    // SAFETY: the arena is only handed out through `alloc`, which moves `next` with a
    // compare-and-swap, so two callers never receive the same bytes.
    unsafe impl Sync for Bump {}

    // SAFETY: `alloc` returns a block inside the arena that no earlier call returned (the offset
    // only grows), aligned as `layout` asks, or null when it does not fit. `dealloc` frees
    // nothing, which is always sound.
    unsafe impl GlobalAlloc for Bump {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let base = self.arena.get() as usize;
            let mut current = self.next.load(Ordering::Relaxed);
            loop {
                let start = (base + current).next_multiple_of(layout.align()) - base;
                let Some(end) = start.checked_add(layout.size()) else {
                    return core::ptr::null_mut();
                };
                if end > ARENA_LEN {
                    return core::ptr::null_mut();
                }
                match self.next.compare_exchange_weak(
                    current,
                    end,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return (base + start) as *mut u8,
                    Err(seen) => current = seen,
                }
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }

    /// In `.uninit`, not zeroed at start-up and reported apart from `.bss` (docs/mcu.md's heap).
    #[global_allocator]
    #[unsafe(link_section = ".uninit.HEAP")]
    static HEAP: Bump = Bump {
        next: AtomicUsize::new(0),
        arena: UnsafeCell::new([MaybeUninit::uninit(); ARENA_LEN]),
    };

    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    /// The entry point of the linked image. Runs the smoke test and parks the core with the
    /// result kept (so the code is not folded away).
    #[unsafe(no_mangle)]
    pub extern "C" fn _start() -> ! {
        // The result is discarded, but the call stays: black_box keeps it from being folded away.
        let _ = core::hint::black_box(styx_nostd_smoke::run());
        loop {
            core::hint::spin_loop();
        }
    }
}
