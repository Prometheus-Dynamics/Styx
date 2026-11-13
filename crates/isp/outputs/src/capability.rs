//! What an ISP can make: its outputs, the sizes each reaches and the formats each writes.
//!
//! An ISP with one output (a single pass to one picture) and an ISP with two (output 0 at full
//! size, output 1 at a second size, from the same pass) are both described here. The planner
//! only reads these descriptions; it never runs an ISP.

use styx_format::{FourCc, Resolution};

/// The sizes one output can make, given the size of the capture it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeRule {
    /// Exactly the capture's size divided by `divisor` on each side: 1 is full size, 2 is half.
    Divisor(u32),
    /// Any size from the capture down to the capture divided by `max_divisor` on each side.
    Downscale { max_divisor: u32 },
}

impl SizeRule {
    /// Whether an output with this rule makes `size` from a `capture`.
    pub fn accepts(self, capture: Resolution, size: Resolution) -> bool {
        match self {
            Self::Divisor(divisor) => exact(capture, divisor) == Some(size),
            Self::Downscale { max_divisor } => {
                size.width <= capture.width
                    && size.height <= capture.height
                    && size.width.get().saturating_mul(max_divisor) >= capture.width.get()
                    && size.height.get().saturating_mul(max_divisor) >= capture.height.get()
            }
        }
    }

    /// The size this output makes by default from a `capture`: the exact size for a divisor, the
    /// full capture for a range.
    pub fn default_size(self, capture: Resolution) -> Resolution {
        match self {
            Self::Divisor(divisor) => exact(capture, divisor).unwrap_or(capture),
            Self::Downscale { .. } => capture,
        }
    }
}

/// The capture divided by `divisor` on each side, rounded down; `None` if that is empty.
fn exact(capture: Resolution, divisor: u32) -> Option<Resolution> {
    let divisor = divisor.max(1);
    Resolution::new(
        capture.width.get() / divisor,
        capture.height.get() / divisor,
    )
}

/// One output of an ISP: its size rule and the picture formats it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputCapability {
    pub sizes: SizeRule,
    /// The FourCc picture formats this output writes. Empty: none is exposed yet.
    pub formats: &'static [FourCc],
}

/// An ISP's capability: its outputs, by index (output 0 first).
pub trait IspCapability {
    /// The ISP's name, for refusal reasons.
    fn name(&self) -> &str;

    /// Its outputs, by index.
    fn outputs(&self) -> &[OutputCapability];

    /// How many outputs one pass produces.
    fn max_outputs(&self) -> usize {
        self.outputs().len()
    }
}

/// A capability written out as data: a name and a fixed list of outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Described {
    pub name: &'static str,
    pub outputs: &'static [OutputCapability],
}

impl IspCapability for Described {
    fn name(&self) -> &str {
        self.name
    }

    fn outputs(&self) -> &[OutputCapability] {
        self.outputs
    }
}

/// The PiSP back end as `styx-pisp` exposes it: output 0 at full size, output 1 exactly half
/// size (its 2x2 box). `styx-pisp` names its formats as the library's image configs, not as
/// FourCcs, so no format is listed: it needs to expose one FourCc per output (see the PR).
pub const PISP: Described = Described {
    name: "pisp",
    outputs: &[
        OutputCapability {
            sizes: SizeRule::Divisor(1),
            formats: &[],
        },
        OutputCapability {
            sizes: SizeRule::Divisor(2),
            formats: &[],
        },
    ],
};

/// The GPU ISP as `styx-gpuisp` exposes it: full and half size, with the picture kinds its
/// stage takes (RGB3, GREY, NV12, I420 and YU12). It has no second size range yet.
pub const GPU: Described = Described {
    name: "gpu",
    outputs: &[
        OutputCapability {
            sizes: SizeRule::Divisor(1),
            formats: &PICTURE_FORMATS,
        },
        OutputCapability {
            sizes: SizeRule::Divisor(2),
            formats: &PICTURE_FORMATS,
        },
    ],
};

const PICTURE_FORMATS: [FourCc; 5] = [
    FourCc::RGB3,
    FourCc::GREY,
    FourCc::NV12,
    FourCc::I420,
    FourCc::YU12,
];
