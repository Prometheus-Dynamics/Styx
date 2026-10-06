//! Checks the byte order of the RGB formats a libcamera camera delivers against NV12 from the
//! same scene: for each of NV12, RG24, BG24, XR24, XB24, RGBA and BGRA it requests the format,
//! prints what was negotiated (FourCC, size, stride), lets AE/AWB settle, and averages the R, G
//! and B means of a few frames read through Styx's layout for the delivered FourCC
//! (`FourCc::layout_info`). NV12 converted to RGB (with the matrix and range of the frame's
//! colour space; BT.601 when unknown) is the reference, whatever the RGB byte order.
//!
//! Verdicts: `PASS` (within the tolerance of NV12), `FAIL` (closer to NV12 with R and B
//! exchanged: swapped), `MISMATCH` (neither: a colour difference that is not a swap),
//! `INCONCLUSIVE` (R and B are too close in the scene to tell; point the camera at something
//! coloured). Exits 1 on any `FAIL`, else 2 on any `MISMATCH`.
//!
//! ```sh
//! libcamera_format_check [--camera N|NAME] [--size 640x480] [--frames 10] [--tolerance 6]
//!                        [--formats NV12,RG24,...]
//! ```

use std::time::{Duration, Instant};

use styx::prelude::*;

type Error = Box<dyn std::error::Error>;

const FORMATS: [FourCc; 7] = [
    FourCc::NV12,
    FourCc::RG24,
    FourCc::BG24,
    FourCc::XR24,
    FourCc::XB24,
    FourCc::RGBA,
    FourCc::BGRA,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    Mismatch,
    Inconclusive,
}

/// The verdict for `rgb` against the NV12 reference `nv12` (means in 0..=255).
fn verdict(nv12: [f64; 3], rgb: [f64; 3], tolerance: f64) -> Verdict {
    let dist = |a: [f64; 3], b: [f64; 3]| (0..3).map(|i| (a[i] - b[i]).abs()).fold(0.0, f64::max);
    let direct = dist(nv12, rgb);
    let swapped = dist(nv12, [rgb[2], rgb[1], rgb[0]]);
    // A swap moves R and B by |R - B|: below twice the tolerance it cannot be told apart.
    if (nv12[0] - nv12[2]).abs() < 2.0 * tolerance {
        Verdict::Inconclusive
    } else if direct <= tolerance {
        Verdict::Pass
    } else if swapped < direct {
        Verdict::Fail
    } else {
        Verdict::Mismatch
    }
}

/// Mean R, G, B of a packed 8-bit RGB image (every other pixel of every other row), the bytes
/// read in `order`.
fn packed_means(
    data: &[u8],
    stride: usize,
    (w, h): (usize, usize),
    order: PackedChannelOrder,
) -> Option<[f64; 3]> {
    let channels = order.channels();
    let at = |c| channels.iter().position(|&x| x == c);
    let idx = [at(Channel::Red)?, at(Channel::Green)?, at(Channel::Blue)?];
    let bpp = channels.len();
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for y in (0..h).step_by(2) {
        let row = data.get(y * stride..y * stride + w * bpp)?;
        for px in row.chunks_exact(bpp).step_by(2) {
            for (s, &i) in sum.iter_mut().zip(&idx) {
                *s += u64::from(px[i]);
            }
            n += 1;
        }
    }
    let n = n.max(1) as f64;
    Some(sum.map(|s| s as f64 / n))
}

/// (Kr, Kb, full range) of a frame's colour space, as libcamera on Raspberry Pi reports it:
/// sYCC (`Srgb`) is full-range BT.601; the others are limited range.
fn yuv_matrix(color: ColorSpace) -> (f64, f64, bool) {
    match color {
        ColorSpace::Srgb => (0.299, 0.114, true),
        ColorSpace::Bt709 => (0.2126, 0.0722, false),
        ColorSpace::Bt2020 => (0.2627, 0.0593, false),
        ColorSpace::Unknown => (0.299, 0.114, false),
    }
}

