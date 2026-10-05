//! The exported atomics behave as core's, on whichever source the build picked (core's
//! instructions, `portable-atomic`'s fallback, or critical sections: `check-nostd.sh` runs these
//! without `std` with and without `critical-section`).

use super::*;

#[test]
fn integer_atomics_read_modify_write() {
    let o = Ordering::SeqCst;
    let i8_ = AtomicI8::new(-1);
    assert_eq!(i8_.fetch_add(2, o), -1);
    assert_eq!(i8_.fetch_sub(3, o), 1);
    assert_eq!(i8_.load(o), -2);
    let i16_ = AtomicI16::new(i16::MAX);
    assert_eq!(i16_.fetch_add(1, o), i16::MAX);
    assert_eq!(i16_.load(o), i16::MIN, "wraps as core's");
    let i32_ = AtomicI32::new(5);
    assert_eq!(i32_.fetch_max(9, o), 5);
    assert_eq!(i32_.fetch_min(-4, o), 9);
    assert_eq!(i32_.load(o), -4);
    let i64_ = AtomicI64::new(1 << 40);
    assert_eq!(
        i64_.compare_exchange(1 << 40, -(1 << 41), o, o),
        Ok(1 << 40)
    );
    assert_eq!(i64_.compare_exchange(0, 1, o, o), Err(-(1 << 41)));
    let isize_ = AtomicIsize::new(-7);
    assert_eq!(isize_.swap(7, o), -7);
    assert_eq!(isize_.fetch_update(o, o, |v| Some(v * 2)), Ok(7));
    assert_eq!(isize_.into_inner(), 14);
    let u8_ = AtomicU8::new(0b1010);
    assert_eq!(u8_.fetch_or(0b0101, o), 0b1010);
    assert_eq!(u8_.fetch_and(0b0011, o), 0b1111);
    assert_eq!(u8_.fetch_xor(0b0001, o), 0b0011);
    assert_eq!(u8_.load(o), 0b0010);
    let u16_ = AtomicU16::new(u16::MAX);
    assert_eq!(u16_.fetch_add(2, o), u16::MAX);
    assert_eq!(u16_.load(o), 1);
    let u32_ = AtomicU32::new(3);
    u32_.store(4, o);
    assert_eq!(u32_.fetch_sub(5, o), 4);
    assert_eq!(u32_.load(o), u32::MAX);
    let u64_ = AtomicU64::new(u64::MAX - 1);
    assert_eq!(u64_.fetch_add(1, o), u64::MAX - 1);
    assert_eq!(u64_.load(o), u64::MAX);
    let usize_ = AtomicUsize::new(0);
    assert_eq!(usize_.fetch_add(3, o), 0);
    assert_eq!(usize_.load(o), 3);
    let flag = AtomicBool::new(false);
    assert!(!flag.fetch_xor(true, o));
    assert!(flag.fetch_and(false, o));
    assert!(!flag.load(o));
}

#[test]
fn pointer_atomics_and_fences() {
    let mut a = 1u32;
    let mut b = 2u32;
    let (pa, pb) = (&mut a as *mut u32, &mut b as *mut u32);
    let ptr = AtomicPtr::new(pa);
    assert_eq!(ptr.swap(pb, Ordering::AcqRel), pa);
    assert_eq!(
        ptr.compare_exchange(pb, pa, Ordering::AcqRel, Ordering::Acquire),
        Ok(pb)
    );
    fence(Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    // SAFETY: `pa` points to `a`, alive and not otherwise borrowed.
    assert_eq!(unsafe { *ptr.load(Ordering::Acquire) }, 1);
}

#[test]
fn arc_and_weak() {
    let arc = Arc::new(5u32);
    let weak = Arc::downgrade(&arc);
    assert_eq!(weak.upgrade().as_deref(), Some(&5));
    assert_eq!(Arc::strong_count(&arc), 1);
    drop(arc);
    assert!(weak.upgrade().is_none());
}
