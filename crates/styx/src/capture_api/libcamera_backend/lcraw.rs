//! libcamera control lists read and written in place, through libcamera's C API, without the
//! per-value `SmallVec`s of `libcamera::control_value::ControlValue`: those spill to the heap
//! for every array (`ColourGains`, `ColourCorrectionMatrix`, `SensorBlackLevels`,
//! `FrameDurationLimits`, the IPA's statistics), which made each completed request allocate
//! several times just to look at its metadata.
//!
//! [`RawValue`] borrows a value's elements from the list; [`record_metadata`] turns a request's
//! metadata into Styx control readback and the frame's timing without allocating once the
//! readback map holds every id (a multi-rectangle value allocates only when it changes).

use std::collections::HashMap;
use std::ptr::NonNull;

use libcamera::control::ControlList;
use libcamera_sys::libcamera_control_type::*;
use libcamera_sys::*;
use smallvec::SmallVec;
use styx_core::controls::{ControlId, ControlRect, ControlValue};

/// libcamera's `SensorTimestamp` and `FrameDuration` control ids.
const SENSOR_TIMESTAMP: u32 = libcamera::controls::ControlId::SensorTimestamp as u32;
const FRAME_DURATION: u32 = libcamera::controls::ControlId::FrameDuration as u32;

/// A control value's elements, borrowed from the list holding it.
#[derive(Clone, Copy, Debug)]
pub(super) enum RawValue<'a> {
    None,
    Bool(&'a [bool]),
    Byte(&'a [u8]),
    Uint16(&'a [u16]),
    Uint32(&'a [u32]),
    Int32(&'a [i32]),
    Int64(&'a [i64]),
    Float(&'a [f32]),
    Rectangle(&'a [libcamera_rectangle_t]),
    /// Strings, sizes, points: never read back.
    Other,
}

/// Timing from a completed request's metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RequestTiming {
    /// `SensorTimestamp`: start of exposure of the first line, in nanoseconds. libcamera
    /// documents `CLOCK_BOOTTIME`; on V4L2 pipelines it is the receiver's buffer timestamp
    /// (`CLOCK_MONOTONIC`), see `sensor_clock`.
    pub sensor_timestamp: Option<u64>,
    /// `FrameDuration` in nanoseconds.
    pub frame_duration_ns: Option<u64>,
}

fn rect(r: &libcamera_rectangle_t) -> ControlRect {
    ControlRect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    }
}

/// The Styx value of a libcamera value, as control readback reports it (`None`: not one Styx
/// reads back). Scalars, and an `Int64` pair with equal ends, become scalars; rectangles
/// become `Rect` / `Rects`. Only `Rects` allocates.
pub(super) fn styx_value(value: RawValue<'_>) -> Option<ControlValue> {
    let one = |len: usize| len == 1;
    Some(match value {
        RawValue::None => ControlValue::None,
        RawValue::Bool(v) if one(v.len()) => ControlValue::Bool(v[0]),
        RawValue::Int32(v) if one(v.len()) => ControlValue::Int(v[0]),
        RawValue::Int64(v) if one(v.len()) => ControlValue::Int(i32::try_from(v[0]).ok()?),
        RawValue::Int64([a, b]) if a == b => ControlValue::Int(i32::try_from(*a).ok()?),
        RawValue::Uint16(v) if one(v.len()) => ControlValue::Uint(u32::from(v[0])),
        RawValue::Uint32(v) if one(v.len()) => ControlValue::Uint(v[0]),
        RawValue::Float(v) if one(v.len()) => ControlValue::Float(v[0]),
        RawValue::Rectangle(v) if one(v.len()) => ControlValue::Rect(rect(&v[0])),
        RawValue::Rectangle(v) if !v.is_empty() => {
            ControlValue::Rects(v.iter().map(rect).collect())
        }
        _ => return None,
    })
}

/// Store `value` as `slot`'s readback, allocating only for a list of rectangles that changed.
fn store(readback: &mut HashMap<ControlId, ControlValue>, id: u32, value: RawValue<'_>) {
    let key = ControlId(id);
    if let (Some(ControlValue::Rects(have)), RawValue::Rectangle(new)) =
        (readback.get_mut(&key), value)
        && new.len() > 1
    {
        if have.len() == new.len() && have.iter().zip(new).all(|(a, b)| *a == rect(b)) {
            return;
        }
        have.clear();
        have.extend(new.iter().map(rect));
        return;
    }
    let Some(v) = styx_value(value) else {
        return;
    };
    match readback.get_mut(&key) {
        Some(slot) => *slot = v,
        None => {
            readback.insert(key, v);
        }
    }
}

/// Record a completed request's metadata (`entries`: id and value) as control readback and
/// pick out its timing. Allocates nothing once `readback` holds every id it reports (beyond
/// a changed list of rectangles).
pub(super) fn record_metadata<'a>(
    entries: impl IntoIterator<Item = (u32, RawValue<'a>)>,
    readback: &mut HashMap<ControlId, ControlValue>,
) -> RequestTiming {
    let mut timing = RequestTiming::default();
    for (id, value) in entries {
        if let RawValue::Int64(v) = value {
            let first = v.first().and_then(|v| u64::try_from(*v).ok());
            if id == SENSOR_TIMESTAMP {
                timing.sensor_timestamp = first;
            } else if id == FRAME_DURATION {
                timing.frame_duration_ns = first.map(|us| us * 1_000);
            }
        }
        store(readback, id, value);
    }
    timing
}

fn list_ptr(list: &ControlList) -> *mut libcamera_control_list_t {
    // `ControlList` is `#[repr(transparent)]` over `libcamera_control_list_t`.
    std::ptr::from_ref(list).cast_mut().cast()
}

/// The entries of a control list, each value borrowed from the list.
pub(super) struct Entries<'a> {
    it: NonNull<libcamera_control_list_iter_t>,
    _list: std::marker::PhantomData<&'a ControlList>,
}

/// Iterate `list` without converting its values.
pub(super) fn entries(list: &ControlList) -> Entries<'_> {
    // SAFETY: `list` is a live control list; the iterator is destroyed on drop, before the
    // borrow of `list` ends.
    let it = unsafe { libcamera_control_list_iter(list_ptr(list)) };
    Entries {
        it: NonNull::new(it).expect("libcamera control list iterator"),
        _list: std::marker::PhantomData,
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, RawValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let it = self.it.as_ptr();
        // SAFETY: `it` is a live iterator over a list borrowed for `'a`; the value pointer it
        // yields stays valid (and unchanged) while the list is borrowed.
        unsafe {
            if libcamera_control_list_iter_end(it) {
                return None;
            }
            let id = libcamera_control_list_iter_id(it);
            let value = raw_value(libcamera_control_list_iter_value(it));
            libcamera_control_list_iter_next(it);
            Some((id, value))
        }
    }
}

impl Drop for Entries<'_> {
    fn drop(&mut self) {
        // SAFETY: created by `libcamera_control_list_iter`, destroyed once.
        unsafe { libcamera_control_list_iter_destroy(self.it.as_ptr()) }
    }
}

/// # Safety
/// `val` is null or a live control value that outlives `'a` unchanged.
unsafe fn raw_value<'a>(val: *const libcamera_control_value_t) -> RawValue<'a> {
    if val.is_null() {
        return RawValue::Other;
    }
    // SAFETY: per the contract; libcamera stores `num_elements` elements of the value's type
    // contiguously at `data`.
    unsafe {
        let ty = libcamera_control_value_type(val);
        let n = libcamera_control_value_num_elements(val);
        let data = libcamera_control_value_get(val);
        fn slice<'a, T>(data: *const core::ffi::c_void, n: usize) -> &'a [T] {
            if data.is_null() || n == 0 {
                &[]
            } else {
                // SAFETY: see `raw_value`.
                unsafe { std::slice::from_raw_parts(data.cast(), n) }
            }
        }
        match ty {
            LIBCAMERA_CONTROL_TYPE_NONE => RawValue::None,
            LIBCAMERA_CONTROL_TYPE_BOOL => RawValue::Bool(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_BYTE => RawValue::Byte(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_UINT16 => RawValue::Uint16(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_UINT32 => RawValue::Uint32(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_INT32 => RawValue::Int32(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_INT64 => RawValue::Int64(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_FLOAT => RawValue::Float(slice(data, n)),
            LIBCAMERA_CONTROL_TYPE_RECTANGLE => RawValue::Rectangle(slice(data, n)),
            _ => RawValue::Other,
        }
    }
}

/// Set control `id` of `list` to `value`'s elements (an array unless exactly one element),
/// as `ControlList::set_raw` does, without building a `ControlValue`.
pub(super) fn set(list: &mut ControlList, id: u32, value: RawValue<'_>) {
    let (ty, data, len): (u32, *const core::ffi::c_void, usize) = match value {
        RawValue::None => (LIBCAMERA_CONTROL_TYPE_NONE, std::ptr::null(), 0),
        RawValue::Bool(v) => (LIBCAMERA_CONTROL_TYPE_BOOL, v.as_ptr().cast(), v.len()),
        RawValue::Byte(v) => (LIBCAMERA_CONTROL_TYPE_BYTE, v.as_ptr().cast(), v.len()),
        RawValue::Uint16(v) => (LIBCAMERA_CONTROL_TYPE_UINT16, v.as_ptr().cast(), v.len()),
        RawValue::Uint32(v) => (LIBCAMERA_CONTROL_TYPE_UINT32, v.as_ptr().cast(), v.len()),
        RawValue::Int32(v) => (LIBCAMERA_CONTROL_TYPE_INT32, v.as_ptr().cast(), v.len()),
        RawValue::Int64(v) => (LIBCAMERA_CONTROL_TYPE_INT64, v.as_ptr().cast(), v.len()),
        RawValue::Float(v) => (LIBCAMERA_CONTROL_TYPE_FLOAT, v.as_ptr().cast(), v.len()),
        RawValue::Rectangle(v) => (LIBCAMERA_CONTROL_TYPE_RECTANGLE, v.as_ptr().cast(), v.len()),
        RawValue::Other => return,
    };
    // SAFETY: the value is created, filled from `len` elements of type `ty` at `data` (copied
    // by libcamera), stored into the live `list` (copied again) and destroyed.
    unsafe {
        let val = libcamera_control_value_create();
        if val.is_null() {
            return;
        }
        libcamera_control_value_set(val, ty, data, len != 1, len as _);
        libcamera_control_list_set(list_ptr(list), id as _, val);
        libcamera_control_value_destroy(val);
    }
}

/// Set control `id` of `list` to a Styx control value, without allocating (up to four
/// rectangles).
pub(super) fn set_styx(list: &mut ControlList, id: u32, value: &ControlValue) {
    let lc = |r: &ControlRect| libcamera_rectangle_t {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    };
    match value {
        ControlValue::None => set(list, id, RawValue::None),
        ControlValue::Bool(v) => set(list, id, RawValue::Bool(&[*v])),
        ControlValue::Int(v) => set(list, id, RawValue::Int32(&[*v])),
        ControlValue::Uint(v) => set(list, id, RawValue::Uint32(&[*v])),
        ControlValue::Float(v) => set(list, id, RawValue::Float(&[*v])),
        ControlValue::Rect(r) => set(list, id, RawValue::Rectangle(&[lc(r)])),
        ControlValue::Rects(rects) => {
            let rects: SmallVec<[libcamera_rectangle_t; 4]> = rects.iter().map(lc).collect();
            set(list, id, RawValue::Rectangle(&rects));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::allocations;

    fn lc_rect(x: i32) -> libcamera_rectangle_t {
        libcamera_rectangle_t {
            x,
            y: 2,
            width: 64,
            height: 48,
        }
    }

    /// What libcamera's Raspberry Pi pipeline reports with every frame, as raw values.
    fn metadata<'a>(ts: &'a [i64], rects: &'a [libcamera_rectangle_t]) -> [(u32, RawValue<'a>); 9] {
        [
            (SENSOR_TIMESTAMP, RawValue::Int64(ts)),
            (FRAME_DURATION, RawValue::Int64(&[33_333])),
            (1, RawValue::Int32(&[10_000])),
            (2, RawValue::Float(&[2.0])),
            (3, RawValue::Float(&[1.5, 1.9])),
            (4, RawValue::Float(&[1.0; 9])),
            (5, RawValue::Int32(&[4096; 4])),
            (6, RawValue::Rectangle(&rects[..1])),
            (7, RawValue::Rectangle(rects)),
        ]
    }

    #[test]
    fn metadata_becomes_readback_and_timing() {
        let rects = [lc_rect(0), lc_rect(8)];
        let mut readback = HashMap::new();
        let timing = record_metadata(metadata(&[123_000], &rects), &mut readback);
        assert_eq!(
            timing,
            RequestTiming {
                sensor_timestamp: Some(123_000),
                frame_duration_ns: Some(33_333_000),
            }
        );
        assert_eq!(
            readback.get(&ControlId(1)),
            Some(&ControlValue::Int(10_000))
        );
        assert_eq!(readback.get(&ControlId(2)), Some(&ControlValue::Float(2.0)));
        // Arrays are not scalars: not read back.
        assert!(!readback.contains_key(&ControlId(3)));
        assert!(!readback.contains_key(&ControlId(4)));
        assert_eq!(
            readback.get(&ControlId(6)),
            Some(&ControlValue::Rect(rect(&rects[0])))
        );
        assert_eq!(
            readback.get(&ControlId(7)),
            Some(&ControlValue::Rects(rects.iter().map(rect).collect()))
        );
        // An equal pair reads as its value (FrameDurationLimits min == max).
        assert_eq!(
            styx_value(RawValue::Int64(&[5, 5])),
            Some(ControlValue::Int(5))
        );
        assert_eq!(styx_value(RawValue::Int64(&[5, 6])), None);
    }

    /// Once every id is known, a frame's metadata allocates nothing, a changed list of
    /// rectangles aside (reused in place when it keeps its length).
    #[test]
    fn steady_metadata_does_not_allocate() {
        let rects = [lc_rect(0), lc_rect(8)];
        let mut readback = HashMap::new();
        record_metadata(metadata(&[1], &rects), &mut readback);
        let (_, n) = allocations(|| {
            for ts in 2..500i64 {
                let ts = [ts];
                std::hint::black_box(record_metadata(metadata(&ts, &rects), &mut readback));
            }
        });
        assert_eq!(n, 0, "steady metadata allocated {n} times");
        let moved = [lc_rect(4), lc_rect(12)];
        let (_, n) = allocations(|| record_metadata(metadata(&[600], &moved), &mut readback));
        assert_eq!(n, 0, "a moved crop of the same length allocated");
        assert_eq!(
            readback.get(&ControlId(7)),
            Some(&ControlValue::Rects(moved.iter().map(rect).collect()))
        );
    }

    /// Through a real libcamera control list (no camera needed): values written with `set` /
    /// `set_styx` read back through `entries`, and neither allocates on the Rust heap.
    #[test]
    fn control_list_round_trip_without_allocating() {
        let mut list = ControlList::new();
        let crops = ControlValue::Rects(vec![
            ControlRect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            ControlRect {
                x: 8,
                y: 8,
                width: 320,
                height: 240,
            },
        ]);
        let (_, n) = allocations(|| {
            set(
                &mut list,
                FRAME_DURATION,
                RawValue::Int64(&[33_333, 33_333]),
            );
            set_styx(&mut list, 1, &ControlValue::Int(7));
            set_styx(&mut list, 2, &ControlValue::Float(0.5));
            set_styx(&mut list, 7, &crops);
        });
        assert_eq!(n, 0, "writing controls allocated {n} times");
        let mut readback = HashMap::new();
        record_metadata(entries(&list), &mut readback);
        let (timing, n) = allocations(|| record_metadata(entries(&list), &mut readback));
        assert_eq!(n, 0, "reading the list allocated {n} times");
        assert_eq!(timing.frame_duration_ns, Some(33_333_000));
        assert_eq!(
            readback.get(&ControlId(FRAME_DURATION)),
            Some(&ControlValue::Int(33_333))
        );
        assert_eq!(readback.get(&ControlId(1)), Some(&ControlValue::Int(7)));
        assert_eq!(readback.get(&ControlId(2)), Some(&ControlValue::Float(0.5)));
        assert_eq!(readback.get(&ControlId(7)), Some(&crops));
    }
}
