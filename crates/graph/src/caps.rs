//! Port capabilities: formats, sizes, frame intervals, memory domains and cost hints.

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, BitOr};

/// A V4L2 pixel format code (four ASCII characters, little-endian), the format of frames in
/// memory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FourCc(pub u32);

impl FourCc {
    pub const GREY: FourCc = FourCc::new(b"GREY");
    pub const Y10: FourCc = FourCc::new(b"Y10 ");
    pub const NV12: FourCc = FourCc::new(b"NV12");
    pub const YU12: FourCc = FourCc::new(b"YU12");
    pub const YUYV: FourCc = FourCc::new(b"YUYV");
    pub const UYVY: FourCc = FourCc::new(b"UYVY");
    pub const RGB3: FourCc = FourCc::new(b"RGB3");
    pub const BGR3: FourCc = FourCc::new(b"BGR3");
    pub const MJPG: FourCc = FourCc::new(b"MJPG");
    pub const H264: FourCc = FourCc::new(b"H264");
    /// 10-bit Bayer GRBG, unpacked to 16 bits.
    pub const SGRBG10: FourCc = FourCc::new(b"BA10");
    /// 10-bit Bayer RGGB, unpacked to 16 bits.
    pub const SRGGB10: FourCc = FourCc::new(b"RG10");
    /// Raspberry Pi PiSP compressed raw.
    pub const PISP_COMP1_RGGB: FourCc = FourCc::new(b"PC1R");
    /// Raspberry Pi PiSP front end statistics buffer.
    pub const PISP_FE_STATS: FourCc = FourCc::new(b"RPFS");
    /// Raspberry Pi PiSP back end configuration buffer.
    pub const PISP_BE_CONFIG: FourCc = FourCc::new(b"RPBC");

    /// The code for four ASCII characters.
    pub const fn new(code: &[u8; 4]) -> Self {
        FourCc(u32::from_le_bytes(*code))
    }

    pub const fn bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Bytes of one tightly packed frame, for the formats whose size depends only on the
    /// dimensions.
    pub fn frame_bytes(self, width: u32, height: u32) -> Option<usize> {
        let pixels = width as usize * height as usize;
        match self {
            FourCc::GREY => Some(pixels),
            FourCc::NV12 | FourCc::YU12 => Some(pixels * 3 / 2),
            FourCc::YUYV | FourCc::UYVY | FourCc::Y10 | FourCc::SGRBG10 | FourCc::SRGGB10 => {
                Some(pixels * 2)
            }
            FourCc::RGB3 | FourCc::BGR3 => Some(pixels * 3),
            _ => None,
        }
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.bytes() {
            let c = if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            };
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCc({self})")
    }
}

/// A media bus code (`MEDIA_BUS_FMT_*`), the format of pixels on a link between hardware
/// blocks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BusCode(pub u32);

impl BusCode {
    pub const FIXED: BusCode = BusCode(0x0001);
    pub const Y8_1X8: BusCode = BusCode(0x2001);
    pub const Y10_1X10: BusCode = BusCode(0x200a);
    pub const UYVY8_1X16: BusCode = BusCode(0x200f);
    pub const SBGGR10_1X10: BusCode = BusCode(0x3007);
    pub const SGRBG10_1X10: BusCode = BusCode(0x300a);
    pub const SGBRG10_1X10: BusCode = BusCode(0x300e);
    pub const SRGGB10_1X10: BusCode = BusCode(0x300f);
    pub const METADATA_FIXED: BusCode = BusCode(0x7001);
}

/// A format on a port: in memory (pixel format) or on a bus (media bus code).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FormatCode {
    Memory(FourCc),
    Bus(BusCode),
}

impl From<FourCc> for FormatCode {
    fn from(code: FourCc) -> Self {
        FormatCode::Memory(code)
    }
}

impl From<BusCode> for FormatCode {
    fn from(code: BusCode) -> Self {
        FormatCode::Bus(code)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Size { width, height }
    }

    pub fn megapixels(self) -> f32 {
        self.width as f32 * self.height as f32 / 1_000_000.0
    }
}

/// Sizes a port supports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SizeRange {
    Discrete(Size),
    /// Every size from `min` to `max` in multiples of `step` above `min` (a step of 0 or 1 means
    /// any size in range).
    Stepwise {
        min: Size,
        max: Size,
        step: Size,
    },
}

impl SizeRange {
    pub const fn discrete(width: u32, height: u32) -> Self {
        SizeRange::Discrete(Size::new(width, height))
    }

