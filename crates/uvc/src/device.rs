//! An opened UVC camera: its interfaces claimed through usbfs, controls, and stream
//! negotiation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use styx_kernel::usbfs::{ControlSetup, UsbDevice};

use crate::controls::{self, ControlDef, ControlInfo, Info, Kind, Unit, request};
use crate::descriptors::{Format, Frame, StreamingInterface, UvcFunction};
use crate::probe::{StreamingParams, VS_COMMIT_CONTROL, VS_PROBE_CONTROL};
use crate::stream::{StreamConfig, UvcStream};
use crate::sysfs::UsbCameraInfo;
use crate::{Result, UvcError};

const REQ_GET: u8 = 0xa1;
const REQ_SET: u8 = 0x21;
/// Some cameras take seconds to answer a control while they reconfigure (`uvcvideo` waits 5 s).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// How to open a camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// Detach a kernel driver (`uvcvideo`) from the camera's video interfaces if one holds
    /// them, and give them back when the camera closes. Only the video function's interfaces:
    /// a microphone on the same device keeps its driver. Off by default: the camera then has
    /// to be unbound from `uvcvideo` first.
    pub detach_kernel_driver: bool,
}

pub(crate) struct Inner {
    pub(crate) usb: Arc<UsbDevice>,
    pub(crate) info: UsbCameraInfo,
    claimed: Mutex<Vec<u8>>,
    detached: Vec<u8>,
    pub(crate) streaming: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let claimed = self.claimed.lock().map(|c| c.clone()).unwrap_or_default();
        for n in claimed {
            let _ = self.usb.release_interface(n);
        }
        // Rebinding the control interface's driver probes the whole function again (uvcvideo
        // claims the streaming interfaces itself); the rest is for drivers that bind each.
        for &n in &self.detached {
            if let Err(_e) = self.usb.attach_kernel_driver(n) {
                #[cfg(feature = "tracing")]
                tracing::debug!(interface = n, error = %_e, "uvc: reattaching the kernel driver");
            }
        }
    }
}

/// An opened camera. Clones share it; it is closed (interfaces released, a detached kernel
/// driver reattached) when the last clone and its stream are gone.
#[derive(Clone)]
pub struct UvcDevice {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for UvcDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UvcDevice")
            .field("key", &self.inner.info.key())
            .field("name", &self.inner.info.name())
            .finish()
    }
}

impl UvcDevice {
    /// Opens a camera found with [`crate::enumerate`] and claims its video interfaces.
    pub fn open(info: UsbCameraInfo, options: OpenOptions) -> Result<UvcDevice> {
        let usb = Arc::new(UsbDevice::open(info.node()).map_err(UvcError::from_kernel)?);
        let f = &info.function;
        let wanted: Vec<u8> = std::iter::once(f.control.number)
            .chain(f.streaming.iter().map(|s| s.number))
            .collect();
        let mut claimed = Vec::new();
        let mut detached = Vec::new();
        let result = (|| {
            for &n in &wanted {
                match usb.driver(n).map_err(UvcError::from_kernel)? {
                    None => usb.claim_interface(n).map_err(|e| match e.errno() {
                        Some(libc::EBUSY) => {
                            UvcError::Busy(format!("interface {n} is claimed by another process"))
                        }
                        _ => UvcError::from_kernel(e),
                    })?,
                    Some(d) if d == "usbfs" => {
                        return Err(UvcError::Busy(format!(
                            "interface {n} is claimed by another process through usbfs"
                        )));
                    }
                    Some(driver) if options.detach_kernel_driver => {
                        match usb.detach_and_claim(n) {
                            Ok(()) => detached.push(n),
                            // Released meanwhile (uvcvideo lets go of the streaming
                            // interfaces when its control interface goes).
                            Err(e) if e.errno() == Some(libc::ENODATA) => {
                                usb.claim_interface(n).map_err(UvcError::from_kernel)?;
                            }
                            Err(e) => {
                                return Err(UvcError::Busy(format!(
                                    "detaching {driver} from interface {n}: {e}"
                                )));
                            }
                        }
                        #[cfg(feature = "tracing")]
                        tracing::info!(interface = n, %driver, "uvc: detached the kernel driver");
                    }
                    Some(driver) => {
                        return Err(UvcError::KernelDriver {
                            interface: n,
                            driver,
                        });
                    }
                }
                claimed.push(n);
            }
            Ok(())
        })();
        let inner = Inner {
            usb,
            info,
            claimed: Mutex::new(claimed),
            detached,
            streaming: AtomicBool::new(false),
        };
        // On failure dropping `inner` releases and reattaches what was taken.
        result?;
        Ok(UvcDevice {
            inner: Arc::new(inner),
        })
    }

