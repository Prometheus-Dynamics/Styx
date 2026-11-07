//! The memory a PiSP frame moved (`styx_core::metrics::moved`), computed from the buffers its
//! jobs touch: the front end writing the raw frame and its statistics; the back end reading
//! the raw frame and the temporal-denoise history, writing its outputs and the next history;
//! each extra pass reading its region of the raw frame again and writing its output.

use lemnos_drivers_pisp::uapi::{ImageFormatConfig, RawStatistics, bayer_enable};
use styx_core::metrics::{TrafficStage, moved};
use styx_pipeline::device::{PispFrame, PispPipeline};

use super::pisp_lease::OutputSpec;

fn bytes(f: &ImageFormatConfig) -> u64 {
    u64::from(f.stride.unsigned_abs()) * u64::from(f.height)
}

/// Counts frame `f`'s traffic, with `specs` the outputs it wrote.
pub(super) fn count(p: &PispPipeline, f: &PispFrame, specs: &[Option<OutputSpec>; 2]) {
    let c = &p.back_end_config().config;
    let raw = bytes(&c.input_format);
    moved(
        TrafficStage::Receiver,
        0,
        raw + std::mem::size_of::<RawStatistics>() as u64,
    );
    let enables = c.global.bayer_enables;
    let tdn = |bit: u32, format: &ImageFormatConfig| {
        if enables & bit != 0 { bytes(format) } else { 0 }
    };
    let outputs: u64 = specs.iter().flatten().map(|s| s.bytes()).sum();
    moved(
        TrafficStage::Isp,
        raw + tdn(bayer_enable::TDN_INPUT, &c.tdn_input_format),
        outputs + tdn(bayer_enable::TDN_OUTPUT, &c.tdn_output_format),
    );
    let input_px = u64::from(c.input_format.width) * u64::from(c.input_format.height);
    for pass in f.passes.iter().flatten() {
        let crop = pass.spec.crop;
        let (w, h) = pass.spec.size.unwrap_or((crop.width, crop.height));
        let Some(Some(output)) = specs.get(pass.spec.output) else {
            continue;
        };
        let written = OutputSpec {
            code: output.code,
            width: u32::from(w),
            height: u32::from(h),
        }
        .bytes();
        let read = raw * u64::from(crop.width) * u64::from(crop.height) / input_px.max(1);
        moved(TrafficStage::IspPass, read, written);
    }
}
