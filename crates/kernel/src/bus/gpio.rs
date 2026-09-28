//! GPIO through the GPIO character device, uAPI v2 (`/dev/gpiochipN`).
//!
//! A sensor driver needs a few output lines (reset, power-down, enables). Request them by
//! offset or by line name with [`GpioChip::request_outputs`]; the returned [`GpioLines`] holds
//! them until it is dropped, which releases them.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::ioctl::{ioctl, ior, iowr};

const GPIO_MAX_NAME_SIZE: usize = 32;
/// Most lines one request can hold (`GPIO_V2_LINES_MAX`).
pub const MAX_LINES: usize = 64;
const GPIO_V2_LINE_NUM_ATTRS_MAX: usize = 10;

const GPIO_V2_LINE_FLAG_USED: u64 = 1 << 0;
const GPIO_V2_LINE_FLAG_ACTIVE_LOW: u64 = 1 << 1;
const GPIO_V2_LINE_FLAG_INPUT: u64 = 1 << 2;
const GPIO_V2_LINE_FLAG_OUTPUT: u64 = 1 << 3;
const GPIO_V2_LINE_FLAG_OPEN_DRAIN: u64 = 1 << 6;
const GPIO_V2_LINE_FLAG_OPEN_SOURCE: u64 = 1 << 7;

const GPIO_V2_LINE_ATTR_ID_FLAGS: u32 = 1;
const GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES: u32 = 2;

/// `struct gpiochip_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpioChipInfo {
    pub name: [u8; GPIO_MAX_NAME_SIZE],
    pub label: [u8; GPIO_MAX_NAME_SIZE],
    pub lines: u32,
}

/// `struct gpio_v2_line_attribute` (the union is 8 bytes: flags / values / debounce).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GpioV2LineAttribute {
    pub id: u32,
    pub padding: u32,
    pub value: u64,
}

/// `struct gpio_v2_line_config_attribute`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GpioV2LineConfigAttribute {
    pub attr: GpioV2LineAttribute,
    pub mask: u64,
}

/// `struct gpio_v2_line_config`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GpioV2LineConfig {
    pub flags: u64,
    pub num_attrs: u32,
    pub padding: [u32; 5],
    pub attrs: [GpioV2LineConfigAttribute; GPIO_V2_LINE_NUM_ATTRS_MAX],
}

/// `struct gpio_v2_line_request`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpioV2LineRequest {
    pub offsets: [u32; MAX_LINES],
    pub consumer: [u8; GPIO_MAX_NAME_SIZE],
    pub config: GpioV2LineConfig,
    pub num_lines: u32,
    pub event_buffer_size: u32,
    pub padding: [u32; 5],
    pub fd: i32,
}

/// `struct gpio_v2_line_values`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GpioV2LineValues {
    pub bits: u64,
    pub mask: u64,
}

/// `struct gpio_v2_line_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpioV2LineInfo {
    pub name: [u8; GPIO_MAX_NAME_SIZE],
    pub consumer: [u8; GPIO_MAX_NAME_SIZE],
    pub offset: u32,
    pub num_attrs: u32,
    pub flags: u64,
    pub attrs: [GpioV2LineAttribute; GPIO_V2_LINE_NUM_ATTRS_MAX],
    pub padding: [u32; 4],
}

const GPIO_GET_CHIPINFO_IOCTL: u32 = ior::<GpioChipInfo>(0xb4, 0x01);
const GPIO_V2_GET_LINEINFO_IOCTL: u32 = iowr::<GpioV2LineInfo>(0xb4, 0x05);
const GPIO_V2_GET_LINE_IOCTL: u32 = iowr::<GpioV2LineRequest>(0xb4, 0x07);
const GPIO_V2_LINE_GET_VALUES_IOCTL: u32 = iowr::<GpioV2LineValues>(0xb4, 0x0e);
const GPIO_V2_LINE_SET_VALUES_IOCTL: u32 = iowr::<GpioV2LineValues>(0xb4, 0x0f);

fn c_string(bytes: &[u8]) -> String {
    CStr::from_bytes_until_nul(bytes)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
}

/// How an output line drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Drive {
    /// Push-pull.
    #[default]
    PushPull,
    /// Open drain.
    OpenDrain,
    /// Open source.
    OpenSource,
}

/// Which line of a chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineId {
    /// By offset on the chip.
    Offset(u32),
    /// By line name (as in the device tree `gpio-line-names`).
    Name(String),
}

/// One output line to request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// Which line.
    pub line: LineId,
    /// Logical value set when the request is made.
    pub initial: bool,
    /// Invert: logical 1 drives the pin low.
    pub active_low: bool,
    /// Output drive.
    pub drive: Drive,
}

