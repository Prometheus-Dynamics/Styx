//! `be-replay`: one raw frame through the back end with given configs (byte images of
//! `pisp_be_tiles_config`), output 0 written as packed NV12 per config. For comparing back end
//! settings (e.g. ours against libcamera's) on identical input.

use std::path::Path;
use std::time::Duration;

use styx_pisp::device::{BackEndDevice, BeOutput};
use styx_pisp::uapi::{BayerOrder, BeTilesConfig};

use crate::Args;

/// Runs every `--configs` file on `--raw` (16-bit Bayer, the size of the configs' input).
pub fn run(a: &Args) -> Result<(), String> {
    let raw_path = a.raw.as_ref().ok_or("be-replay needs --raw")?;
    let raw = std::fs::read(raw_path).map_err(|e| format!("{}: {e}", raw_path.display()))?;
    let mut dev: Option<BackEndDevice> = None;
    for path in &a.configs {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: BeTilesConfig = bytemuck::pod_read_unaligned(
            bytes
                .get(..std::mem::size_of::<BeTilesConfig>())
                .ok_or_else(|| format!("{}: too short", path.display()))?,
        );
        let input = cfg.config.input_format;
        if dev.is_none() {
            let order = match cfg.config.global.bayer_order {
                0 => BayerOrder::Rggb,
                1 => BayerOrder::Gbrg,
                2 => BayerOrder::Bggr,
                _ => BayerOrder::Grbg,
            };
            dev = Some(
                BackEndDevice::open(0, input, order, BeOutput::Nv12).map_err(|e| e.to_string())?,
            );
        }
        let d = dev.as_mut().ok_or("no back end")?;
        let (out, took) = d
            .process(&raw, &cfg, Duration::from_secs(1))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let f = d.output_format();
        let (w, h, s) = (f.width as usize, f.height as usize, f.stride as usize);
        let mut packed = Vec::with_capacity(w * h * 3 / 2);
        for row in out.chunks(s).take(h + h / 2) {
            packed.extend_from_slice(&row[..w]);
        }
        let name = Path::new(path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let dst = a.out.join(format!("{name}.nv12"));
        std::fs::write(&dst, &packed).map_err(|e| format!("{}: {e}", dst.display()))?;
        println!("{name}: {took:?} -> {}", dst.display());
    }
    if let Some(d) = dev {
        d.stop().map_err(|e| e.to_string())?;
    }
    Ok(())
}
