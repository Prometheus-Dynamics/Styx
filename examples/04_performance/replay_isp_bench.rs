//! What replaying raw recordings as processed frames costs (`ReplaySourceConfig::soft_isp`):
//! a synthetic recording of 16-bit BGGR frames (the PiSP's raw frames) is written to DIR, then
//! replayed through the software ISP as fast as it goes; per frame: wall time and this process's
//! CPU time. No camera needed.
//!
//! ```text
//! replay_isp_bench DIR [FRAMES] [WIDTHxHEIGHT] [THREADS]
//! ```

use std::time::{Duration, Instant};

use styx::prelude::*;
use styx::replay::{RecordingHeader, StreamRecorder};

fn cpu_time() -> Duration {
    // SAFETY: `getrusage` fills the zeroed struct for the calling process.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args
        .first()
        .ok_or("usage: replay_isp_bench DIR [FRAMES] [WxH] [THREADS]")?;
    let frames: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let (w, h) = args
        .get(2)
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1280u32, 800u32));
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    let path = format!("{dir}/replay_isp_bench.mcap");
    let format = MediaFormat::new(
        FourCc::new(*b"BYR2"),
        Resolution::new(w, h).unwrap(),
        ColorSpace::Unknown,
    );
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: "replay_isp_bench".into(),
            keys: vec![],
        },
        backend: "native".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut rec = StreamRecorder::with_header(&path, &header)?;
    for i in 0..frames {
        // A ramp with some texture, 10-bit values in the top bits.
        let bytes: Vec<u8> = (0..h)
            .flat_map(|y| {
                (0..w).flat_map(move |x| {
                    let v = ((x * 3 + y * 5 + i * 7) % 900 + 64) as u16;
                    (v << 6).to_le_bytes()
                })
            })
            .collect();
        let mut f = FrameLease::from_visible_bytes(format, u64::from(i) * 33_333_333, &bytes)?;
        f.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence: i,
            exposure_ns: 10_000_000,
            analog_gain: 2.0,
            digital_gain: 1.0,
            frame_duration_ns: 33_333_333,
            aaa: Some(NativeAaa {
                colour_gains: [1.6, 1.0, 1.9],
                colour_temperature_k: 4000.0,
                ..Default::default()
            }),
            ..Default::default()
        }));
        rec.record(&f)?;
    }
    rec.finish()?;
    let mut config = ReplaySourceConfig::new(&path)
        .pacing(ReplayPacing::Unpaced)
        .soft_isp("ov9782");
    if let Some(isp) = config.isp.as_mut() {
        isp.threads = threads;
    }
    let handle = CaptureRequest::replay_source(config)?.open()?;
    // The first frame sets the ISP up: timed apart.
    let t0 = Instant::now();
    let first = handle.recv_blocking(Duration::from_secs(10));
    let setup = t0.elapsed();
    if !matches!(first, RecvOutcome::Data(_)) {
        return Err("no first frame".into());
    }
    let (cpu, start) = (cpu_time(), Instant::now());
    let mut n = 0u32;
    while let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_secs(10)) {
        assert_eq!(f.meta().format.code, FourCc::NV12);
        n += 1;
    }
    let took = start.elapsed().as_secs_f64();
    let cpu = (cpu_time() - cpu).as_secs_f64();
    let _ = std::fs::remove_file(&path);
    println!(
        "{w}x{h}, {threads} ISP threads: first frame (set-up) {:.0} ms; then {n} frames: \
         {:.2} ms each ({:.1} fps), CPU {:.2} ms a frame",
        setup.as_secs_f64() * 1e3,
        took * 1e3 / f64::from(n.max(1)),
        f64::from(n) / took,
        cpu * 1e3 / f64::from(n.max(1))
    );
    Ok(())
}
