//! `styx-uvc` on its own: list cameras, stream with per-frame payload details, watch hotplug.
//!
//! ```sh
//! uvc_raw                                   # list cameras, formats, controls
//! uvc_raw stream <YUYV|MJPG> <WxH> <fps> [frames] [detach]
//! uvc_raw async <YUYV|MJPG> <WxH> <fps> [frames]
//! uvc_raw hotplug [seconds]
//! ```

use std::time::{Duration, Instant};

use styx_uvc::{Hotplug, HotplugEvent, OpenOptions, UvcDevice, UvcStream};

fn fourcc(s: &str) -> [u8; 4] {
    let mut c = [b' '; 4];
    for (d, b) in c.iter_mut().zip(s.bytes()) {
        *d = b;
    }
    c
}

fn open(detach: bool) -> Option<UvcDevice> {
    let (cams, errors) = styx_uvc::enumerate();
    for e in errors {
        println!("error: {e}");
    }
    let info = cams.into_iter().next()?;
    match UvcDevice::open(
        info,
        OpenOptions {
            detach_kernel_driver: detach,
        },
    ) {
        Ok(d) => Some(d),
        Err(e) => {
            println!("open: {e}");
            None
        }
    }
}

fn start(dev: &UvcDevice, args: &[String]) -> Option<UvcStream> {
    let code = fourcc(args.get(2).map_or("YUYV", String::as_str));
    let (w, h) = args
        .get(3)
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((640, 480));
    let fps: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(30);
    let Some(mut cfg) = dev.find_mode(code, w, h, Some(10_000_000 / fps)) else {
        println!("no such mode");
        return None;
    };
    // UVC_DAMAGED=1: deliver damaged frames (flagged). UVC_SET=id=value,...: controls first.
    cfg.deliver_damaged = std::env::var_os("UVC_DAMAGED").is_some();
    cfg.mapped_urbs = std::env::var_os("UVC_MMAP").is_some();
    for kv in std::env::var("UVC_SET").unwrap_or_default().split(',') {
        if let Some((id, v)) = kv.split_once('=') {
            let id = u32::from_str_radix(id.trim_start_matches("0x"), 16).unwrap_or(0);
            let v: i64 = v.parse().unwrap_or(0);
            println!("set {id:#x} = {v}: {:?}", dev.set_control(id, v));
        }
    }
    let t = Instant::now();
    match dev.start(cfg) {
        Ok(s) => {
            println!(
                "started in {:.1} ms: {:?}",
                t.elapsed().as_secs_f64() * 1e3,
                s.format()
            );
            Some(s)
        }
        Err(e) => {
            println!("start: {e}");
            None
        }
    }
}

