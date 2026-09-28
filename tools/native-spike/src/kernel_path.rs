//! The comparison path: raw frames through `rp1-cfe-csi2_ch0` with the normal kernel sensor
//! driver (`ov9282`) bound, its exposure, gain and blanking set through its V4L2 controls.
//! Run with the device lock held and `helios-peripherals` stopped, before `up.sh`. Nothing is
//! written to the sensor except through the kernel driver.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use styx_kernel::media::{self, LinkFlags, MediaDevice};
use styx_kernel::subdev::{MbusCode, MbusFormat, Subdev, Which};
use styx_kernel::v4l2::{
    BufType, ControlValue, Controls, Format, Memory, PixFormat, QueueBuffer, VideoDevice, cid,
};
use styx_kernel::{FourCc, Mapping};

use crate::analysis::{FrameLevels, describe_frame};
use crate::frames::Raw10Layout;
use crate::pipeline::{self, LinkChange};
use crate::{Result, ResultExt, interrupted, log};

const CAPTURE: BufType = BufType::VideoCapture;
/// `MEDIA_BUS_FMT_SBGGR10_1X10`.
const SBGGR10: u32 = 0x3007;
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;

/// One setting of the kernel driver's controls and how many frames to take with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelSetting {
    /// `V4L2_CID_EXPOSURE` in lines (`None`: the maximum).
    pub exposure: Option<i64>,
    /// `V4L2_CID_ANALOGUE_GAIN` code (`None`: the maximum).
    pub gain: Option<i64>,
}

/// Parses `exposure:gain` pairs, `max` for the maximum (`max:max,642:16`).
pub fn parse_settings(s: &str) -> std::result::Result<Vec<KernelSetting>, String> {
    let value = |v: &str| -> std::result::Result<Option<i64>, String> {
        if v == "max" {
            Ok(None)
        } else {
            v.parse()
                .map(Some)
                .map_err(|_| format!("--kernel: '{v}' is not a number or 'max'"))
        }
    };
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (e, g) = p
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("--kernel: '{p}' is not exposure:gain"))?;
            Ok(KernelSetting {
                exposure: value(e)?,
                gain: value(g)?,
            })
        })
        .collect()
}

struct Session {
    media: MediaDevice,
    undo: Vec<LinkChange>,
    video: VideoDevice,
    maps: Vec<Mapping>,
    streaming: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.streaming
            && let Err(e) = self.video.stream_off(CAPTURE)
        {
            log!("kernel path: STREAMOFF: {e}");
        }
        self.maps.clear();
        if let Err(e) = self.video.free_buffers(CAPTURE, Memory::Mmap) {
            log!("kernel path: freeing buffers: {e}");
        }
        for c in self.undo.iter().rev() {
            let flags = if c.enable {
                LinkFlags::ENABLED
            } else {
                LinkFlags::empty()
            };
            if let Err(e) = self.media.setup_link(c.source, c.sink, flags) {
                log!("kernel path: restoring a link: {e}");
            }
        }
        log!("kernel path: stream stopped, buffers freed, links restored");
    }
}

fn find_kernel_sensor() -> Result<(MediaDevice, media::Topology, pipeline::RawPath)> {
    for p in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open(&p) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        if let Ok(path) = pipeline::find_raw_path(&topo)
            && !path.sensor.1.contains("styx")
            && path.sensor_path.is_some()
        {
            log!(
                "kernel path: media {}, sensor \"{}\"",
                p.display(),
                path.sensor.1
            );
            return Ok((dev, topo, path));
        }
    }
    Err("no media graph with a kernel sensor driver feeding csi2 (is ov9282 bound?)".into())
}

fn control_range(sensor: &Subdev, id: u32) -> Result<(i64, i64)> {
    let info = sensor.query_control(id).ctx("query control")?;
    Ok((info.minimum, info.maximum))
}