fn yuv_to_rgb(y: f64, cb: f64, cr: f64, (kr, kb, full): (f64, f64, bool)) -> [f64; 3] {
    let (y, cb, cr) = if full {
        (y, cb - 128.0, cr - 128.0)
    } else {
        (
            (y - 16.0) * 255.0 / 219.0,
            (cb - 128.0) * 255.0 / 224.0,
            (cr - 128.0) * 255.0 / 224.0,
        )
    };
    let kg = 1.0 - kr - kb;
    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let g = y - (2.0 * kb * (1.0 - kb) * cb + 2.0 * kr * (1.0 - kr) * cr) / kg;
    [r, g, b].map(|v| v.clamp(0.0, 255.0))
}

/// Mean R, G, B of an NV12 image converted per pixel (the top-left luma of each 2x2 block).
fn nv12_means(
    (luma, luma_stride): (&[u8], usize),
    (chroma, chroma_stride): (&[u8], usize),
    (w, h): (usize, usize),
    matrix: (f64, f64, bool),
) -> Option<[f64; 3]> {
    let (mut sum, mut n) = ([0f64; 3], 0u64);
    for cy in 0..h / 2 {
        let yrow = luma.get(2 * cy * luma_stride..2 * cy * luma_stride + w)?;
        let crow = chroma.get(cy * chroma_stride..cy * chroma_stride + w / 2 * 2)?;
        for (cx, uv) in crow.as_chunks::<2>().0.iter().enumerate() {
            let rgb = yuv_to_rgb(
                f64::from(yrow[2 * cx]),
                f64::from(uv[0]),
                f64::from(uv[1]),
                matrix,
            );
            for (s, v) in sum.iter_mut().zip(rgb) {
                *s += v;
            }
            n += 1;
        }
    }
    let n = n.max(1) as f64;
    Some(sum.map(|s| s / n))
}

/// The R, G, B means of a frame through Styx's layout for its FourCC.
fn frame_means(f: &FrameLease) -> Option<[f64; 3]> {
    let format = f.meta().format;
    let size = (
        format.resolution.width.get() as usize,
        format.resolution.height.get() as usize,
    );
    let planes = f.planes();
    if format.code == FourCc::NV12 {
        let (y, uv) = (planes.first()?, planes.get(1)?);
        return nv12_means(
            (y.data(), y.stride()),
            (uv.data(), uv.stride()),
            size,
            yuv_matrix(format.color),
        );
    }
    let packed = format.code.layout_info().packed?;
    (packed.bytes_per_pixel == packed.order.channels().len())
        .then_some(())
        .and_then(|()| packed_means(planes[0].data(), planes[0].stride(), size, packed.order))
}

/// What one format delivered: FourCC, size, first-plane stride, R/G/B means.
type Measured = (FourCc, (u32, u32), usize, [f64; 3]);

struct Row {
    requested: FourCc,
    result: Result<Measured, String>,
}

/// One format: start, settle, average `frames` frames' means.
fn measure(
    device: &ProbedDevice,
    code: FourCc,
    (w, h): (u32, u32),
    frames: usize,
) -> Result<Measured, Error> {
    // The probed mode of this format nearest the size; one made from NV12's otherwise (the
    // backend then says whether the format works).
    let mut device = device.clone();
    let backend = device
        .backends
        .iter_mut()
        .find(|b| b.kind == BackendKind::Libcamera)
        .ok_or("no libcamera backend")?;
    let nearest = |c: FourCc| {
        backend
            .descriptor
            .modes
            .iter()
            .filter(|m| m.format.code == c)
            .min_by_key(|m| {
                let r = m.format.resolution;
                r.width.get().abs_diff(w) + r.height.get().abs_diff(h)
            })
            .cloned()
    };
    let mode = match nearest(code) {
        Some(m) => m,
        None => {
            let mut m = nearest(FourCc::NV12).ok_or("no NV12 mode")?;
            println!("  {code}: not among the probed modes; requesting it anyway");
            m.format.code = code;
            m.id.format.code = code;
            backend.descriptor.modes.push(m.clone());
            m
        }
    };
    let handle = CaptureRequest::new(&device)
        .backend(BackendKind::Libcamera)
        .mode(mode.id.clone())
        .config(StyxConfig::default().libcamera_output_size(w, h))
        .start()?;
    let next = || match handle.recv_blocking(Duration::from_secs(3)) {
        RecvOutcome::Data(f) => Ok::<_, Error>(f),
        _ => Err("no frame".into()),
    };
    // AE/AWB settle.
    let settle = Instant::now() + Duration::from_secs(1);
    let mut first = next()?;
    while Instant::now() < settle {
        first = next()?;
    }
    let format = first.meta().format;
    let stride = first.planes().first().map_or(0, |p| p.stride());
    drop(first);
    let mut sum = [0f64; 3];
    for _ in 0..frames {
        let f = next()?;
        let m = frame_means(&f).ok_or_else(|| format!("{} is not RGB or NV12", format.code))?;
        for (s, v) in sum.iter_mut().zip(m) {
            *s += v;
        }
    }
    handle.stop();
    let r = format.resolution;
    Ok((
        format.code,
        (r.width.get(), r.height.get()),
        stride,
        sum.map(|s| s / frames.max(1) as f64),
    ))
}