    /// Opens the camera with key `key` (`usb:3-1`).
    pub fn open_key(key: &str, options: OpenOptions) -> Result<UvcDevice> {
        let info = crate::sysfs::find(key)
            .ok_or_else(|| UvcError::NotFound(format!("no UVC camera at {key}")))?;
        UvcDevice::open(info, options)
    }

    /// What sysfs said about the camera when it was opened.
    pub fn info(&self) -> &UsbCameraInfo {
        &self.inner.info
    }

    /// Its descriptors.
    pub fn function(&self) -> &UvcFunction {
        &self.inner.info.function
    }

    /// The usbfs device.
    pub fn usb(&self) -> &Arc<UsbDevice> {
        &self.inner.usb
    }

    fn entity_request(&self, req: u8, entity: u8, selector: u8, data: &mut [u8]) -> Result<usize> {
        let setup = ControlSetup {
            request_type: if req & 0x80 != 0 { REQ_GET } else { REQ_SET },
            request: req,
            value: u16::from(selector) << 8,
            index: (u16::from(entity) << 8) | u16::from(self.function().control.number),
        };
        self.inner
            .usb
            .control(setup, data, CONTROL_TIMEOUT)
            .map_err(UvcError::from_kernel)
    }

    pub(crate) fn streaming_request(
        &self,
        interface: u8,
        req: u8,
        selector: u8,
        data: &mut [u8],
    ) -> Result<usize> {
        let setup = ControlSetup {
            request_type: if req & 0x80 != 0 { REQ_GET } else { REQ_SET },
            request: req,
            value: u16::from(selector) << 8,
            index: u16::from(interface),
        };
        self.inner
            .usb
            .control(setup, data, CONTROL_TIMEOUT)
            .map_err(UvcError::from_kernel)
    }

    fn entity_of(&self, def: &ControlDef) -> Option<(u8, u64)> {
        let c = &self.function().control;
        match def.unit {
            Unit::CameraTerminal => c.camera_terminal(),
            Unit::ProcessingUnit => c.processing_unit(),
        }
    }

    fn query(&self, def: &ControlDef, req: u8) -> Result<i64> {
        let (entity, _) = self
            .entity_of(def)
            .ok_or_else(|| UvcError::Unsupported(def.name.into()))?;
        let mut buf = vec![0u8; usize::from(def.size)];
        let n = self.entity_request(req, entity, def.selector, &mut buf)?;
        if def.kind == Kind::AeMode && req == request::GET_RES {
            return Ok(i64::from(buf[0]));
        }
        Ok(def.decode(&buf[..n]))
    }

    /// The controls the camera announces and answers for, with their ranges.
    pub fn controls(&self) -> Vec<ControlInfo> {
        let c = &self.function().control;
        let mut out = Vec::new();
        for (unit, bitmap) in [
            (Unit::ProcessingUnit, c.processing_unit().map_or(0, |u| u.1)),
            (Unit::CameraTerminal, c.camera_terminal().map_or(0, |u| u.1)),
        ] {
            for def in controls::announced(unit, bitmap) {
                match self.control_info(def) {
                    Ok(info) => out.push(info),
                    Err(_e) => {
                        #[cfg(feature = "tracing")]
                        tracing::debug!(control = def.name, error = %_e, "uvc: control skipped");
                    }
                }
            }
        }
        out
    }

    fn control_info(&self, def: &'static ControlDef) -> Result<ControlInfo> {
        let (entity, _) = self
            .entity_of(def)
            .ok_or_else(|| UvcError::Unsupported(def.name.into()))?;
        let mut info = [0u8; 1];
        self.entity_request(request::GET_INFO, entity, def.selector, &mut info)?;
        let default = self.query(def, request::GET_DEF).unwrap_or(0);
        let (min, max, step) = match def.kind {
            Kind::Bool => (0, 1, 1),
            Kind::AeMode => {
                let menu = ControlDef::ae_menu(self.query(def, request::GET_RES)? as u8);
                (*menu.first().unwrap_or(&0), *menu.last().unwrap_or(&0), 1)
            }
            Kind::Menu => (
                self.query(def, request::GET_MIN).unwrap_or(0),
                self.query(def, request::GET_MAX).unwrap_or(2),
                1,
            ),
            Kind::Signed | Kind::Unsigned => (
                self.query(def, request::GET_MIN)?,
                self.query(def, request::GET_MAX)?,
                self.query(def, request::GET_RES).unwrap_or(1).max(1),
            ),
        };
        Ok(ControlInfo {
            def,
            min,
            max,
            step,
            default,
            info: Info(info[0]),
        })
    }