impl OutputLine {
    /// A push-pull, active-high output at offset `offset`.
    pub fn offset(offset: u32, initial: bool) -> Self {
        Self {
            line: LineId::Offset(offset),
            initial,
            active_low: false,
            drive: Drive::PushPull,
        }
    }

    /// A push-pull, active-high output by line name.
    pub fn named(name: impl Into<String>, initial: bool) -> Self {
        Self {
            line: LineId::Name(name.into()),
            initial,
            active_low: false,
            drive: Drive::PushPull,
        }
    }

    /// Marks the line active-low.
    pub fn active_low(mut self) -> Self {
        self.active_low = true;
        self
    }

    fn flags(&self) -> u64 {
        let mut flags = GPIO_V2_LINE_FLAG_OUTPUT;
        if self.active_low {
            flags |= GPIO_V2_LINE_FLAG_ACTIVE_LOW;
        }
        flags |= match self.drive {
            Drive::PushPull => 0,
            Drive::OpenDrain => GPIO_V2_LINE_FLAG_OPEN_DRAIN,
            Drive::OpenSource => GPIO_V2_LINE_FLAG_OPEN_SOURCE,
        };
        flags
    }
}

/// Builds a `gpio_v2_line_request` for outputs at resolved `offsets` (same order as `lines`).
///
/// Lines with different flags get per-line flag attributes; initial values go in one
/// output-values attribute.
pub(crate) fn build_output_request(
    consumer: &str,
    lines: &[OutputLine],
    offsets: &[u32],
) -> io::Result<GpioV2LineRequest> {
    if lines.is_empty() || lines.len() > MAX_LINES || lines.len() != offsets.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} lines (1..={MAX_LINES})", lines.len()),
        ));
    }
    let mut req = GpioV2LineRequest {
        offsets: [0; MAX_LINES],
        consumer: [0; GPIO_MAX_NAME_SIZE],
        config: GpioV2LineConfig::default(),
        num_lines: lines.len() as u32,
        event_buffer_size: 0,
        padding: [0; 5],
        fd: -1,
    };
    req.offsets[..offsets.len()].copy_from_slice(offsets);
    let name = consumer.as_bytes();
    let n = name.len().min(GPIO_MAX_NAME_SIZE - 1);
    req.consumer[..n].copy_from_slice(&name[..n]);

    // Default flags: those of the first line; others that differ get an attribute each.
    let base = lines[0].flags();
    req.config.flags = base;
    let mut attrs = Vec::new();
    let mut distinct: Vec<(u64, u64)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let flags = line.flags();
        if flags == base {
            continue;
        }
        match distinct.iter_mut().find(|(f, _)| *f == flags) {
            Some((_, mask)) => *mask |= 1 << i,
            None => distinct.push((flags, 1 << i)),
        }
    }
    for (flags, mask) in distinct {
        attrs.push(GpioV2LineConfigAttribute {
            attr: GpioV2LineAttribute {
                id: GPIO_V2_LINE_ATTR_ID_FLAGS,
                padding: 0,
                value: flags,
            },
            mask,
        });
    }
    let values = lines
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, l)| acc | (u64::from(l.initial) << i));
    let all = if lines.len() == 64 {
        u64::MAX
    } else {
        (1u64 << lines.len()) - 1
    };
    attrs.push(GpioV2LineConfigAttribute {
        attr: GpioV2LineAttribute {
            id: GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES,
            padding: 0,
            value: values,
        },
        mask: all,
    });
    if attrs.len() > GPIO_V2_LINE_NUM_ATTRS_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many distinct line configurations",
        ));
    }
    req.config.num_attrs = attrs.len() as u32;
    req.config.attrs[..attrs.len()].copy_from_slice(&attrs);
    Ok(req)
}

/// What the kernel reports about one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInfo {
    /// Offset on the chip.
    pub offset: u32,
    /// Line name, empty if unnamed.
    pub name: String,
    /// Who holds it, empty if free.
    pub consumer: String,
    /// Whether it is requested by anyone (kernel or userspace).
    pub used: bool,
    /// Configured as output.
    pub output: bool,
    /// Configured as input.
    pub input: bool,
    /// Active-low.
    pub active_low: bool,
}

/// An open GPIO chip.
#[derive(Debug)]
pub struct GpioChip {
    file: File,
    path: PathBuf,
    name: String,
    label: String,
    lines: u32,
}