    pub const fn stepwise(min: Size, max: Size, step: Size) -> Self {
        SizeRange::Stepwise { min, max, step }
    }

    pub fn contains(&self, size: Size) -> bool {
        match *self {
            SizeRange::Discrete(s) => s == size,
            SizeRange::Stepwise { min, max, step } => {
                on_step(size.width, min.width, max.width, step.width)
                    && on_step(size.height, min.height, max.height, step.height)
            }
        }
    }

    /// The largest size in the range.
    pub fn max(&self) -> Size {
        match *self {
            SizeRange::Discrete(s) => s,
            SizeRange::Stepwise { max, .. } => max,
        }
    }
}

fn on_step(value: u32, min: u32, max: u32, step: u32) -> bool {
    (min..=max).contains(&value) && (step <= 1 || (value - min).is_multiple_of(step))
}

/// A frame interval in seconds (`num / den`); 1/30 is 30 frames per second.
#[derive(Clone, Copy, Debug, Eq)]
pub struct Fraction {
    pub num: u32,
    pub den: u32,
}

impl Fraction {
    pub const fn new(num: u32, den: u32) -> Self {
        Fraction { num, den }
    }

    /// The interval of `fps` frames per second.
    pub const fn from_fps(fps: u32) -> Self {
        Fraction { num: 1, den: fps }
    }

    pub fn fps(self) -> f64 {
        if self.num == 0 {
            return 0.0;
        }
        self.den as f64 / self.num as f64
    }

    pub fn as_duration(self) -> std::time::Duration {
        if self.den == 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_nanos(self.num as u64 * 1_000_000_000 / self.den as u64)
    }
}

impl PartialEq for Fraction {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd for Fraction {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Fraction {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.num as u64 * other.den as u64).cmp(&(other.num as u64 * self.den as u64))
    }
}

/// Frame intervals a port supports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntervalRange {
    Discrete(Fraction),
    /// Any interval from `min` (fastest) to `max` (slowest). Sensors driven by Styx can reach
    /// any interval their timing allows, so the step is informational.
    Stepwise {
        min: Fraction,
        max: Fraction,
        step: Fraction,
    },
}

impl IntervalRange {
    /// Every rate from `min_fps` to `max_fps`.
    pub const fn fps(min_fps: u32, max_fps: u32) -> Self {
        IntervalRange::Stepwise {
            min: Fraction::from_fps(max_fps),
            max: Fraction::from_fps(min_fps),
            step: Fraction::new(0, 1),
        }
    }

    pub fn contains(&self, interval: Fraction) -> bool {
        match *self {
            IntervalRange::Discrete(i) => i == interval,
            IntervalRange::Stepwise { min, max, .. } => min <= interval && interval <= max,
        }
    }
}

/// One format a port supports, with the sizes and intervals it supports in that format. Empty
/// `sizes` or `intervals` mean "not constrained here".
#[derive(Clone, PartialEq, Debug)]
pub struct FormatCaps {
    pub code: FormatCode,
    pub sizes: Vec<SizeRange>,
    pub intervals: Vec<IntervalRange>,
}

impl FormatCaps {
    pub fn new(code: impl Into<FormatCode>) -> Self {
        FormatCaps {
            code: code.into(),
            sizes: Vec::new(),
            intervals: Vec::new(),
        }
    }

    pub fn size(mut self, range: SizeRange) -> Self {
        self.sizes.push(range);
        self
    }

    pub fn interval(mut self, range: IntervalRange) -> Self {
        self.intervals.push(range);
        self
    }

    pub fn accepts_size(&self, size: Size) -> bool {
        self.sizes.is_empty() || self.sizes.iter().any(|r| r.contains(size))
    }

    pub fn accepts_interval(&self, interval: Fraction) -> bool {
        self.intervals.is_empty() || self.intervals.iter().any(|r| r.contains(interval))
    }
}

/// Where a buffer can live. A set of domains, combined with `|`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MemoryDomains(u8);

impl MemoryDomains {
    pub const NONE: MemoryDomains = MemoryDomains(0);
    /// Mapped into the process (mmap'd V4L2 buffers, memfd, heap).
    pub const CPU: MemoryDomains = MemoryDomains(1);
    /// Exportable/importable dma-buf file descriptors.
    pub const DMABUF: MemoryDomains = MemoryDomains(1 << 1);
    /// GPU-local memory (Vulkan/GL images).
    pub const GPU: MemoryDomains = MemoryDomains(1 << 2);
    /// NPU-local memory.
    pub const NPU: MemoryDomains = MemoryDomains(1 << 3);