    /// A control's current value (V4L2 id and units, [`crate::controls::ids`]).
    pub fn control(&self, id: u32) -> Result<i64> {
        let def = self.announced(id)?;
        self.query(def, request::GET_CUR)
    }

    /// Sets a control (V4L2 id and units).
    pub fn set_control(&self, id: u32, value: i64) -> Result<()> {
        let def = self.announced(id)?;
        let (entity, _) = self
            .entity_of(def)
            .ok_or_else(|| UvcError::Unsupported(def.name.into()))?;
        let mut data = def.encode(value);
        self.entity_request(request::SET_CUR, entity, def.selector, &mut data)?;
        Ok(())
    }

    fn announced(&self, id: u32) -> Result<&'static ControlDef> {
        let def =
            controls::find(id).ok_or_else(|| UvcError::Unsupported(format!("control {id:#x}")))?;
        match self.entity_of(def) {
            Some((_, bitmap)) if bitmap & (1u64 << def.bit) != 0 => Ok(def),
            _ => Err(UvcError::Unsupported(format!(
                "the camera has no {} control",
                def.name
            ))),
        }
    }

    /// Manual exposure of `t` (100 µs steps), automatic exposure off.
    pub fn set_exposure(&self, t: Duration) -> Result<()> {
        self.set_control(controls::ids::EXPOSURE_AUTO, 1)?;
        let units = (t.as_micros() / 100).max(1) as i64;
        self.set_control(controls::ids::EXPOSURE_ABSOLUTE, units)
    }

    /// Automatic exposure on (aperture priority where that is all the camera offers, as on
    /// most webcams) or off.
    pub fn set_auto_exposure(&self, on: bool) -> Result<()> {
        if !on {
            return self.set_control(controls::ids::EXPOSURE_AUTO, 1);
        }
        let def = self.announced(controls::ids::EXPOSURE_AUTO)?;
        let modes = ControlDef::ae_menu(self.query(def, request::GET_RES)? as u8);
        let mode = [0, 3, 2]
            .into_iter()
            .find(|m| modes.contains(m))
            .unwrap_or(0);
        self.set_control(controls::ids::EXPOSURE_AUTO, mode)
    }

    /// Mains frequency for flicker avoidance: 0 off, 1 50 Hz, 2 60 Hz, 3 automatic.
    pub fn set_power_line_frequency(&self, mode: i64) -> Result<()> {
        self.set_control(controls::ids::POWER_LINE_FREQUENCY, mode)
    }

    /// White balance at `kelvin`, automatic white balance off.
    pub fn set_white_balance(&self, kelvin: u32) -> Result<()> {
        self.set_control(controls::ids::AUTO_WHITE_BALANCE, 0)?;
        self.set_control(controls::ids::WHITE_BALANCE_TEMPERATURE, i64::from(kelvin))
    }

    /// The streaming interface `number`, or the first.
    pub fn streaming(&self, number: Option<u8>) -> Result<&StreamingInterface> {
        let s = &self.function().streaming;
        match number {
            Some(n) => s.iter().find(|v| v.number == n),
            None => s.first(),
        }
        .ok_or_else(|| UvcError::NotFound("streaming interface".into()))
    }

    /// The format, frame and interval for `fourcc`, `width`×`height` closest to `interval`
    /// (100 ns units; the frame's default when `None`).
    pub fn find_mode(
        &self,
        fourcc: [u8; 4],
        width: u32,
        height: u32,
        interval: Option<u32>,
    ) -> Option<StreamConfig> {
        find_mode(self.function(), fourcc, width, height, interval)
    }

    /// PROBE and COMMIT: asks for the format, frame and interval and returns what the camera
    /// committed to.
    pub fn negotiate(
        &self,
        interface: u8,
        format: u8,
        frame: u8,
        interval: u32,
    ) -> Result<StreamingParams> {
        let len = StreamingParams::len_for(self.function().control.uvc_version);
        let mut req = StreamingParams::request(format, frame, interval).encode(len);
        self.streaming_request(interface, request::SET_CUR, VS_PROBE_CONTROL, &mut req)?;
        let mut cur = vec![0u8; len];
        let n = self.streaming_request(interface, request::GET_CUR, VS_PROBE_CONTROL, &mut cur)?;
        let got = StreamingParams::decode(&cur[..n]);
        if got.format_index != format || got.frame_index != frame {
            #[cfg(feature = "tracing")]
            tracing::debug!(?got, "uvc: the camera changed the probe");
        }
        let mut commit = got.encode(len);
        self.streaming_request(interface, request::SET_CUR, VS_COMMIT_CONTROL, &mut commit)?;
        Ok(got)
    }

    /// Starts streaming (one stream at a time per camera).
    pub fn start(&self, config: StreamConfig) -> Result<UvcStream> {
        if self.inner.streaming.swap(true, Ordering::AcqRel) {
            return Err(UvcError::Busy("the camera is already streaming".into()));
        }
        UvcStream::start(self.clone(), config).inspect_err(|_| {
            self.inner.streaming.store(false, Ordering::Release);
        })
    }
}