impl GpioChip {
    /// Opens a chip node, e.g. `/dev/gpiochip0`.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)?;
        // SAFETY: all-zero bytes are a valid `gpiochip_info`.
        let mut info: GpioChipInfo = unsafe { std::mem::zeroed() };
        // SAFETY: GPIO_GET_CHIPINFO_IOCTL fills one `gpiochip_info`, which `info` is.
        unsafe { ioctl(file.as_fd(), GPIO_GET_CHIPINFO_IOCTL, &mut info) }?;
        Ok(Self {
            file,
            path,
            name: c_string(&info.name),
            label: c_string(&info.label),
            lines: info.lines,
        })
    }

    /// Opens the first chip whose label (e.g. `pinctrl-rp1`) matches.
    pub fn open_by_label(label: &str) -> io::Result<Self> {
        for path in chip_paths()? {
            if let Ok(chip) = Self::open(&path)
                && chip.label == label
            {
                return Ok(chip);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no GPIO chip labelled {label}"),
        ))
    }

    /// Node path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Kernel name (`gpiochipN`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Chip label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Number of lines.
    pub fn num_lines(&self) -> u32 {
        self.lines
    }

    /// Information about one line.
    pub fn line_info(&self, offset: u32) -> io::Result<LineInfo> {
        // SAFETY: all-zero bytes are a valid `gpio_v2_line_info`.
        let mut info: GpioV2LineInfo = unsafe { std::mem::zeroed() };
        info.offset = offset;
        // SAFETY: GPIO_V2_GET_LINEINFO_IOCTL reads and fills one `gpio_v2_line_info`.
        unsafe { ioctl(self.file.as_fd(), GPIO_V2_GET_LINEINFO_IOCTL, &mut info) }?;
        Ok(LineInfo {
            offset,
            name: c_string(&info.name),
            consumer: c_string(&info.consumer),
            used: info.flags & GPIO_V2_LINE_FLAG_USED != 0,
            output: info.flags & GPIO_V2_LINE_FLAG_OUTPUT != 0,
            input: info.flags & GPIO_V2_LINE_FLAG_INPUT != 0,
            active_low: info.flags & GPIO_V2_LINE_FLAG_ACTIVE_LOW != 0,
        })
    }

    /// Finds a line by name.
    pub fn find_line(&self, name: &str) -> io::Result<Option<u32>> {
        for offset in 0..self.lines {
            if self.line_info(offset)?.name == name {
                return Ok(Some(offset));
            }
        }
        Ok(None)
    }

    fn resolve(&self, line: &LineId) -> io::Result<u32> {
        match line {
            LineId::Offset(o) if *o < self.lines => Ok(*o),
            LineId::Offset(o) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line {o} beyond {}", self.lines),
            )),
            LineId::Name(n) => self.find_line(n)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no line named {n} on {}", self.name),
                )
            }),
        }
    }

    /// Requests `lines` as outputs for `consumer`. Fails with `EBUSY` if any is already held.
    pub fn request_outputs(&self, consumer: &str, lines: &[OutputLine]) -> io::Result<GpioLines> {
        let offsets = lines
            .iter()
            .map(|l| self.resolve(&l.line))
            .collect::<io::Result<Vec<_>>>()?;
        let mut req = build_output_request(consumer, lines, &offsets)?;
        // SAFETY: GPIO_V2_GET_LINE_IOCTL reads and fills one `gpio_v2_line_request`.
        unsafe { ioctl(self.file.as_fd(), GPIO_V2_GET_LINE_IOCTL, &mut req) }?;
        if req.fd < 0 {
            return Err(io::Error::other("GPIO line request returned no descriptor"));
        }
        // SAFETY: the kernel returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(req.fd) };
        Ok(GpioLines { fd, offsets })
    }
}

/// Lists `/dev/gpiochip*` nodes, sorted.
pub fn chip_paths() -> io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev")?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("gpiochip"))
        .map(|e| e.path())
        .collect();
    paths.sort();
    Ok(paths)
}

/// Requested output lines; released when dropped.
#[derive(Debug)]
pub struct GpioLines {
    fd: OwnedFd,
    offsets: Vec<u32>,
}

/// Bits and mask for setting `values` (by request index); pure.
pub(crate) fn values_for(
    num_lines: usize,
    values: &[(usize, bool)],
) -> io::Result<GpioV2LineValues> {
    let mut v = GpioV2LineValues::default();
    for &(index, value) in values {
        if index >= num_lines {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line index {index} of {num_lines}"),
            ));
        }
        v.mask |= 1 << index;
        if value {
            v.bits |= 1 << index;
        }
    }
    Ok(v)
}

