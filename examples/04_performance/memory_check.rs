//! Where a served camera's memory goes, and whether its buffers still hold the frame rate: a
//! camera service with one client pulling every frame for a while, then the client's frame
//! rate and gaps, and the process's memory split into capture buffers (dma-buf and video node
//! mappings, plus the dma-bufs held), code, heap and other anonymous memory, from
//! `/proc/self/smaps`.
//!
//! ```text
//! memory_check [seconds] [fps] [WIDTHxHEIGHT] [raw=N,out=N,queue=N]
//! memory_check 20 120 1280x800 raw=3,out=2
//! ```
//!
//! The options (comma separated) set the receiver's raw buffers, the ISP's output buffers
//! allocated at start and the capture queue depth; the rest are `StyxConfig`'s defaults.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

/// Resident and proportional KiB per kind of mapping.
fn smaps() -> BTreeMap<&'static str, (u64, u64)> {
    let mut kinds = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/self/smaps") else {
        return kinds;
    };
    let exe = std::env::current_exe().ok();
    let mut kind = "other";
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let first = words.next().unwrap_or("");
        if first.contains('-') && first.chars().next().is_some_and(|c| c.is_ascii_hexdigit()) {
            let name = words.nth(4).unwrap_or("");
            kind = if name.starts_with("/dmabuf") || name.starts_with("/dev/") {
                "buffers (dma-buf, video)"
            } else if exe.as_ref().is_some_and(|e| e.as_os_str() == name) {
                "code (this binary)"
            } else if name.starts_with('/') {
                "code (libraries)"
            } else if name == "[heap]" {
                "heap"
            } else if name.is_empty() {
                "anonymous (thread arenas, stacks, mmap allocations)"
            } else {
                "other"
            };
            continue;
        }
        let value = words
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let entry = kinds.entry(kind).or_insert((0, 0));
        match first {
            "Rss:" => entry.0 += value,
            "Pss:" => entry.1 += value,
            _ => {}
        }
    }
    kinds
}

/// Bytes of the dma-bufs this process holds a descriptor of, mapped or not.
fn dmabuf_bytes() -> u64 {
    let Ok(dir) = std::fs::read_dir("/proc/self/fdinfo") else {
        return 0;
    };
    dir.flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter(|s| s.contains("exp_name:"))
        .filter_map(|s| {
            s.lines()
                .find(|l| l.starts_with("size:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .sum()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let secs: f64 = args.next().map_or(Ok(20.0), |a| a.parse())?;
    let fps: u32 = args.next().map_or(Ok(120), |a| a.parse())?;
    let size = args.next().unwrap_or_else(|| "1280x800".into());
    let (w, h) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or("size is WIDTHxHEIGHT")?;
    let options = args.next().unwrap_or_default();
    let mut config = StyxConfig::default();
    for option in options.split(',').filter(|o| !o.is_empty()) {
        let (key, value) = option.split_once('=').ok_or("options are key=N")?;
        let n: u32 = value.parse()?;
        config = match key {
            "raw" => config.native_raw_buffers(n),
            "out" => config.native_output_buffers(n),
            "queue" => config.capture_queue_depth(n as usize),
            _ => return Err(format!("unknown option {key}").into()),
        };
    }

    let cameras = styx::probe_all();
    let camera = cameras
        .iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .or(cameras.first())
        .ok_or("no camera")?
        .clone();
    println!(
        "camera: {}, {w}x{h} gray at {fps} fps for {secs} s, options \"{options}\"",
        camera.identity.display
    );
    let path = std::env::temp_dir().join(format!("styx-memory-{}.sock", std::process::id()));
    let service = CameraService::new(camera)
        .keep_streaming()
        .capture_config(config)
        .serve(&path)?;
    let client = FrameClient::request(&path, &Frames::gray().size(w, h).fps(fps))?;

    // The first second warms the pools up.
    let warm = Instant::now() + Duration::from_secs(1);
    while Instant::now() < warm {
        let _ = client.recv(Duration::from_millis(200));
    }
    let start = Instant::now();
    let end = start + Duration::from_secs_f64(secs);
    let (mut frames, mut gaps, mut last_seq) = (0u64, 0u64, None::<u64>);
    while Instant::now() < end {
        if let RecvOutcome::Data(f) = client.recv(Duration::from_millis(200)) {
            frames += 1;
            if let Some(seq) = f.meta().sequence().map(u64::from) {
                if last_seq.is_some_and(|p| seq > p + 1) {
                    gaps += seq - last_seq.unwrap_or(seq) - 1;
                }
                last_seq = Some(seq);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let kinds = smaps();
    let held = dmabuf_bytes();
    drop(client);
    service.stop();

    println!(
        "{frames} frames in {elapsed:.1} s: {:.1} fps, {gaps} sequence gaps",
        frames as f64 / elapsed
    );
    let (mut rss, mut pss) = (0, 0);
    for (kind, (r, p)) in &kinds {
        println!("  {r:7} KiB rss  {p:7} KiB pss  {kind}");
        rss += r;
        pss += p;
    }
    println!("  {rss:7} KiB rss  {pss:7} KiB pss  total");
    println!("  {:7} KiB of dma-bufs held (mapped or not)", held / 1024);
    Ok(())
}