/// See [`UvcDevice::find_mode`].
pub fn find_mode(
    f: &UvcFunction,
    fourcc: [u8; 4],
    width: u32,
    height: u32,
    interval: Option<u32>,
) -> Option<StreamConfig> {
    for vs in &f.streaming {
        for format in vs.formats.iter().filter(|fm| fm.fourcc() == Some(fourcc)) {
            let Some(frame) = format
                .frames
                .iter()
                .find(|fr| u32::from(fr.width) == width && u32::from(fr.height) == height)
            else {
                continue;
            };
            let iv = frame
                .intervals
                .closest(interval.unwrap_or(frame.default_interval))?;
            return Some(StreamConfig::new(vs.number, format.index, frame.index, iv));
        }
    }
    None
}

/// Largest frame buffer a camera may ask for: an 8K RGBA frame and then some. The sizes come
/// from the device (`dwMaxVideoFrameSize`, `dwMaxVideoFrameBufferSize`), which may claim 4 GiB.
const MAX_FRAME_CAPACITY: usize = 256 << 20;

/// The frame buffer size a frame needs: what the device declares, at least an uncompressed
/// frame's exact size, and no more than 4 bytes per pixel (plus headroom for tiny compressed
/// frames) or [`MAX_FRAME_CAPACITY`].
pub(crate) fn frame_capacity(format: &Format, frame: &Frame, params: &StreamingParams) -> usize {
    let declared = (params.max_video_frame_size as usize).max(frame.max_frame_size as usize);
    let exact = format.frame_bytes(frame).unwrap_or(0);
    let pixels = usize::from(frame.width) * usize::from(frame.height);
    let fallback = pixels * 2;
    let plausible = pixels * 4 + (64 << 10);
    declared
        .min(plausible)
        .max(exact)
        .max(if declared == 0 { fallback } else { 0 })
        .min(MAX_FRAME_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::{FormatKind, Intervals};

    fn frame(width: u16, height: u16, max_frame_size: u32) -> Frame {
        Frame {
            index: 1,
            width,
            height,
            max_frame_size,
            default_interval: 333_333,
            intervals: Intervals::Discrete(vec![333_333]),
            bytes_per_line: 0,
        }
    }

    fn format(kind: FormatKind) -> Format {
        Format {
            index: 1,
            kind,
            default_frame: 1,
            frames: Vec::new(),
            color: None,
        }
    }

    #[test]
    fn frame_buffers_are_bounded_whatever_the_device_declares() {
        let mjpeg = format(FormatKind::Mjpeg);
        let yuyv = format(FormatKind::Uncompressed {
            guid: *b"YUY2\0\0\x10\0\x80\0\0\xaa\0\x38\x9b\x71",
            bits_per_pixel: 16,
        });
        let params = |size| StreamingParams {
            max_video_frame_size: size,
            ..StreamingParams::default()
        };
        // The C270 at 640x480: what it declares.
        assert_eq!(
            frame_capacity(&yuyv, &frame(640, 480, 614_400), &params(614_400)),
            614_400
        );
        assert_eq!(
            frame_capacity(&mjpeg, &frame(640, 480, 614_400), &params(0)),
            614_400
        );
        // Nothing declared: two bytes per pixel.
        assert_eq!(
            frame_capacity(&mjpeg, &frame(640, 480, 0), &params(0)),
            614_400
        );
        // 4 GiB claimed for a 640x480 frame: 4 bytes per pixel and 64 KiB.
        assert_eq!(
            frame_capacity(&mjpeg, &frame(640, 480, u32::MAX), &params(u32::MAX)),
            640 * 480 * 4 + (64 << 10)
        );
        // The largest frame a descriptor can describe.
        assert_eq!(
            frame_capacity(
                &yuyv,
                &frame(u16::MAX, u16::MAX, u32::MAX),
                &params(u32::MAX)
            ),
            MAX_FRAME_CAPACITY
        );
    }
}
