//! The PiSP front end's focus (CDAF) grid over AF's windows, and the area a grid covers.

use lemnos_drivers_pisp::uapi::{CDAF_STATS_SIZE, FeCdafStatsConfig};
use styx_algo::FocusArea;

/// The front end's 8x8 CDAF grid over `area` of the `w x h` statistics frame (the whole frame
/// for `None`): zones of even sizes, at least 2, inside the frame.
pub(crate) fn cdaf_grid(
    current: FeCdafStatsConfig,
    area: Option<FocusArea>,
    w: u16,
    h: u16,
) -> FeCdafStatsConfig {
    let n = CDAF_STATS_SIZE as f64;
    let axis = |total: u16, at: f64, len: f64| {
        let total = f64::from(total);
        let size = ((len.clamp(0.0, 1.0) * total / n) as u16 & !1).max(2);
        let room = (total - n * f64::from(size)).max(0.0);
        let offset = ((at.clamp(0.0, 1.0) * total).min(room) as u16) & !1;
        (offset, size)
    };
    let (ax, ay, aw, ah) = area.map_or((0.0, 0.0, 1.0, 1.0), |a| (a.x, a.y, a.width, a.height));
    let (offset_x, size_x) = axis(w, ax, aw);
    let (offset_y, size_y) = axis(h, ay, ah);
    FeCdafStatsConfig {
        offset_x,
        offset_y,
        size_x,
        size_y,
        ..current
    }
}

/// The part of the `w x h` statistics frame `c`'s CDAF grid covers.
#[cfg(any(feature = "device", test))]
pub(crate) fn cdaf_area(c: &FeCdafStatsConfig, w: u16, h: u16) -> Option<FocusArea> {
    let (w, h) = (f64::from(w.max(1)), f64::from(h.max(1)));
    let n = CDAF_STATS_SIZE as f64;
    (c.size_x > 0 && c.size_y > 0).then(|| FocusArea {
        x: f64::from(c.offset_x) / w,
        y: f64::from(c.offset_y) / h,
        width: n * f64::from(c.size_x) / w,
        height: n * f64::from(c.size_y) / h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_covers_the_area_inside_the_frame() {
        let whole = cdaf_grid(FeCdafStatsConfig::default(), None, 1280, 800);
        assert_eq!((whole.offset_x, whole.size_x, whole.size_y), (0, 160, 100));
        let area = FocusArea {
            x: 0.25,
            y: 0.25,
            width: 0.5,
            height: 0.5,
        };
        let c = cdaf_grid(FeCdafStatsConfig::default(), Some(area), 1280, 800);
        assert_eq!(
            (c.offset_x, c.offset_y, c.size_x, c.size_y),
            (320, 200, 80, 50 & !1)
        );
        let back = cdaf_area(&c, 1280, 800).unwrap();
        assert!((back.x - 0.25).abs() < 1e-9 && (back.width - 0.5).abs() < 1e-9);
        // At the edge: moved in, not past the frame; tiny: zones of 2.
        let edge = FocusArea {
            x: 0.99,
            y: 0.0,
            width: 0.001,
            height: 0.001,
        };
        let c = cdaf_grid(FeCdafStatsConfig::default(), Some(edge), 1280, 800);
        assert_eq!(c.size_x, 2);
        assert!(c.offset_x + 8 * c.size_x <= 1280);
    }
}