    pub const fn contains(self, other: MemoryDomains) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: MemoryDomains) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for MemoryDomains {
    type Output = MemoryDomains;
    fn bitor(self, rhs: Self) -> Self {
        MemoryDomains(self.0 | rhs.0)
    }
}

impl fmt::Debug for MemoryDomains {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (Self::CPU, "CPU"),
            (Self::DMABUF, "DMABUF"),
            (Self::GPU, "GPU"),
            (Self::NPU, "NPU"),
        ];
        let set: Vec<&str> = names
            .iter()
            .filter(|(d, _)| self.contains(*d))
            .map(|(_, n)| *n)
            .collect();
        write!(f, "MemoryDomains({})", set.join(" | "))
    }
}

/// What a port can carry.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Capabilities {
    pub formats: Vec<FormatCaps>,
    /// Memory domains for ports that read or write memory; empty for on-chip bus ports.
    pub memory: MemoryDomains,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn format(mut self, caps: FormatCaps) -> Self {
        self.formats.push(caps);
        self
    }

    pub fn memory(mut self, domains: MemoryDomains) -> Self {
        self.memory = domains;
        self
    }

    pub fn find(&self, code: FormatCode) -> Option<&FormatCaps> {
        self.formats.iter().find(|f| f.code == code)
    }

    /// Whether this port carries `code` at `size` (and `interval` when given).
    pub fn accepts(&self, code: FormatCode, size: Size, interval: Option<Fraction>) -> bool {
        self.formats.iter().any(|f| {
            f.code == code && f.accepts_size(size) && interval.is_none_or(|i| f.accepts_interval(i))
        })
    }

    /// Whether two ports on either end of a link can agree on a format. Ports in different
    /// namespaces (bus codes on one side, memory formats on the other) or without declared
    /// formats are not compared.
    pub fn compatible(&self, other: &Capabilities) -> bool {
        let comparable = self
            .formats
            .iter()
            .any(|a| other.formats.iter().any(|b| same_namespace(a.code, b.code)));
        !comparable
            || self
                .formats
                .iter()
                .any(|a| other.formats.iter().any(|b| a.code == b.code))
    }
}

fn same_namespace(a: FormatCode, b: FormatCode) -> bool {
    matches!(
        (a, b),
        (FormatCode::Memory(_), FormatCode::Memory(_)) | (FormatCode::Bus(_), FormatCode::Bus(_))
    )
}

/// Estimated cost of one frame through a node, in milliseconds.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Cost {
    /// Time added between exposure and delivery.
    pub latency_ms: f32,
    /// Host CPU time spent.
    pub cpu_ms: f32,
}

impl Cost {
    pub const ZERO: Cost = Cost {
        latency_ms: 0.0,
        cpu_ms: 0.0,
    };

    pub const fn new(latency_ms: f32, cpu_ms: f32) -> Self {
        Cost { latency_ms, cpu_ms }
    }

    fn scale(self, factor: f32) -> Cost {
        Cost::new(self.latency_ms * factor, self.cpu_ms * factor)
    }
}

impl Add for Cost {
    type Output = Cost;
    fn add(self, rhs: Cost) -> Cost {
        Cost::new(self.latency_ms + rhs.latency_ms, self.cpu_ms + rhs.cpu_ms)
    }
}

/// How much a node costs per frame: a fixed part plus a part proportional to the frame's
/// megapixels (the same model as the planner's `StepCost` constants).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct CostHint {
    pub fixed: Cost,
    pub per_megapixel: Cost,
}

impl CostHint {
    pub const FREE: CostHint = CostHint {
        fixed: Cost::ZERO,
        per_megapixel: Cost::ZERO,
    };

    pub const fn fixed(latency_ms: f32, cpu_ms: f32) -> Self {
        CostHint {
            fixed: Cost::new(latency_ms, cpu_ms),
            per_megapixel: Cost::ZERO,
        }
    }

    pub const fn per_megapixel(latency_ms: f32, cpu_ms: f32) -> Self {
        CostHint {
            fixed: Cost::ZERO,
            per_megapixel: Cost::new(latency_ms, cpu_ms),
        }
    }

