//! The one sort the algorithms share (float maths for `no_std` builds is `styx_math`).

/// Sorts `items` by key, ties by index: with the items' positions as indices, the order a
/// stable sort by key gives. One unstable sort shared by every caller, so a firmware image
/// carries one copy of the sort code instead of one per comparison closure (docs/mcu.md).
pub(crate) fn sort_keyed(items: &mut [(f64, u32)]) {
    items.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
}
