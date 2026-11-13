//! The tables' generation: new when the tables change, kept while they do not, so the copies
//! and compares between the algorithm and an ISP skip unchanged tables.

use super::*;

fn stats(w: u32, h: u32) -> Statistics {
    use crate::stats::{ColourZone, ZoneGrid};
    Statistics {
        colour: ZoneGrid {
            width: w,
            height: h,
            zones: vec![
                ColourZone {
                    r: 200.0,
                    g: 200.0,
                    b: 200.0,
                    counted: 1000,
                };
                (w * h) as usize
            ],
        },
        before_lsc: true,
        ..Default::default()
    }
}

#[test]
fn generation_changes_only_with_the_tables() {
    let mut a = Alsc::new(AlscTuning {
        grid: (8, 6),
        startup_frames: 2,
        frame_period: 4,
        ..Default::default()
    })
    .unwrap();
    a.prepare(&CameraConfig::default()).unwrap();
    let mut p = Params::default();
    a.initial(&mut p);
    let first = p.lens_shading.as_ref().unwrap().generation;
    assert_ne!(first, 0);
    let meta = |f| FrameMetadata::new(f, Default::default(), 1.0, Default::default());
    let mut gens = Vec::new();
    for f in 0..40 {
        a.process(&stats(8, 6), &meta(f), &mut p);
        gens.push(p.lens_shading.as_ref().unwrap().generation);
    }
    // After start-up the tables change at most once per run (every `frame_period` frames: the
    // run's result is taken, then they hold), so 3 frames in 4 skip the copy and the compare.
    let changes = gens[8..].windows(2).filter(|g| g[0] != g[1]).count();
    assert!(changes <= gens[8..].len() / 4 + 1, "{gens:?}");
    // Equal to a full copy made without the generation.
    let ls = p.lens_shading.clone().unwrap();
    let mut plain = ls.clone();
    plain.generation = 0;
    assert_eq!(plain, ls);
    // A copy holding the generation is not rewritten; an edited one (generation 0) is.
    let mut held = ls.clone();
    held.r[0] = 9.0;
    held.clone_from(&ls);
    assert_eq!(held.r[0], 9.0, "same generation: no copy");
    held.generation = 0;
    assert_ne!(held, ls);
    held.clone_from(&ls);
    assert_eq!(held.r, ls.r);
    // A prepare makes new tables, with a new generation.
    a.prepare(&CameraConfig::default()).unwrap();
    a.initial(&mut p);
    assert_ne!(
        p.lens_shading.as_ref().unwrap().generation,
        *gens.last().unwrap()
    );
}