struct Args {
    camera: Option<String>,
    size: (u32, u32),
    frames: usize,
    tolerance: f64,
    formats: Vec<FourCc>,
}

fn args() -> Result<Args, Error> {
    let mut a = Args {
        camera: None,
        size: (640, 480),
        frames: 10,
        tolerance: 6.0,
        formats: FORMATS.to_vec(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--camera" => a.camera = Some(value()?),
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                a.size = (w.parse()?, h.parse()?);
            }
            "--frames" => a.frames = value()?.parse()?,
            "--tolerance" => a.tolerance = value()?.parse()?,
            "--formats" => {
                a.formats = value()?
                    .split(',')
                    .map(|s| s.parse::<FourCc>().map_err(|e| format!("{s}: {e}")))
                    .collect::<Result<_, _>>()?;
            }
            _ => {
                return Err(
                    "usage: libcamera_format_check [--camera N|NAME] [--size WxH] \
                            [--frames N] [--tolerance T] [--formats NV12,RG24,...]"
                        .into(),
                );
            }
        }
    }
    Ok(a)
}

fn main() -> Result<(), Error> {
    let args = args()?;
    let cameras: Vec<ProbedDevice> = probe_all()
        .into_iter()
        .filter(|d| d.backends.iter().any(|b| b.kind == BackendKind::Libcamera))
        .collect();
    let device = match &args.camera {
        None => cameras.first(),
        Some(c) => match c.parse::<usize>() {
            Ok(i) => cameras.get(i),
            Err(_) => cameras
                .iter()
                .find(|d| d.identity.display.contains(c.as_str())),
        },
    }
    .ok_or("no such libcamera camera")?;
    println!("{} through libcamera", device.identity.display);
    let mut rows = Vec::new();
    for &code in &args.formats {
        let result = measure(device, code, args.size, args.frames).map_err(|e| e.to_string());
        match &result {
            Ok((got, (w, h), stride, _)) => {
                println!("  {code}: negotiated {got} {w}x{h}, stride {stride}")
            }
            Err(e) => println!("  {code}: unsupported: {e}"),
        }
        rows.push(Row {
            requested: code,
            result,
        });
    }

    println!(
        "\n{:<6} {:<6} {:>10} {:>7} {:>7} {:>7} {:>7}",
        "asked", "got", "size", "stride", "R", "G", "B"
    );
    for row in &rows {
        match &row.result {
            Ok((got, (w, h), stride, [r, g, b])) => println!(
                "{:<6} {:<6} {:>10} {stride:>7} {r:>7.1} {g:>7.1} {b:>7.1}",
                row.requested.to_string(),
                got.to_string(),
                format!("{w}x{h}")
            ),
            Err(_) => println!("{:<6} unsupported", row.requested.to_string()),
        }
    }

    let reference = rows.iter().find_map(|r| match &r.result {
        Ok((FourCc::NV12, _, _, m)) => Some(*m),
        _ => None,
    });
    let Some(nv12) = reference else {
        return Err("no NV12 reference: cannot judge the RGB formats".into());
    };
    println!(
        "\nverdicts (tolerance {}/255 against NV12):",
        args.tolerance
    );
    let (mut failed, mut mismatched, mut inconclusive) = (false, false, false);
    for row in rows.iter().filter(|r| r.requested != FourCc::NV12) {
        let name = row.requested;
        let Ok((got, _, _, means)) = &row.result else {
            println!("  {name}: unsupported");
            continue;
        };
        if *got == FourCc::NV12 {
            println!("  {name}: delivered NV12 instead; nothing to check");
            continue;
        }
        let v = verdict(nv12, *means, args.tolerance);
        let via = if *got == name {
            String::new()
        } else {
            format!(" (delivered as {got})")
        };
        match v {
            Verdict::Pass => println!("  {name}: PASS{via}"),
            Verdict::Fail => {
                failed = true;
                println!("  {name}: FAIL{via}: R and B look swapped");
            }
            Verdict::Mismatch => {
                mismatched = true;
                println!("  {name}: MISMATCH{via}: differs from NV12 but not by a R/B swap");
            }
            Verdict::Inconclusive => {
                inconclusive = true;
                println!("  {name}: INCONCLUSIVE{via}: R and B are about equal in this scene");
            }
        }
    }
    if inconclusive {
        println!(
            "R ({:.1}) and B ({:.1}) are too close to tell a swap: point the camera at \
             something strongly coloured (red or blue) and run again.",
            nv12[0], nv12[2]
        );
    }
    if failed {
        std::process::exit(1);
    }
    if mismatched {
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [f64; 3] = [180.0, 60.0, 40.0];

    #[test]
    fn verdicts() {
        assert_eq!(verdict(RED, [183.0, 58.0, 42.0], 6.0), Verdict::Pass);
        assert_eq!(verdict(RED, [42.0, 58.0, 183.0], 6.0), Verdict::Fail);
        assert_eq!(verdict(RED, [120.0, 90.0, 40.0], 6.0), Verdict::Mismatch);
        let grey = [100.0, 100.0, 104.0];
        assert_eq!(
            verdict(grey, [104.0, 100.0, 100.0], 6.0),
            Verdict::Inconclusive
        );
    }

    #[test]
    fn packed_orders() {
        // One red pixel (R 200, G 10, B 30) in each layout; 2x2 so the sampling sees one.
        let px = |code: FourCc| -> Vec<u8> {
            let order = code.layout_info().packed.unwrap().order;
            let one: Vec<u8> = order
                .channels()
                .iter()
                .map(|c| match c {
                    Channel::Red => 200,
                    Channel::Green => 10,
                    Channel::Blue => 30,
                    _ => 255,
                })
                .collect();
            one.repeat(4)
        };
        for code in [
            FourCc::RG24,
            FourCc::BG24,
            FourCc::XR24,
            FourCc::XB24,
            FourCc::RGBA,
            FourCc::BGRA,
        ] {
            let packed = code.layout_info().packed.unwrap();
            let data = px(code);
            let m = packed_means(&data, 2 * packed.bytes_per_pixel, (2, 2), packed.order);
            assert_eq!(m, Some([200.0, 10.0, 30.0]), "{code}");
        }
        // XR24 is B, G, R, x in memory.
        assert_eq!(&px(FourCc::XR24)[..4], &[30, 10, 200, 255]);
    }

    #[test]
    fn nv12_grey_and_red() {
        let full = yuv_matrix(ColorSpace::Srgb);
        let grey = yuv_to_rgb(128.0, 128.0, 128.0, full);
        assert!(grey.iter().all(|&v| (v - 128.0).abs() < 1e-9));
        // Full-range BT.601 red (255, 0, 0): Y 76.2, Cb 84.97, Cr 255.5.
        let red = yuv_to_rgb(76.245, 84.972, 255.5, full);
        assert!(
            (red[0] - 255.0).abs() < 0.5 && red[1] < 0.5 && red[2] < 0.5,
            "{red:?}"
        );
        let (y, uv) = ([76u8; 4], [85u8, 255]);
        let m = nv12_means((&y, 2), (&uv, 2), (2, 2), full).unwrap();
        assert!(m[0] > 250.0 && m[2] < 5.0, "{m:?}");
    }
}
