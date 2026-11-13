//! The `isp.isp600/1` contract's numbers, answered from the Lemnos library's tables.

use lemnos_contract_isp_isp600::{Isp600Info, Region as WireRegion};

use crate::library::layout::{
    LAYOUT_VERSION, LOAD_BYTES, LOAD_REGIONS, Region, STATISTICS_BYTES, STATISTICS_REGIONS,
};

/// The block's description: the library's layout version, sizes and region counts, with the
/// settle frames the caller's driver uses (a driver setting, not a layout fact).
#[must_use]
pub fn info(settle_frames: u8) -> Isp600Info {
    Isp600Info {
        layout_version: LAYOUT_VERSION,
        load_bytes: LOAD_BYTES as u32,
        statistics_bytes: STATISTICS_BYTES as u32,
        load_regions: LOAD_REGIONS.len() as u8,
        statistics_regions: STATISTICS_REGIONS.len() as u8,
        settle_frames,
    }
}

/// Where load region `region` lies, or `None` past the last one.
#[must_use]
pub fn load_region(region: u8) -> Option<WireRegion> {
    LOAD_REGIONS.get(usize::from(region)).copied().map(wire)
}

/// Where statistics region `region` lies, or `None` past the last one.
#[must_use]
pub fn statistics_region(region: u8) -> Option<WireRegion> {
    STATISTICS_REGIONS
        .get(usize::from(region))
        .copied()
        .map(wire)
}

fn wire(region: Region) -> WireRegion {
    WireRegion {
        // Every size is below 2^32 (`LOAD_BYTES` is 0x7700); the library's compile-time checks
        // keep the regions inside the buffers, so the casts lose no bits.
        offset: region.offset as u32,
        bytes: region.bytes as u32,
    }
}
