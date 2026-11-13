//! Capture with controls: set exposure and gain for one named frame, and see them land there.
//!
//! ```text
//! cargo run -p styx-examples --example capture -- --sim [--out DIR]
//! ```
//!
//! A control names the frame it must show on (`Update::frame`). The backend posts the value
//! early enough for the sensor's delay, so the frame named is the first one to show it. This
//! example reads the first frame to learn where the stream is, asks for a new exposure and gain
//! four frames on, and then reads on past that frame. The metadata of each frame says what was in
//! force on it, so the landing is printed frame by frame.
//!
//! Then it writes the named frame's raw bytes to a file (its plane 0, row by row with the stride
//! the buffer gives). A JPEG is written only when `styx-jpeg` takes the frame's format; the
//! simulated camera makes RGGB10P (raw Bayer), which it does not take.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use styx_controls::{Control, ControlValue, FrameMetadata, Update};
use styx_examples::{Lab, flag, value, with_first_camera};
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_jpeg::{JpegEncoder, PlanarInput, descriptor};

/// How many frames ahead of the first frame read the new controls are asked to show.
const AHEAD: u64 = 4;
/// The exposure and analogue gain asked for, against the camera's 10 ms and 1.0x start (the sensor takes 9 to 16.4 ms at this frame length).
const EXPOSURE_US: u32 = 12_000;
const GAIN_MILLI: u32 = 2_000;
/// How long to wait for each frame.
const WAIT: Duration = Duration::from_secs(5);
/// The JPEG quality, when a frame is encoded.
const QUALITY: u8 = 90;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("capture: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let lab = if flag("--sim") {
        Lab::sim("capture")?
    } else {
        Lab::real(value("--lemnos").map(PathBuf::from))
    };
    let out =
        value("--out").map_or_else(|| std::env::temp_dir().join("styx-examples"), PathBuf::from);
    with_first_camera(&lab, |camera, selected| -> Result<(), String> {
        println!(
            "camera {} owned by {} ({})",
            selected.id, selected.owner, selected.reason
        );
        let next =
            |camera: &mut dyn styx_select::Capture| -> Result<FrameLease<FrameMetadata>, String> {
                camera
                    .next_frame(WAIT)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no frame within {} s", WAIT.as_secs()))
            };

        let start = next(camera)?.id();
        let target = start + AHEAD;
        println!(
            "asking for exposure_us={EXPOSURE_US} and analogue_gain_milli={GAIN_MILLI} on frame {target} (the first frame read is {start})"
        );
        camera
            .set_controls(&[
                Update {
                    control: Control::Exposure,
                    value: ControlValue::Manual(EXPOSURE_US),
                    frame: target,
                },
                Update {
                    control: Control::Gain,
                    value: ControlValue::Manual(GAIN_MILLI),
                    frame: target,
                },
            ])
            .map_err(|e| e.to_string())?;

        // Read past the target, and keep the target frame to write it out.
        let mut landed: Option<u64> = None;
        let mut named: Option<FrameLease<FrameMetadata>> = None;
        loop {
            let frame = next(camera)?;
            let id = frame.id();
            let meta = frame.meta();
            let shows = meta.exposure_us == EXPOSURE_US && meta.analogue_gain_milli == GAIN_MILLI;
            if shows && landed.is_none() {
                landed = Some(id);
            }
            println!(
                "frame id={id} exposure_us={} analogue_gain_milli={}{}",
                meta.exposure_us,
                meta.analogue_gain_milli,
                if id == target { "  <- named" } else { "" }
            );
            if id == target {
                named = Some(frame);
            } else if id > target + 2 {
                break;
            }
        }
        match landed {
            Some(id) if id == target => println!("landed on frame {target}, the frame named"),
            Some(id) => {
                return Err(format!(
                    "the controls landed on frame {id}, not frame {target}"
                ));
            }
            None => return Err("the controls never showed in the metadata".to_string()),
        }

        let frame = named.ok_or("the named frame was not read")?;
        write_outputs(&frame, &out)
    })?
}

/// Writes the frame's raw bytes (plane 0, row by row at the buffer's stride) and, when the format
/// has a JPEG encoder here, its JPEG.
fn write_outputs(frame: &FrameLease<FrameMetadata>, out: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let format = frame.format();
    let (width, height) = (
        format.resolution.width.get() as usize,
        format.resolution.height.get() as usize,
    );
    let read = frame.read().ok_or("the frame's buffer cannot be read")?;
    let plane = read.plane(0).ok_or("the frame has no plane 0")?;
    let stride = plane.stride;
    // Raw: the rows of plane 0 at the stride, as the buffer holds them.
    let row_bytes = stride;
    let mut raw = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        raw.extend_from_slice(plane.row_bytes(row, row_bytes).ok_or("short plane")?);
    }
    let raw_path = out.join(format!("frame-{}.raw", frame.id()));
    fs::write(&raw_path, &raw).map_err(|e| format!("{}: {e}", raw_path.display()))?;
    println!(
        "wrote {} ({} bytes: {width}x{height} {} at stride {stride})",
        raw_path.display(),
        raw.len(),
        format.code
    );

    match encode_jpeg(&read, format.code, width, height)? {
        Some(jpeg) => {
            let path = out.join(format!("frame-{}.jpg", frame.id()));
            fs::write(&path, &jpeg).map_err(|e| format!("{}: {e}", path.display()))?;
            println!(
                "wrote {} ({} bytes, quality {QUALITY})",
                path.display(),
                jpeg.len()
            );
        }
        None => println!(
            "no JPEG: styx-jpeg takes I420, YU12, NV12, GREY and R8; this frame is {} (raw Bayer)",
            format.code
        ),
    }
    Ok(())
}

/// Encodes the frame when `styx-jpeg` takes its format; `None` when it does not.
fn encode_jpeg(
    read: &styx_frame::CpuRead<'_, FrameMetadata>,
    code: FourCc,
    width: usize,
    height: usize,
) -> Result<Option<Vec<u8>>, String> {
    if descriptor(code).is_none() {
        return Ok(None);
    }
    let plane = |i: usize| read.plane(i).ok_or("the frame is missing a plane");
    let input = if code == FourCc::GREY || code == FourCc::R8 {
        let y = plane(0)?;
        PlanarInput::Gray {
            y: y.data,
            width,
            height,
            stride: y.stride,
        }
    } else if code == FourCc::NV12 {
        let (y, uv) = (plane(0)?, plane(1)?);
        PlanarInput::Nv12 {
            y: y.data,
            uv: uv.data,
            width,
            height,
            y_stride: y.stride,
            uv_stride: uv.stride,
        }
    } else {
        let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
        PlanarInput::I420 {
            y: y.data,
            u: u.data,
            v: v.data,
            width,
            height,
            y_stride: y.stride,
            c_stride: u.stride,
        }
    };
    let mut out = Vec::new();
    JpegEncoder::new(QUALITY)
        .encode(&input, &mut out)
        .map_err(|e| format!("jpeg: {e}"))?;
    Ok(Some(out))
}
