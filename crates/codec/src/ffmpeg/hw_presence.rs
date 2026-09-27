//! Whether this machine has video codec hardware, checked without loading FFmpeg.
//!
//! Hardware decoders are only probed through FFmpeg (which loads it) when the matching device
//! exists, so machines without them never load FFmpeg just to find nothing.

use std::sync::OnceLock;

use styx_core::prelude::FourCc;

const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
const VIDIOC_ENUM_FMT: libc::c_ulong = 0xC040_5602;
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_8000;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_CAPTURE: u32 = 1;
const BUF_OUTPUT: u32 = 2;
const BUF_CAPTURE_MPLANE: u32 = 9;
const BUF_OUTPUT_MPLANE: u32 = 10;

/// Pixel formats of the V4L2 memory-to-memory devices: compressed formats they accept (decoders)
/// and produce (encoders).
#[derive(Default)]
struct M2mFormats {
    accepted: Vec<[u8; 4]>,
    produced: Vec<[u8; 4]>,
}

fn m2m_formats() -> &'static M2mFormats {
    static FORMATS: OnceLock<M2mFormats> = OnceLock::new();
    FORMATS.get_or_init(|| {
        let mut formats = M2mFormats::default();
        let Ok(entries) = std::fs::read_dir("/dev") else {
            return formats;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("video") {
                continue;
            }
            if let Some(device) = query_m2m(&entry.path()) {
                formats.accepted.extend(device.accepted);
                formats.produced.extend(device.produced);
            }
        }
        formats
    })
}

/// Output-queue (accepted) and capture-queue (produced) formats of an M2M device.
fn query_m2m(path: &std::path::Path) -> Option<M2mFormats> {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: open/ioctl/close on a device path with correctly sized structs.
    unsafe {
        let fd = libc::open(
            cpath.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return None;
        }
        let mut cap = [0u8; 104];
        let result = if libc::ioctl(fd, VIDIOC_QUERYCAP as _, cap.as_mut_ptr()) == 0 {
            let caps = u32::from_ne_bytes(cap[84..88].try_into().unwrap());
            let device_caps = u32::from_ne_bytes(cap[88..92].try_into().unwrap());
            let caps = if caps & V4L2_CAP_DEVICE_CAPS != 0 {
                device_caps
            } else {
                caps
            };
            if caps & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE) != 0 {
                let mplane = caps & V4L2_CAP_VIDEO_M2M_MPLANE != 0;
                let (output, capture) = if mplane {
                    (BUF_OUTPUT_MPLANE, BUF_CAPTURE_MPLANE)
                } else {
                    (BUF_OUTPUT, BUF_CAPTURE)
                };
                Some(M2mFormats {
                    accepted: enum_formats(fd, output),
                    produced: enum_formats(fd, capture),
                })
            } else {
                None
            }
        } else {
            None
        };
        libc::close(fd);
        result
    }
}

unsafe fn enum_formats(fd: libc::c_int, buf_type: u32) -> Vec<[u8; 4]> {
    let mut formats = Vec::new();
    for index in 0u32..64 {
        let mut desc = [0u8; 64];
        desc[0..4].copy_from_slice(&index.to_ne_bytes());
        desc[4..8].copy_from_slice(&buf_type.to_ne_bytes());
        // SAFETY: v4l2_fmtdesc is 64 bytes; the kernel fills pixelformat at offset 44.
        if unsafe { libc::ioctl(fd, VIDIOC_ENUM_FMT as _, desc.as_mut_ptr()) } != 0 {
            break;
        }
        formats.push(desc[44..48].try_into().unwrap());
    }
    formats
}

const JPEG_CODES: &[[u8; 4]] = &[*b"MJPG", *b"JPEG"];
const H264_CODES: &[[u8; 4]] = &[*b"H264"];
const HEVC_CODES: &[[u8; 4]] = &[*b"HEVC"];
const H264_SLICE_CODES: &[[u8; 4]] = &[*b"S264"];
const HEVC_SLICE_CODES: &[[u8; 4]] = &[*b"S265"];