fn print_frame(f: &styx_uvc::UvcFrame, prev: &mut Option<(u64, u64)>) {
    let ts = f.timestamp.as_nanos() as u64;
    let arr = f.first_payload.as_nanos() as u64;
    let (dts, darr) = prev.map_or((0.0, 0.0), |(p, a)| {
        ((ts as f64 - p as f64) / 1e6, (arr as f64 - a as f64) / 1e6)
    });
    *prev = Some((ts, arr));
    println!(
        "#{:<4} {:>7} B pts {:>10?} scr {:?} ts {:.3} ms (+{dts:.3}) from_pts {} arrival +{darr:.3} transfer {:.2} ms flags {:?}",
        f.sequence,
        f.data.len(),
        f.pts,
        f.scr.map(|s| (s.stc, s.sof)),
        ts as f64 / 1e6,
        f.timestamp_from_pts,
        (f.last_payload.as_nanos() as f64 - arr as f64) / 1e6,
        f.flags
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("stream") => {
            let detach = args.get(6).is_some_and(|a| a == "detach");
            let Some(dev) = open(detach) else { return };
            let Some(mut s) = start(&dev, &args) else {
                return;
            };
            let n: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);
            let mut prev = None;
            for _ in 0..n {
                match s.next_blocking(Duration::from_secs(2)) {
                    Ok(f) => print_frame(&f, &mut prev),
                    Err(e) => {
                        println!("error: {e}");
                        break;
                    }
                }
            }
            println!("{:?}", s.stats());
            let t = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
            let f: Vec<&str> = t
                .rsplit_once(')')
                .map_or(vec![], |r| r.1.split_whitespace().collect());
            println!(
                "utime {} stime {} ticks",
                f.get(11).unwrap_or(&"?"),
                f.get(12).unwrap_or(&"?")
            );
        }
        Some("async") => {
            let Some(dev) = open(false) else { return };
            let Some(mut s) = start(&dev, &args) else {
                return;
            };
            let n: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);
            let t = Instant::now();
            let got = styx_graph::rt::block_on(async {
                let mut got = 0;
                for _ in 0..n {
                    match s.next().await {
                        Ok(_) => got += 1,
                        Err(e) => {
                            println!("error: {e}");
                            break;
                        }
                    }
                }
                got
            });
            println!(
                "async: {got} frames in {:.2} s, {:?}",
                t.elapsed().as_secs_f64(),
                s.stats()
            );
        }
        Some("replug") => {
            // Stream, deauthorize the camera, authorize it again, reopen (detaching uvcvideo,
            // which binds again on re-enumeration), stream again.
            let run = |label: &str| {
                let Some(dev) = open(true) else { return };
                let Some(mut s) = start(&dev, &args) else {
                    return;
                };
                let mut n = 0;
                let err = loop {
                    match s.next_blocking(Duration::from_secs(2)) {
                        Ok(_) => n += 1,
                        Err(e) => break e,
                    }
                    if n == 30 {
                        if label == "before" {
                            let port = &dev.info().port;
                            let _ = std::fs::write(
                                format!("/sys/bus/usb/devices/{port}/authorized"),
                                "0",
                            );
                        } else {
                            break styx_uvc::UvcError::Timeout;
                        }
                    }
                };
                println!("{label}: {n} frames, then {err}");
            };
            run("before");
            let _ = std::fs::write("/sys/bus/usb/devices/3-1/authorized", "1");
            let t = Instant::now();
            while styx_uvc::sysfs::find("usb:3-1").is_none()
                && t.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            println!("back after {:?}", t.elapsed());
            run("after");
        }
        Some("hotplug") => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
            let mut hp = Hotplug::new();
            println!(
                "uevents: {}; present: {:?}",
                hp.uses_uevents(),
                hp.cameras().map(|c| c.key()).collect::<Vec<_>>()
            );
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(secs) {
                for ev in hp.poll(Duration::from_millis(500)) {
                    let at = t.elapsed().as_secs_f64();
                    match ev {
                        HotplugEvent::Added(c) => {
                            println!("{at:.2} s added {} {}", c.key(), c.name())
                        }
                        HotplugEvent::Removed(k) => println!("{at:.2} s removed {k}"),
                        HotplugEvent::DriversChanged(c) => {
                            println!("{at:.2} s drivers {} {:?}", c.key(), c.interfaces)
                        }
                    }
                }
            }
        }
        _ => {
            let (cams, errors) = styx_uvc::enumerate();
            for e in errors {
                println!("error: {e}");
            }
            for c in cams {
                println!(
                    "{} {} ({:04x}:{:04x}) {:?} bus {} drivers {:?} uvc {:x} clock {} Hz",
                    c.key(),
                    c.name(),
                    c.vendor_id,
                    c.product_id,
                    c.speed,
                    c.bus_info,
                    c.interfaces,
                    c.function.control.uvc_version,
                    c.function.control.clock_frequency
                );
                for vs in &c.function.streaming {
                    for f in &vs.formats {
                        let fc = f.fourcc().map(|b| String::from_utf8_lossy(&b).into_owned());
                        for fr in &f.frames {
                            println!("  {:?} {}x{} {:?}", fc, fr.width, fr.height, fr.intervals);
                        }
                    }
                }
                if !c.bound_to_kernel_driver()
                    && let Ok(d) = UvcDevice::open(c.clone(), OpenOptions::default())
                {
                    for ctl in d.controls() {
                        println!(
                            "  control {:<28} {}..{} step {} default {} now {:?}",
                            ctl.def.name,
                            ctl.min,
                            ctl.max,
                            ctl.step,
                            ctl.default,
                            d.control(ctl.def.id).ok()
                        );
                    }
                }
            }
        }
    }
}