fn set_int(sensor: &Subdev, id: u32, v: i64) -> Result<()> {
    sensor
        .set_control(id, ControlValue::Integer(v as i32))
        .ctx(&format!("set control {id:#x} = {v}"))
}

fn get_int(sensor: &Subdev, id: u32) -> i64 {
    sensor
        .control(id)
        .ok()
        .and_then(|v| v.as_i64())
        .unwrap_or(-1)
}

/// Streams with each setting in turn and reports levels; saves the last frame of each.
pub fn run(settings: &[KernelSetting], vblank: i64, frames: usize, out_dir: &Path) -> Result<()> {
    let (media, topo, path) = find_kernel_sensor()?;
    let sensor = Subdev::open(path.sensor_path.as_ref().ok_or("no sensor node")?)
        .ctx("open sensor subdev")?;
    let fmt = MbusFormat {
        width: WIDTH,
        height: HEIGHT,
        code: MbusCode(SBGGR10),
        field: 1,
        colorspace: 11,
        ..Default::default()
    };
    let got = sensor
        .set_format(path.sensor_pad, Which::Active, &fmt)
        .ctx("sensor format")?;
    if (got.width, got.height, got.code) != (WIDTH, HEIGHT, MbusCode(SBGGR10)) {
        return Err(format!("sensor chose {got:?}"));
    }
    set_int(&sensor, cid::VBLANK, vblank)?;
    let plan = pipeline::link_plan(&topo, &path);
    let undo: Vec<LinkChange> = plan
        .iter()
        .map(|c| LinkChange {
            enable: !c.enable,
            ..*c
        })
        .collect();
    let csi = Subdev::open(path.receiver_path.as_ref().ok_or("no csi2 node")?).ctx("open csi2")?;
    let video =
        VideoDevice::open(path.node_path.as_ref().ok_or("no raw node")?).ctx("open raw node")?;
    let mut s = Session {
        media,
        undo: Vec::new(),
        video,
        maps: Vec::new(),
        streaming: false,
    };
    for (c, u) in plan.iter().zip(&undo) {
        log!("kernel path: {}", pipeline::describe(&topo, c));
        let flags = if c.enable {
            LinkFlags::ENABLED
        } else {
            LinkFlags::empty()
        };
        s.media
            .setup_link(c.source, c.sink, flags)
            .ctx("setup link")?;
        s.undo.push(*u);
    }
    csi.set_format(path.receiver_sink, Which::Active, &fmt)
        .ctx("csi2 sink format")?;
    let set = s
        .video
        .set_format(
            CAPTURE,
            &Format::Single(PixFormat {
                width: WIDTH,
                height: HEIGHT,
                fourcc: FourCc::new(b"pBAA"),
                field: 1,
                ..Default::default()
            }),
        )
        .ctx("video format")?;
    let Format::Single(p) = set else {
        return Err(format!("unexpected format {set:?}"));
    };
    let layout = Raw10Layout {
        width: WIDTH as usize,
        height: HEIGHT as usize,
        stride: p.bytes_per_line as usize,
    };
    let got = s
        .video
        .request_buffers(CAPTURE, Memory::Mmap, 4)
        .ctx("REQBUFS")?;
    for i in 0..got.count {
        let mut planes = s.video.map_buffer(CAPTURE, i).ctx("mmap")?;
        s.maps.push(planes.remove(0));
        s.video.queue(&QueueBuffer::mmap(CAPTURE, i)).ctx("QBUF")?;
    }
    // Controls before streaming: the driver writes them at stream on.
    apply(
        &sensor,
        settings.first().copied().unwrap_or(KernelSetting {
            exposure: None,
            gain: None,
        }),
    )?;
    s.video.stream_on(CAPTURE).ctx("STREAMON")?;
    s.streaming = true;
    for (i, st) in settings.iter().enumerate() {
        if i > 0 {
            apply(&sensor, *st)?;
        }
        let label = format!(
            "kernel exp {} gain {}",
            get_int(&sensor, cid::EXPOSURE),
            get_int(&sensor, cid::ANALOGUE_GAIN)
        );
        // Let the change land (the driver writes at once; a few frames of delay).
        let mut last = None;
        let mut means = Vec::new();
        for n in 0..frames + 6 {
            let (seq, mean, data) = next(&s, &layout, n + 1 == frames + 6)?;
            if n >= 6 {
                means.push(mean);
            }
            if let Some(d) = data {
                last = Some((seq, d));
            }
        }
        log!(
            "{label}: {} frames, mean level {:.2} (per frame {:.1}..{:.1})",
            means.len(),
            means.iter().sum::<f64>() / means.len().max(1) as f64,
            means.iter().copied().fold(f64::MAX, f64::min),
            means.iter().copied().fold(f64::MIN, f64::max)
        );
        if let Some((seq, data)) = last {
            let f = FrameLevels::unpack(&layout, &data).ctx("unpack")?;
            describe_frame(&label, &f);
            let file: PathBuf = out_dir.join(format!("kernel-path-{i}-{seq}.raw"));
            std::fs::write(&file, &data).ctx("write frame")?;
            log!("{label}: saved {}", file.display());
        }
    }
    Ok(())
}