fn stateful_code(input: FourCc) -> &'static [[u8; 4]] {
    match &input.to_u32().to_le_bytes() {
        b"MJPG" | b"JPEG" => JPEG_CODES,
        b"H264" => H264_CODES,
        b"H265" | b"HEVC" => HEVC_CODES,
        _ => &[],
    }
}

fn stateless_code(input: FourCc) -> &'static [[u8; 4]] {
    match &input.to_u32().to_le_bytes() {
        b"H264" => H264_SLICE_CODES,
        b"H265" | b"HEVC" => HEVC_SLICE_CODES,
        _ => &[],
    }
}

/// A stateful V4L2 decoder (FFmpeg `*_v4l2m2m`) for `input`.
pub(crate) fn v4l2m2m_decoder(input: FourCc) -> bool {
    let formats = &m2m_formats().accepted;
    stateful_code(input).iter().any(|c| formats.contains(c))
}

/// A stateless V4L2 decoder (FFmpeg `*_v4l2request`) for `input`.
pub(crate) fn v4l2request_decoder(input: FourCc) -> bool {
    let formats = &m2m_formats().accepted;
    stateless_code(input).iter().any(|c| formats.contains(c))
}

/// A V4L2 encoder (FFmpeg `*_v4l2m2m`) producing `output`.
pub(crate) fn v4l2m2m_encoder(output: FourCc) -> bool {
    let formats = &m2m_formats().produced;
    stateful_code(output).iter().any(|c| formats.contains(c))
}

fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

/// Rockchip MPP (`*_rkmpp`).
pub(crate) fn rkmpp() -> bool {
    exists("/dev/mpp_service")
}

/// NVIDIA Jetson multimedia decoders (`*_nvv4l2dec`, `*_nvmpi`).
pub(crate) fn jetson() -> bool {
    exists("/dev/nvhost-nvdec") || exists("/dev/nvhost-nvdec1")
}

/// An NVIDIA GPU (CUDA / `*_cuvid`).
pub(crate) fn cuda() -> bool {
    exists("/dev/nvidiactl")
}

fn render_nodes() -> Vec<std::path::PathBuf> {
    std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        })
        .collect()
}

/// A render node with a VA-API driver installed.
pub(crate) fn vaapi() -> bool {
    if render_nodes().is_empty() {
        return false;
    }
    let mut dirs: Vec<String> = std::env::var("LIBVA_DRIVERS_PATH")
        .map(|v| v.split(':').map(str::to_string).collect())
        .unwrap_or_default();
    dirs.extend(
        [
            "/usr/lib/dri",
            "/usr/lib64/dri",
            "/usr/lib/x86_64-linux-gnu/dri",
            "/usr/lib/aarch64-linux-gnu/dri",
        ]
        .map(str::to_string),
    );
    dirs.iter().any(|dir| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with("_drv_video.so"))
    })
}

/// An Intel GPU (Quick Sync).
pub(crate) fn qsv() -> bool {
    render_nodes().iter().any(|node| {
        let name = node
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor"))
            .is_ok_and(|v| v.trim() == "0x8086")
    })
}

/// Whether the hardware behind FFmpeg decoder `name` (e.g. `h264_rkmpp`) exists.
pub(crate) fn decoder_hardware(name: &str, input: FourCc) -> bool {
    if name.ends_with("_rkmpp") {
        rkmpp()
    } else if name.ends_with("_v4l2m2m") {
        v4l2m2m_decoder(input)
    } else if name.ends_with("_v4l2request") {
        v4l2request_decoder(input)
    } else if name.ends_with("_nvv4l2dec") || name.ends_with("_nvmpi") {
        jetson()
    } else if name.ends_with("_qsv") {
        qsv()
    } else if name.ends_with("_cuvid") {
        cuda()
    } else {
        true
    }
}
