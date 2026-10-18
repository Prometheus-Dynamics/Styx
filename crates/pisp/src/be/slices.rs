//! A frame's back end job split at a line, for processing the top of a frame while the bottom
//! is still arriving: with tiles in horizontal stripes ([`BackEnd::set_max_stripe_height`]), the
//! tiles whose input ends above the line run as one job and the rest as another, both into the
//! same output buffers (each tile writes at its own offsets).
//!
//! [`BackEnd::set_max_stripe_height`]: super::BackEnd::set_max_stripe_height

use crate::uapi::{BE_NUM_OUTPUTS, BeTilesConfig, Tile};

/// A prepared config split in two at an input line.
#[derive(Clone, Debug)]
pub struct SplitJobs {
    /// The tiles whose input rows (context included) end at or above the line.
    pub first: BeTilesConfig,
    /// The other tiles.
    pub rest: BeTilesConfig,
    /// Rows of each output final after `first` (every row above the first tile of `rest`):
    /// what a consumer may read once `first` is done.
    pub rows_after_first: [u32; BE_NUM_OUTPUTS],
}

fn input_end(t: &Tile) -> u32 {
    u32::from(t.input_offset_y) + u32::from(t.input_height)
}

/// Splits `cfg` (prepared with stripes) between the tiles whose input ends by input row `line`
/// and the rest. `None` when either part would be empty (the line is above the first stripe's
/// input, or below the whole frame): the frame then runs as one job.
pub fn split_tiles(cfg: &BeTilesConfig, line: u32) -> Option<SplitJobs> {
    let (mut first, mut rest) = (*cfg, *cfg);
    let rows_after_first = split_tiles_into(cfg, line, &mut first, &mut rest)?;
    Some(SplitJobs {
        first,
        rest,
        rows_after_first,
    })
}

/// [`split_tiles`] written into `first` and `rest` (both overwritten: whole configs, the
/// tiles of each part first), so a caller can keep them and split every frame without
/// allocating. Returns the rows of each output final after `first`; `None` as
/// [`split_tiles`] (then `first` and `rest` are not meaningful).
pub fn split_tiles_into(
    cfg: &BeTilesConfig,
    line: u32,
    first: &mut BeTilesConfig,
    rest: &mut BeTilesConfig,
) -> Option<[u32; BE_NUM_OUTPUTS]> {
    let tiles = &cfg.tiles[..cfg.num_tiles as usize];
    *first = *cfg;
    *rest = *cfg;
    let (mut nf, mut nr) = (0usize, 0usize);
    let mut rows = [u32::MAX; BE_NUM_OUTPUTS];
    for t in tiles {
        if input_end(t) <= line {
            first.tiles[nf] = *t;
            nf += 1;
        } else {
            rest.tiles[nr] = *t;
            nr += 1;
            for (i, r) in rows.iter_mut().enumerate() {
                if t.output_width[i] > 0 && t.output_height[i] > 0 {
                    *r = (*r).min(u32::from(t.output_offset_y[i]));
                }
            }
        }
    }
    if nf == 0 || nr == 0 {
        return None;
    }
    first.num_tiles = nf as u32;
    rest.num_tiles = nr as u32;
    // An output no tile of `rest` writes is complete after `first`.
    for (i, r) in rows.iter_mut().enumerate() {
        if *r == u32::MAX {
            *r = tiles
                .iter()
                .map(|t| u32::from(t.output_offset_y[i]) + u32::from(t.output_height[i]))
                .max()
                .unwrap_or(0);
        }
    }
    Some(rows)
}
