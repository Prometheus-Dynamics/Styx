//! `be-replay --recording BASE --configs MAIN.bin`: every frame of a raw recording through the
//! back end with one config (a main job's `pisp_be_tiles_config`, e.g. dumped from a live run
//! with `LEMNOS_PISP_BE_DUMP`), writing the luma of each output the config enables:
//! `OUT/NAME.out0.grey8` and `OUT/NAME.out1.grey8` (frames concatenated, rows packed), and
//! `OUT/NAME.frames.jsonl` (the recording's index with `"settled"` added per frame).
//!
//! The raw10 samples go in as the front end writes 16-bit Bayer (MSB-aligned: value << 6), so
//! the config must be one for an uncompressed 16-bit input of the recording's size.
//! `"settled"` is true when the frame's exposure and analogue gain equal the recording's last
//! frame's: right for `--fixed` takes, whose first frames still carry the start-up values.

use std::io::Write;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use lemnos_drivers_pisp::device::{BackEndStream, BeFormat, BeOutputSetup};
use lemnos_drivers_pisp::uapi::{BayerOrder, BeTilesConfig, rgb_enable};
use styx_native::dma::{Access, DmaHeap};
use styx_pipeline::rawrec::RawRecording;

use crate::Args;

pub fn run(a: &Args, base: &Path) -> Result<(), String> {
    let rec = RawRecording::open(base).map_err(|e| format!("{}: {e}", base.display()))?;
    let cfg_path = a
        .configs
        .first()
        .ok_or("be-replay --recording needs --configs MAIN.bin")?;
    let bytes = std::fs::read(cfg_path).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    let cfg: BeTilesConfig = bytemuck::pod_read_unaligned(
        bytes
            .get(..std::mem::size_of::<BeTilesConfig>())
            .ok_or_else(|| format!("{}: too short", cfg_path.display()))?,
    );
    let input = cfg.config.input_format;
    let (w, h) = (
        rec.header.format.width as usize,
        rec.header.format.height as usize,
    );
    if (input.width as usize, input.height as usize) != (w, h) {
        return Err(format!(
            "config input {}x{}, recording {w}x{h}",
            input.width, input.height
        ));
    }
    let order = match cfg.config.global.bayer_order {
        0 => BayerOrder::Rggb,
        1 => BayerOrder::Gbrg,
        2 => BayerOrder::Bggr,
        _ => BayerOrder::Grbg,
    };
    let enables = cfg.config.global.rgb_enables;
    let outs: Vec<usize> = [rgb_enable::OUTPUT0, rgb_enable::OUTPUT1]
        .iter()
        .enumerate()
        .filter(|&(_, &bit)| enables & bit != 0)
        .map(|(i, _)| i)
        .collect();
    let setup = |i: usize| {
        let f = cfg.config.output_format[i].image;
        BeOutputSetup {
            format: BeFormat::Nv12,
            width: f.width.into(),
            height: f.height.into(),
            gpu_pad: 0,
        }
    };
    let setups = [
        outs.contains(&0).then(|| setup(0)),
        outs.contains(&1).then(|| setup(1)),
    ];
    let stride = input.stride as usize;
    let len = stride * h;
    let heap = DmaHeap::open("system").map_err(|e| e.to_string())?;
    let buf = heap.allocate(len).map_err(|e| e.to_string())?;
    let mut map = buf.map().map_err(|e| e.to_string())?;
    let fd = buf
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;
    let mut be = BackEndStream::open(0, input, order, vec![fd], len as u32, setups, 2)
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
    let name = base.file_name().unwrap_or_default().to_string_lossy();
    let mut files = Vec::new();
    for &i in &outs {
        let p = a.out.join(format!("{name}.out{i}.grey8"));
        let f = std::fs::File::create(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        files.push((i, std::io::BufWriter::new(f)));
    }
    let mut row = vec![0u16; w];
    let mut took = Vec::with_capacity(rec.len());
    for n in 0..rec.len() {
        let src = rec.frame(n);
        let rs = rec.header.stride;
        buf.begin_cpu_access(Access::Write)
            .map_err(|e| e.to_string())?;
        let dst = map.as_mut_slice();
        for y in 0..h {
            styx_softisp::simd::unpack_raw10_row(&src[y * rs..(y + 1) * rs], &mut row, w);
            for (d, s) in dst[y * stride..y * stride + 2 * w]
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(&row)
            {
                d.copy_from_slice(&(s << 6).to_le_bytes());
            }
        }
        buf.end_cpu_access(Access::Write)
            .map_err(|e| e.to_string())?;
        let job = be
            .process(0, &cfg, Duration::from_secs(1))
            .map_err(|e| format!("frame {n}: {e}"))?;
        took.push(job.elapsed);
        for (i, file) in &mut files {
            let (Some(idx), Some(f)) = (job.outputs[*i], be.output_format(*i)) else {
                return Err(format!("frame {n}: output {i} missing"));
            };
            be.sync_output(*i, idx, true).map_err(|e| e.to_string())?;
            let data = be.output_data(*i, idx).ok_or("output not mapped")?;
            let (ow, oh, os) = (f.width as usize, f.height as usize, f.stride as usize);
            for r in data.chunks(os).take(oh) {
                file.write_all(&r[..ow]).map_err(|e| e.to_string())?;
            }
            be.sync_output(*i, idx, false).map_err(|e| e.to_string())?;
        }
        be.release(&job);
    }
    for (_, f) in &mut files {
        f.flush().map_err(|e| e.to_string())?;
    }
    be.stop().map_err(|e| e.to_string())?;
    write_index(&rec, base, &a.out.join(format!("{name}.frames.jsonl")))?;
    took.sort();
    let p50 = took.get(took.len() / 2).copied().unwrap_or_default();
    println!(
        "{name}: {} frames through the back end (outputs {outs:?}), job p50 {p50:?} -> {}",
        rec.len(),
        a.out.display()
    );
    Ok(())
}

/// The recording's index (its `.jsonl` minus the header line), with `"settled"` added.
fn write_index(rec: &RawRecording, base: &Path, path: &Path) -> Result<(), String> {
    let src = base.with_extension("jsonl");
    let text = std::fs::read_to_string(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    let last = rec
        .frames
        .last()
        .map(|f| (f.sensor.exposure, f.sensor.analogue_gain));
    let mut out = String::new();
    for (line, f) in text.lines().skip(1).zip(&rec.frames) {
        let settled = Some((f.sensor.exposure, f.sensor.analogue_gain)) == last;
        out.push_str(
            line.trim_end()
                .strip_suffix('}')
                .ok_or("index line is not an object")?,
        );
        out.push_str(&format!(",\"settled\":{settled}}}\n"));
    }
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}
