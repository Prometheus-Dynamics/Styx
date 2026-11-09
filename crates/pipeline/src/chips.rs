//! What Styx has for each chip family, as data: which blocks exist, and what each is waiting
//! on. Boards are listed under their chip; nothing is per board. See `docs/boards.md`.
//!
//! No entry here has run on its board unless it says so.

/// The chip's image signal processor, as far as Styx can drive it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IspSupport {
    /// `styx-rkisp` has this generation's parameters and statistics (written, untested on board).
    Rkisp(&'static str),
    /// A generation or block Styx cannot drive yet: cameras use the software ISP.
    Missing(&'static str),
    /// `styx-k230isp` has the registers; the frame path is missing (software ISP until then).
    K230,
}

/// How the chip encodes video without the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderSupport {
    /// Rockchip MPP (`codec-rkmpp`), when the vendor library and `/dev/mpp_service` are present.
    Mpp,
    /// A stateful V4L2 encoder (`V4l2M2mEncoder`).
    V4l2M2m,
    /// Not written.
    Missing(&'static str),
}

/// The chip's 2D scaler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalerSupport {
    /// Rockchip RGA (`codec-rga`): format conversion and the planner's scaling stage.
    Rga,
    /// A V4L2 M2M scaler (`V4l2M2mConverter`).
    V4l2M2m,
    /// Not written.
    Missing(&'static str),
}

/// One chip family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chip {
    /// The family's name.
    pub name: &'static str,
    /// The devicetree compatible strings naming it; empty where the board's are not known
    /// (see [`Chip::isp`] for how the block is found instead).
    pub compatibles: &'static [&'static str],
    /// The boards Styx is meant to run on.
    pub boards: &'static [&'static str],
    /// Its ISP.
    pub isp: IspSupport,
    /// Its video encoder.
    pub encoder: EncoderSupport,
    /// Its 2D scaler.
    pub scaler: ScalerSupport,
}

/// Every chip family on the board list.
pub const CHIPS: &[Chip] = &[
    Chip {
        name: "RK3588S",
        compatibles: &["rockchip,rk3588s", "rockchip,rk3588"],
        boards: &["ROCK 5C (RK3588S2)"],
        isp: IspSupport::Rkisp("ISP30"),
        encoder: EncoderSupport::Mpp,
        scaler: ScalerSupport::Rga,
    },
    Chip {
        name: "RK3566",
        compatibles: &["rockchip,rk3566", "rockchip,rk3568"],
        boards: &["ZERO 3E"],
        isp: IspSupport::Rkisp("ISP21"),
        encoder: EncoderSupport::Mpp,
        scaler: ScalerSupport::Rga,
    },
    Chip {
        name: "RV1126B",
        compatibles: &["rockchip,rv1126b"],
        boards: &["Luckfox Aura"],
        isp: IspSupport::Missing("ISP35: its layouts are not in lemnos-rkisp-sys"),
        encoder: EncoderSupport::Mpp,
        scaler: ScalerSupport::Rga,
    },
    Chip {
        name: "A733",
        compatibles: &["allwinner,a733"],
        boards: &["Cubie A7A", "Cubie A7S", "Cubie A7Z"],
        isp: IspSupport::Missing("ISP600 behind sunxi-vin, 3A in a closed library"),
        encoder: EncoderSupport::Missing("cedar VE: no V4L2 device"),
        scaler: ScalerSupport::Missing("G2D: not a V4L2 device"),
    },
    Chip {
        name: "K230D",
        compatibles: &[],
        boards: &["K230D AI-CAM"],
        isp: IspSupport::K230,
        encoder: EncoderSupport::V4l2M2m,
        scaler: ScalerSupport::V4l2M2m,
    },
];

/// The family one devicetree compatible string names.
pub fn from_compatible(compatible: &str) -> Option<&'static Chip> {
    CHIPS.iter().find(|c| c.compatibles.contains(&compatible))
}

/// This machine's family, from `/proc/device-tree/compatible`.
#[cfg(feature = "std")]
pub fn detect() -> Option<&'static Chip> {
    let bytes = std::fs::read("/proc/device-tree/compatible").ok()?;
    bytes
        .split(|&b| b == 0)
        .filter_map(|s| core::str::from_utf8(s).ok())
        .find_map(from_compatible)
        .or_else(|| {
            // The K230D's compatible strings are not known: its ISP's node names it.
            std::path::Path::new("/dev/vvcam-isp.0")
                .exists()
                .then(|| CHIPS.iter().find(|c| c.isp == IspSupport::K230))
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_board_on_the_list_has_a_family() {
        let boards: Vec<_> = CHIPS.iter().flat_map(|c| c.boards).collect();
        assert_eq!(boards.len(), 7);
        assert_eq!(from_compatible("allwinner,a733").unwrap().name, "A733");
        assert_eq!(from_compatible("rockchip,rv1126b").unwrap().name, "RV1126B");
        assert!(from_compatible("brcm,bcm2712").is_none());
    }

    /// The table's Rockchip entries say what the ISP code does: a generation Styx has is one
    /// Lemnos detects, and a missing one is not, so its cameras fall back to the software ISP.
    #[cfg(feature = "rkisp")]
    #[test]
    fn rockchip_entries_agree_with_detection() {
        for chip in CHIPS {
            for compatible in chip
                .compatibles
                .iter()
                .filter(|c| c.starts_with("rockchip,"))
            {
                let detected = styx_rkisp::Isp::from_soc(compatible).is_some();
                assert_eq!(
                    detected,
                    matches!(chip.isp, IspSupport::Rkisp(_)),
                    "{compatible}"
                );
            }
        }
    }
}