fn apply(sensor: &Subdev, st: KernelSetting) -> Result<()> {
    let (_, emax) = control_range(sensor, cid::EXPOSURE)?;
    let (gmin, gmax) = control_range(sensor, cid::ANALOGUE_GAIN)?;
    let e = st.exposure.unwrap_or(emax).min(emax);
    let g = st.gain.unwrap_or(gmax).clamp(gmin, gmax);
    // One cluster: the driver writes exposure and gain together (inside its 0x3308 bracket).
    set_int(sensor, cid::ANALOGUE_GAIN, g)?;
    set_int(sensor, cid::EXPOSURE, e)?;
    log!(
        "kernel path: exposure {e} (max {emax}), gain {g} ({gmin}..{gmax}), vblank {}, hblank {}",
        get_int(sensor, cid::VBLANK),
        get_int(sensor, cid::HBLANK)
    );
    Ok(())
}

fn next(s: &Session, layout: &Raw10Layout, copy: bool) -> Result<(u32, f64, Option<Vec<u8>>)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if interrupted() {
            return Err("interrupted".into());
        }
        let ready = s.video.wait(Some(Duration::from_millis(100))).ctx("poll")?;
        if ready.error {
            return Err("raw node error".into());
        }
        if let Some(buf) = s.video.dequeue(CAPTURE, Memory::Mmap).ctx("DQBUF")? {
            let map = &s.maps[buf.index as usize];
            let bytes = &map.as_slice()[..buf.bytes_used().min(map.len())];
            let mean = layout.mean_level(bytes, 4).unwrap_or(f64::NAN);
            let data = copy.then(|| bytes.to_vec());
            s.video
                .queue(&QueueBuffer::mmap(CAPTURE, buf.index))
                .ctx("QBUF")?;
            return Ok((buf.sequence, mean, data));
        }
        if Instant::now() >= deadline {
            return Err("no frame from the kernel path within 3 s".into());
        }
    }
}

/// Analyses a saved frame file (host or device): row bands, statistics, phases.
pub fn analyse_file(path: &Path, stride: usize) -> Result<()> {
    let data = std::fs::read(path).ctx("read frame")?;
    let layout = Raw10Layout {
        width: WIDTH as usize,
        height: HEIGHT as usize,
        stride,
    };
    let f = FrameLevels::unpack(&layout, &data).ctx("unpack")?;
    describe_frame(&path.display().to_string(), &f);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse() {
        let s = parse_settings("max:max, 642:16").unwrap();
        assert_eq!(
            s,
            [
                KernelSetting {
                    exposure: None,
                    gain: None
                },
                KernelSetting {
                    exposure: Some(642),
                    gain: Some(16)
                }
            ]
        );
        assert!(parse_settings("642").is_err());
        assert!(parse_settings("x:1").is_err());
    }
}
