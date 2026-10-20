//! `styx-record convert`: a video file (anything ffmpeg reads: H.264/H.265 MP4, MKV, ...) to an
//! MCAP recording of grey or NV12 frames with the video's timestamps, for replay on a device
//! without ffmpeg (`ReplaySourceConfig`). Runs the `ffmpeg` and `ffprobe` programs on the host
//! (nothing is linked).
//!
//! ```text
//! styx-record convert IN OUT.mcap [--gray|--nv12] [--size WxH] [--lz4]
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use styx::prelude::*;
use styx::replay::{RecordCompression, RecorderOptions, RecordingHeader, StreamRecorder};

/// What to convert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertArgs {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Grey (`true`) or NV12 frames.
    pub gray: bool,
    pub size: Option<(u32, u32)>,
    pub lz4: bool,
}

pub const USAGE: &str =
    "usage: styx-record convert IN OUT.mcap [--gray|--nv12] [--size WxH] [--lz4]";

/// The arguments after `convert`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<ConvertArgs, String> {
    let mut paths = Vec::new();
    let (mut gray, mut size, mut lz4) = (false, None, false);
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--gray" | "--grey" => gray = true,
            "--nv12" => gray = false,
            "--lz4" => lz4 = true,
            "--size" => {
                let v = args.next().ok_or("--size needs WxH")?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                size = Some((
                    w.parse().map_err(|_| "bad --size width")?,
                    h.parse().map_err(|_| "bad --size height")?,
                ));
            }
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let [input, output] = <[PathBuf; 2]>::try_from(paths).map_err(|_| USAGE.to_string())?;
    Ok(ConvertArgs {
        input,
        output,
        gray,
        size,
        lz4,
    })
}

/// What a conversion wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    pub frames: u64,
    pub width: u32,
    pub height: u32,
}

fn tool(name: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(name)
        .args(args)
        .output()
        .map_err(|e| format!("{name}: {e} (is ffmpeg installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A video's first stream: its size and its frames' presentation times (seconds; `None` where
/// ffprobe gives none).
struct Probed {
    size: (u32, u32),
    times: Vec<Option<f64>>,
}

fn probe(input: &Path) -> Result<Probed, String> {
    let path = input.to_str().ok_or("input path is not UTF-8")?;
    let dims = tool(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
            path,
        ],
    )?;
    let (w, h) = dims
        .trim()
        .split_once(',')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.trim().parse().ok()?)))
        .ok_or_else(|| format!("no video stream in {}", input.display()))?;
    let times = tool(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "csv=p=0",
            path,
        ],
    )?;
    let times = times
        .lines()
        .map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    Ok(Probed {
        size: (w, h),
        times,
    })
}

/// Converts `a.input` to `a.output`.
pub fn run(a: &ConvertArgs) -> Result<Converted, String> {
    let Probed { size, times } = probe(&a.input)?;
    let (mut w, mut h) = a.size.unwrap_or(size);
    if !a.gray {
        // NV12 has half-resolution chroma: even sizes.
        (w, h) = (w & !1, h & !1);
    }
    let code = if a.gray { FourCc::GREY } else { FourCc::NV12 };
    let format = MediaFormat::new(
        code,
        Resolution::new(w, h).ok_or("empty frames")?,
        ColorSpace::Srgb,
    );
    let frame_len = if a.gray {
        (w * h) as usize
    } else {
        (w * h * 3 / 2) as usize
    };
    let input = a.input.to_str().ok_or("input path is not UTF-8")?;
    let scale = format!("scale={w}:{h}");
    let mut child = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            input,
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-vf",
            &scale,
            "-f",
            "rawvideo",
            "-pix_fmt",
            if a.gray { "gray" } else { "nv12" },
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e} (is ffmpeg installed?)"))?;
    let mut stdout = child.stdout.take().ok_or("ffmpeg: no output")?;
    let name = a
        .input
        .file_name()
        .map_or_else(|| input.to_string(), |n| n.to_string_lossy().into());
    let header = RecordingHeader {
        device: styx::DeviceIdentity {
            display: name,
            keys: vec![format!("file:{input}")],
        },
        backend: "file".into(),
        format,
        interval: None,
    };
    let options = RecorderOptions {
        compression: if a.lz4 {
            RecordCompression::Lz4
        } else {
            RecordCompression::None
        },
        // Every frame: the converter waits for the disk rather than dropping.
        queue_bytes: 0,
        clock_period: None,
        ..Default::default()
    };
    let mut rec = StreamRecorder::mcap(&a.output, &header, options).map_err(|e| e.to_string())?;
    let first = times.iter().flatten().next().copied().unwrap_or(0.0);
    let mut buf = vec![0u8; frame_len];
    let mut n = 0u64;
    let mut last_ns = None;
    while read_frame(&mut stdout, &mut buf)? {
        // The frame's presentation time from the start; frames without one follow the last
        // at 30 fps.
        let ns = match times.get(n as usize).copied().flatten() {
            Some(t) => ((t - first).max(0.0) * 1e9) as u64,
            None => last_ns.map_or(0, |l: u64| l + 33_333_333),
        };
        last_ns = Some(ns);
        let frame = FrameLease::from_visible_bytes(format, ns, &buf).map_err(|e| e.to_string())?;
        rec.record(&frame).map_err(|e| e.to_string())?;
        n += 1;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("ffmpeg failed: {status}"));
    }
    rec.finish().map_err(|e| e.to_string())?;
    Ok(Converted {
        frames: n,
        width: w,
        height: h,
    })
}

/// Fills `buf` with the next frame; `false` at the end of the video.
fn read_frame(r: &mut impl Read, buf: &mut [u8]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("ffmpeg's output ended inside a frame".into()),
            Ok(k) => filled += k,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        let a =
            parse(["in.mp4", "out.mcap", "--gray", "--size", "640x400"].map(String::from)).unwrap();
        assert!(a.gray && a.size == Some((640, 400)) && !a.lz4);
        assert!(parse(["in.mp4"].map(String::from)).is_err());
    }

    /// A short H.264 video made by ffmpeg converts to an MCAP that replays its frames with
    /// their timestamps. Skipped without ffmpeg on the host.
    #[test]
    fn a_video_converts_to_a_replayable_recording() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("no ffmpeg: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("styx-convert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("in.mp4");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x240:rate=10:duration=1")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&video)
            .status()
            .unwrap();
        if !made.success() {
            eprintln!("ffmpeg cannot make a test video: skipped");
            return;
        }
        let out = dir.join("out.mcap");
        let done = run(&ConvertArgs {
            input: video,
            output: out.clone(),
            gray: false,
            size: None,
            lz4: false,
        })
        .unwrap();
        assert_eq!((done.frames, done.width, done.height), (10, 320, 240));
        let (header, frames) = styx::replay::open_recording(&out).unwrap();
        assert_eq!(header.format.code, FourCc::NV12);
        let ts: Vec<u64> = frames.map(|f| f.unwrap().meta().timestamp).collect();
        assert_eq!(ts.len(), 10);
        assert_eq!(ts[1], 100_000_000);
        let _ = std::fs::remove_dir_all(dir);
    }
}