    pub fn estimate(&self, size: Size) -> Cost {
        self.fixed + self.per_megapixel.scale(size.megapixels())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_round_trips_and_displays() {
        assert_eq!(FourCc::NV12.bytes(), *b"NV12");
        assert_eq!(FourCc::Y10.to_string(), "Y10 ");
        assert_eq!(format!("{:?}", FourCc(0x0000_0001)), "FourCc(....)");
        assert_eq!(FourCc::NV12.frame_bytes(4, 2), Some(12));
        assert_eq!(FourCc::MJPG.frame_bytes(4, 2), None);
    }

    #[test]
    fn stepwise_sizes_respect_steps() {
        let range = SizeRange::stepwise(Size::new(64, 64), Size::new(4096, 3072), Size::new(2, 2));
        assert!(range.contains(Size::new(1280, 800)));
        assert!(!range.contains(Size::new(1281, 800)));
        assert!(!range.contains(Size::new(8192, 800)));
        let any = SizeRange::stepwise(Size::new(1, 1), Size::new(100, 100), Size::new(1, 0));
        assert!(any.contains(Size::new(37, 99)));
        assert!(SizeRange::discrete(640, 480).contains(Size::new(640, 480)));
        assert_eq!(range.max(), Size::new(4096, 3072));
    }

    #[test]
    fn fractions_compare_by_value() {
        assert_eq!(Fraction::new(2, 60), Fraction::from_fps(30));
        assert!(Fraction::from_fps(60) < Fraction::from_fps(30));
        assert!((Fraction::new(1001, 30000).fps() - 29.97).abs() < 0.01);
        assert_eq!(
            Fraction::from_fps(50).as_duration(),
            std::time::Duration::from_millis(20)
        );
        assert_eq!(Fraction::new(0, 1).fps(), 0.0);
    }

    #[test]
    fn interval_ranges() {
        let range = IntervalRange::fps(1, 120);
        assert!(range.contains(Fraction::from_fps(60)));
        assert!(range.contains(Fraction::new(1001, 30000)));
        assert!(!range.contains(Fraction::from_fps(240)));
        assert!(IntervalRange::Discrete(Fraction::from_fps(30)).contains(Fraction::new(2, 60)));
    }

    #[test]
    fn capabilities_accept_and_match() {
        let caps = Capabilities::new()
            .format(
                FormatCaps::new(FourCc::NV12)
                    .size(SizeRange::discrete(1280, 720))
                    .interval(IntervalRange::fps(1, 30)),
            )
            .memory(MemoryDomains::CPU | MemoryDomains::DMABUF);
        let nv12 = FormatCode::from(FourCc::NV12);
        assert!(caps.accepts(nv12, Size::new(1280, 720), Some(Fraction::from_fps(30))));
        assert!(caps.accepts(nv12, Size::new(1280, 720), None));
        assert!(!caps.accepts(nv12, Size::new(1280, 720), Some(Fraction::from_fps(60))));
        assert!(!caps.accepts(FourCc::GREY.into(), Size::new(1280, 720), None));
        assert!(caps.find(nv12).is_some());

        let bus = Capabilities::new().format(FormatCaps::new(BusCode::SGRBG10_1X10));
        let other_bus = Capabilities::new().format(FormatCaps::new(BusCode::Y10_1X10));
        assert!(
            caps.compatible(&bus),
            "different namespaces are not compared"
        );
        assert!(!bus.compatible(&other_bus));
        assert!(bus.compatible(&bus.clone()));
        assert!(Capabilities::new().compatible(&bus));
    }

    #[test]
    fn memory_domains_combine() {
        let both = MemoryDomains::CPU | MemoryDomains::DMABUF;
        assert!(both.contains(MemoryDomains::CPU));
        assert!(!both.contains(MemoryDomains::GPU));
        assert!(both.intersects(MemoryDomains::DMABUF | MemoryDomains::NPU));
        assert!(MemoryDomains::NONE.is_empty());
        assert_eq!(format!("{both:?}"), "MemoryDomains(CPU | DMABUF)");
    }

    #[test]
    fn cost_scales_with_megapixels() {
        let hint = CostHint {
            fixed: Cost::new(1.0, 0.0),
            per_megapixel: Cost::new(2.0, 1.0),
        };
        let cost = hint.estimate(Size::new(1000, 1000));
        assert_eq!(cost, Cost::new(3.0, 1.0));
        assert_eq!(CostHint::FREE.estimate(Size::new(10, 10)), Cost::ZERO);
        assert_eq!(
            CostHint::fixed(8.0, 0.1)
                .estimate(Size::default())
                .latency_ms,
            8.0
        );
        assert_eq!(
            CostHint::per_megapixel(1.5, 0.3).estimate(Size::new(2000, 500)),
            Cost::new(1.5, 0.3)
        );
    }
}
