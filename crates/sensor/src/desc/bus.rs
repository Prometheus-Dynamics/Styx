//! The `[bus]` section: how the sensor's data bus is wired, for platforms where no device tree
//! says it (microcontrollers). Linux takes the bus from the device tree and ignores it.

use serde::{Deserialize, Serialize};

/// The `[bus]` section: one of `parallel` or `csi2`.
///
/// ```toml
/// [bus]
/// parallel = { width = 8, pclk_rising = true, hsync_active_high = true, vsync_active_high = false }
/// # or: csi2 = { lanes = 2, continuous_clock = true }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusSection {
    /// A parallel (DVP) port.
    #[serde(default)]
    pub parallel: Option<ParallelBus>,
    /// MIPI CSI-2.
    #[serde(default)]
    pub csi2: Option<Csi2Bus>,
}

/// A parallel (DVP) bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParallelBus {
    /// Data bits (8 to 16).
    pub width: u8,
    /// Data sampled on the rising pixel clock edge (default).
    #[serde(default = "yes")]
    pub pclk_rising: bool,
    /// HSYNC (HREF) active high (default).
    #[serde(default = "yes")]
    pub hsync_active_high: bool,
    /// VSYNC active high (default: no, active low).
    #[serde(default)]
    pub vsync_active_high: bool,
    /// BT.656 embedded sync codes instead of sync lines.
    #[serde(default)]
    pub embedded_sync: bool,
}

/// A MIPI CSI-2 bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Csi2Bus {
    /// Data lanes (1 to 4).
    pub lanes: u8,
    /// Continuous clock (default); else the clock lane may stop between packets.
    #[serde(default = "yes")]
    pub continuous_clock: bool,
    /// Virtual channel of the image data.
    #[serde(default)]
    pub virtual_channel: u8,
}

fn yes() -> bool {
    true
}

impl BusSection {
    /// The bus for `styx-hal`'s receiver configuration; the link frequency comes from the
    /// format (`formats.<name>.link_frequency`, 0 when unknown). `None` without a section.
    pub fn to_hal(&self, link_frequency: Option<u64>) -> Option<styx_hal::Bus> {
        if let Some(p) = self.parallel {
            return Some(styx_hal::Bus::Parallel {
                width: p.width,
                pclk_rising: p.pclk_rising,
                hsync_active_high: p.hsync_active_high,
                vsync_active_high: p.vsync_active_high,
                embedded_sync: p.embedded_sync,
            });
        }
        self.csi2.map(|c| styx_hal::Bus::Csi2 {
            lanes: c.lanes,
            link_frequency: link_frequency.unwrap_or(0),
            continuous_clock: c.continuous_clock,
            virtual_channel: c.virtual_channel,
        })
    }

    /// Problems with the section, as `(path, message)`.
    pub(super) fn problems(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        let both = self.parallel.is_some() && self.csi2.is_some();
        let none = self.parallel.is_none() && self.csi2.is_none();
        let width = self.parallel.is_some_and(|p| !(8..=16).contains(&p.width));
        let lanes = self.csi2.is_some_and(|c| !(1..=4).contains(&c.lanes));
        let vc = self.csi2.is_some_and(|c| c.virtual_channel > 15);
        [
            (both, ("bus", "give either parallel or csi2, not both")),
            (none, ("bus", "needs parallel or csi2")),
            (width, ("bus.parallel.width", "must be 8 to 16 bits")),
            (lanes, ("bus.csi2.lanes", "must be 1 to 4")),
            (vc, ("bus.csi2.virtual_channel", "must be 0 to 15")),
        ]
        .into_iter()
        .filter(|(bad, _)| *bad)
        .map(|(_, p)| p)
    }
}
