//! Debug dump of the back-end configs Styx submits, for diffing against another stack's
//! (libpisp's `BackEnd::GetJsonConfig` reads the same `pisp_be_tiles_config` bytes).
//!
//! `STYX_PISP_BE_DUMP=DIR[:N,N,...]` writes the config of back-end jobs N (default 30 and 90,
//! counted from 0 over the process) to `DIR/styx_be_<N>.bin`; `STYX_PISP_FE_DUMP` does the same
//! for front-end configs (`DIR/styx_fe_<N>.bin`). Unset, each costs one load per config.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::uapi::{BeTilesConfig, bayer_enable};

type Target = OnceLock<Option<(PathBuf, Vec<u64>)>>;
static BE_JOBS: AtomicU64 = AtomicU64::new(0);
static FE_JOBS: AtomicU64 = AtomicU64::new(0);
static BE_TARGET: Target = OnceLock::new();
static FE_TARGET: Target = OnceLock::new();

fn target(t: &'static Target, var: &str) -> Option<&'static (PathBuf, Vec<u64>)> {
    t.get_or_init(|| {
        let v = std::env::var(var).ok()?;
        let (dir, at) = v.split_once(':').unwrap_or((&v, "30,90"));
        let at = at
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        Some((PathBuf::from(dir), at))
    })
    .as_ref()
}

/// Called with each submitted back-end config.
pub(super) fn be_config(bytes: &[u8]) {
    dump(&BE_TARGET, "STYX_PISP_BE_DUMP", &BE_JOBS, "be", bytes);
}

/// Called with each submitted front-end config.
pub(super) fn fe_config(bytes: &[u8]) {
    dump(&FE_TARGET, "STYX_PISP_FE_DUMP", &FE_JOBS, "fe", bytes);
}

fn dump(t: &'static Target, var: &str, jobs: &AtomicU64, what: &str, bytes: &[u8]) {
    let Some((dir, at)) = target(t, var) else {
        return;
    };
    let n = jobs.fetch_add(1, Ordering::Relaxed);
    if at.contains(&n) {
        let path = dir.join(format!("styx_{what}_{n}.bin"));
        match std::fs::write(&path, bytes) {
            Ok(()) => eprintln!("pisp: wrote {what} config {}", path.display()),
            Err(e) => eprintln!("pisp: {what} config dump {}: {e}", path.display()),
        }
    }
}

/// `STYX_PISP_BE_PATCH=REF.bin:block,block,...`: copy the named blocks of a reference
/// (`tdn-reset`, `-sdn` and `-cdn` instead switch that block off; REF can then be any file name)
/// `pisp_be_tiles_config` (e.g. another stack's, dumped with libpisp) into every config Styx
/// submits, to find which block makes an image differ. Unset, it costs one load per job.
static PATCH: OnceLock<Option<(Box<BeTilesConfig>, Vec<String>)>> = OnceLock::new();

fn patch_target() -> Option<&'static (Box<BeTilesConfig>, Vec<String>)> {
    PATCH
        .get_or_init(|| {
            let v = std::env::var("STYX_PISP_BE_PATCH").ok()?;
            let (path, blocks) = v.split_once(':')?;
            let bytes = std::fs::read(path)
                .map_err(|e| eprintln!("pisp: STYX_PISP_BE_PATCH {path}: {e}"))
                .ok()?;
            let Ok(r) = bytemuck::try_pod_read_unaligned::<BeTilesConfig>(&bytes) else {
                eprintln!(
                    "pisp: STYX_PISP_BE_PATCH {path}: {} bytes, not a tiles config",
                    bytes.len()
                );
                return None;
            };
            let blocks: Vec<String> = blocks.split(',').map(|s| s.trim().to_owned()).collect();
            eprintln!("pisp: patching back-end blocks {blocks:?} from {path}");
            Some((Box::new(r), blocks))
        })
        .as_ref()
}

/// The config with the reference's blocks copied in, when `STYX_PISP_BE_PATCH` is set.
pub(super) fn be_patch(cfg: &BeTilesConfig) -> Option<Box<BeTilesConfig>> {
    let (r, blocks) = patch_target()?;
    let (r, mut out) = (&r.config, Box::new(*cfg));
    let c = &mut out.config;
    for b in blocks {
        match b.as_str() {
            "dpc" => c.dpc = r.dpc,
            "geq" => c.geq = r.geq,
            "tdn" => c.tdn = r.tdn,
            "sdn" => c.sdn = r.sdn,
            "blc" => c.blc = r.blc,
            "lsc" => c.lsc = r.lsc,
            "wbg" => c.wbg = r.wbg,
            "cdn" => c.cdn = r.cdn,
            "cac" => c.cac = r.cac,
            "debin" => c.debin = r.debin,
            "tonemap" => c.tonemap = r.tonemap,
            "demosaic" => c.demosaic = r.demosaic,
            "ccm" => c.ccm = r.ccm,
            "ycbcr" => c.ycbcr = r.ycbcr,
            "sharpen" => c.sharpen = r.sharpen,
            "false_colour" => c.false_colour = r.false_colour,
            "sh_fc_combine" => c.sh_fc_combine = r.sh_fc_combine,
            "ycbcr_inverse" => c.ycbcr_inverse = r.ycbcr_inverse,
            "gamma" => c.gamma = r.gamma,
            "resample" => c.resample = r.resample,
            "bayer_enables" => c.global.bayer_enables = r.global.bayer_enables,
            "rgb_enables" => c.global.rgb_enables = r.global.rgb_enables,
            // Not copies: switch a block off (TDN by resetting it every frame).
            "tdn-reset" => c.tdn.reset = 1,
            "-sdn" => c.global.bayer_enables &= !bayer_enable::SDN,
            "-cdn" => c.global.bayer_enables &= !bayer_enable::CDN,
            _ => {}
        }
    }
    Some(out)
}
