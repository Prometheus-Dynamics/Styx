//! PiSP front end statistics buffers (as dequeued from `rp1-cfe-fe_stats`): decoded, queried,
//! converted for the algorithms (both conversions), and run through the default pipeline.

#![no_main]

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use styx_algo::{CameraConfig, FrameMetadata, Pipeline, Tuning};
use styx_pisp::stats::Statistics;
use styx_pisp::uapi::RawStatistics;

fn pipeline() -> &'static Mutex<Pipeline> {
    static P: OnceLock<Mutex<Pipeline>> = OnceLock::new();
    P.get_or_init(|| {
        let mut p = Pipeline::from_tuning(&Tuning::default()).expect("default tuning");
        p.prepare(&CameraConfig::default()).expect("default config");
        Mutex::new(p)
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = Statistics::parse(data) else {
        return;
    };
    let _ = (s.awb_total(), s.histogram_count(), s.histogram_mean());
    for q in [0.0, 0.5, 1.0, -1.0, f64::NAN] {
        let _ = s.histogram_quantile(q);
    }
    for z in &s.awb_zones {
        let _ = z.mean();
    }
    for z in &s.agc_floating {
        let _ = z.mean();
    }
    let stats = styx_pipeline::stats::from_pisp(&s);
    let raw: RawStatistics = bytemuck::pod_read_unaligned(&data[..size_of::<RawStatistics>()]);
    let mut reused = stats.clone();
    styx_pipeline::stats::from_pisp_raw(&raw, &mut reused);
    assert_eq!(reused, stats);
    let _ = stats.mean_luma();
    let mut p = pipeline().lock().unwrap_or_else(|e| e.into_inner());
    let ms = Duration::from_millis(10);
    for frame in 0..2 {
        let _ = p.process(&stats, &FrameMetadata::new(frame, ms, 2.0, ms * 3));
    }
});
