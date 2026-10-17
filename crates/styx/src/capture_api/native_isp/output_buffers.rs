//! How many back end buffers a PiSP output gets, and how many from contiguous memory.

use crate::capture_api::StyxConfig;

/// Most buffers per back end output (V4L2 allows 64; each is a frame of the output's size).
pub(super) const MAX_OUTPUT_BUFFERS: u32 = 32;

/// Back end buffers per PiSP output: [`NativeIspConfig::output_buffers`] covers the back
/// end's own and a capture with the default queue and extra buffers (2 + 2) plus slow holders
/// outside it (a frame server's leases); a deeper queue or more extra buffers (the planner's
/// shared captures reserve every consumer's queue and the frame it works on) come on top, so
/// consumers holding what the planner allows them never leave the camera without a buffer.
/// Returns the buffers per output and how many of them come from contiguous memory: at most
/// half of `free` bytes for all the outputs' buffers (`set_bytes` per buffer of every output;
/// the front end's raw buffers and the back end's temporal denoise need contiguous memory
/// too), but never fewer than `output_buffers`. The rest come from the system heap, so
/// consumers holding what the planner allows never starve the camera when contiguous memory
/// is short.
///
/// [`NativeIspConfig::output_buffers`]: crate::capture_api::NativeIspConfig::output_buffers
pub(super) fn pisp_output_buffers(
    config: &StyxConfig,
    set_bytes: u64,
    free: Option<u64>,
) -> (u32, u32) {
    let capture = config.capture_tunables();
    let planned = capture.queue_depth + capture.extra_buffers;
    let baseline = crate::capture_api::tunables::DEFAULT_QUEUE_DEPTH
        + crate::capture_api::tunables::DEFAULT_CAPTURE_EXTRA_BUFFERS;
    let extra = u32::try_from(planned.saturating_sub(baseline)).unwrap_or(u32::MAX);
    let base = config.backends.native.output_buffers.max(2);
    let all = base.saturating_add(extra).min(MAX_OUTPUT_BUFFERS.max(base));
    let contiguous = free.map_or(all, |free| {
        let fits = (free / 2 / set_bytes.max(1)).saturating_sub(u64::from(base));
        base.saturating_add(u32::try_from(fits).unwrap_or(u32::MAX))
            .min(all)
    });
    (all, contiguous)
}

/// Free contiguous memory (`CmaFree`), where the back end's cached dma-heap buffers come from.
pub(super) fn cma_free() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = info
        .lines()
        .find_map(|l| l.strip_prefix("CmaFree:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffers_cover_what_the_planner_reserves() {
        let set = 1280 * 800 * 3 / 2 + 640 * 400 * 3;
        let buffers = |c: &StyxConfig| pisp_output_buffers(c, set, None).0;
        assert_eq!(buffers(&StyxConfig::default()), 6);
        assert_eq!(buffers(&StyxConfig::default().native_output_buffers(8)), 8);
        // A shared capture of two consumers with queues of 3 (the planner: queue 1, extra
        // 3 + 3 + 3 + 3 + 2 = 14): 11 on top of the 6.
        let shared = StyxConfig::new()
            .capture_queue_depth(1)
            .capture_extra_buffers(14);
        assert_eq!(buffers(&shared), 17);
        let huge = StyxConfig::new().capture_extra_buffers(200);
        assert_eq!(buffers(&huge), MAX_OUTPUT_BUFFERS);
        // Contiguous memory for at most half of what is free: 47 MB free on the CM5 holds 10
        // sets of NV12 1280x800 + RGB 640x400, the other 7 come from the system heap; never
        // fewer contiguous than the base set.
        assert_eq!(pisp_output_buffers(&shared, set, Some(47 << 20)), (17, 10));
        assert_eq!(pisp_output_buffers(&shared, set, Some(0)), (17, 6));
        assert_eq!(
            pisp_output_buffers(&StyxConfig::default(), set, Some(0)),
            (6, 6)
        );
    }
}