impl GpioLines {
    /// Chip offsets of the lines, in request order.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    fn index_of(&self, offset: u32) -> io::Result<usize> {
        self.offsets
            .iter()
            .position(|&o| o == offset)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("line {offset} not in this request"),
                )
            })
    }

    /// Sets logical values by request index, all at once.
    pub fn set_values(&self, values: &[(usize, bool)]) -> io::Result<()> {
        let mut v = values_for(self.offsets.len(), values)?;
        // SAFETY: GPIO_V2_LINE_SET_VALUES_IOCTL reads one `gpio_v2_line_values`.
        unsafe { ioctl(self.fd.as_fd(), GPIO_V2_LINE_SET_VALUES_IOCTL, &mut v) }?;
        Ok(())
    }

    /// Sets one line (by chip offset) to a logical value.
    pub fn set(&self, offset: u32, value: bool) -> io::Result<()> {
        let index = self.index_of(offset)?;
        self.set_values(&[(index, value)])
    }

    /// Reads back the logical values, bit `i` for request index `i`.
    pub fn values(&self) -> io::Result<u64> {
        let all = if self.offsets.len() == 64 {
            u64::MAX
        } else {
            (1u64 << self.offsets.len()) - 1
        };
        let mut v = GpioV2LineValues { bits: 0, mask: all };
        // SAFETY: GPIO_V2_LINE_GET_VALUES_IOCTL reads and fills one `gpio_v2_line_values`.
        unsafe { ioctl(self.fd.as_fd(), GPIO_V2_LINE_GET_VALUES_IOCTL, &mut v) }?;
        Ok(v.bits)
    }

    /// Releases the lines (same as dropping).
    pub fn release(self) {}
}

impl AsFd for GpioLines {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn struct_layouts_match_the_kernel() {
        assert_eq!(size_of::<GpioChipInfo>(), 68);
        assert_eq!(size_of::<GpioV2LineAttribute>(), 16);
        assert_eq!(size_of::<GpioV2LineConfigAttribute>(), 24);
        assert_eq!(size_of::<GpioV2LineConfig>(), 272);
        assert_eq!(size_of::<GpioV2LineRequest>(), 592);
        assert_eq!(offset_of!(GpioV2LineRequest, fd), 588);
        assert_eq!(size_of::<GpioV2LineValues>(), 16);
        assert_eq!(size_of::<GpioV2LineInfo>(), 256);
        assert_eq!(GPIO_V2_GET_LINE_IOCTL, 0xc250_b407);
        assert_eq!(GPIO_V2_LINE_SET_VALUES_IOCTL, 0xc010_b40f);
        assert_eq!(GPIO_V2_GET_LINEINFO_IOCTL, 0xc100_b405);
    }

    #[test]
    fn builds_output_requests() {
        let lines = [
            OutputLine::offset(5, true),
            OutputLine::named("CAM_PWDN", false).active_low(),
            OutputLine::offset(9, true).active_low(),
        ];
        let req = build_output_request("styx-ov9782", &lines, &[5, 7, 9]).unwrap();
        assert_eq!(&req.offsets[..3], &[5, 7, 9]);
        assert_eq!(req.num_lines, 3);
        assert_eq!(c_string(&req.consumer), "styx-ov9782");
        assert_eq!(req.config.flags, GPIO_V2_LINE_FLAG_OUTPUT);
        assert_eq!(req.config.num_attrs, 2);
        let flags = req.config.attrs[0];
        assert_eq!(flags.attr.id, GPIO_V2_LINE_ATTR_ID_FLAGS);
        assert_eq!(
            flags.attr.value,
            GPIO_V2_LINE_FLAG_OUTPUT | GPIO_V2_LINE_FLAG_ACTIVE_LOW
        );
        assert_eq!(flags.mask, 0b110);
        let values = req.config.attrs[1];
        assert_eq!(values.attr.id, GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES);
        assert_eq!((values.attr.value, values.mask), (0b101, 0b111));
    }

    #[test]
    fn truncates_long_consumers_and_rejects_bad_counts() {
        let req =
            build_output_request(&"x".repeat(40), &[OutputLine::offset(0, false)], &[0]).unwrap();
        assert_eq!(c_string(&req.consumer).len(), 31);
        assert!(build_output_request("c", &[], &[]).is_err());
        assert!(build_output_request("c", &[OutputLine::offset(0, false)], &[0, 1]).is_err());
    }

    #[test]
    fn encodes_values() {
        let v = values_for(3, &[(0, true), (2, false)]).unwrap();
        assert_eq!((v.bits, v.mask), (0b001, 0b101));
        assert!(values_for(2, &[(2, true)]).is_err());
    }

    /// Reads chip and line info from every chip present, read-only. Skips when there are none.
    #[test]
    fn reads_present_chips() {
        let Ok(paths) = chip_paths() else { return };
        for path in paths {
            match GpioChip::open(&path) {
                Ok(chip) => {
                    if chip.num_lines() > 0 {
                        let info = chip.line_info(0).unwrap();
                        assert_eq!(info.offset, 0);
                    }
                }
                Err(e) => eprintln!("skipping {}: {e}", path.display()),
            }
        }
    }
}
