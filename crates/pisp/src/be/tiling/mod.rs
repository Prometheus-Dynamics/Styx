//! Back end tiling: splits the image into tiles no wider than the hardware allows (640 pixels
//! on BCM2712), with the context pixels each processing stage needs, so that every output is
//! written exactly once.
//!
//! A Rust port of libpisp `src/libpisp/backend/tiling/` (`pipeline.cpp`, `stages.cpp`,
//! `input_stage.cpp`, `context_stage.cpp`, `split_stage.cpp`, `crop_stage.cpp`,
//! `rescale_stage.cpp`, `output_stage.cpp`, `pisp_tiling.cpp`; BSD-2-Clause, Copyright (C)
//! 2021 - 2023, Raspberry Pi Ltd). The stage graph is an arena of stages linked by index
//! instead of C++ objects linked by pointer; the algorithm (push the next output start up the
//! pipeline, push the widest input end down, trim back up, then push the crops down) is
//! unchanged. See libpisp's `tiling/README.txt`.

mod types;

pub use types::*;
use types::{Result, check};

const PIPELINE_CONTEXT: i32 = 16;
const PIPELINE_ALIGN: i32 = 2;
const COMPRESSION_ALIGN: i32 = 8;
const START_CONTEXT: i32 = 2;
const END_CONTEXT: i32 = 3;
const SCALE_PRECISION: u32 = 12;
const ROUND_UP: i32 = (1 << SCALE_PRECISION) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Input,
    Context,
    Crop(usize),
    Downscale(usize),
    Resample(usize),
    Output(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RescaleKind {
    Downscaler,
    Resampler,
}

#[derive(Clone, Debug)]
enum Kind {
    Input {
        size: Length2,
        align: Length2,
        compression_align: i32,
    },
    Context {
        context: i32,
        align: Length2,
    },
    Split {
        downstream: Vec<usize>,
        count: usize,
    },
    Crop {
        crop: Interval2,
    },
    Rescale {
        out_size: Length2,
        scale: Length2,
        start_context: Length2,
        end_context: Length2,
        kind: RescaleKind,
    },
    Output {
        max_align: Length2,
        min_align: Length2,
        x_mirrored: bool,
        complete: bool,
    },
}

#[derive(Clone, Debug)]
struct Stage {
    kind: Kind,
    slot: Option<Slot>,
    upstream: Option<usize>,
    downstream: Option<usize>,
    input: Interval,
    crop: Crop,
    output: Interval,
}

struct Pipeline {
    stages: Vec<Stage>,
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    max_tile: Length2,
    min_tile: Length2,
    first_tile: bool,
}

fn crop_axis(i: Interval2, d: Dir) -> Interval {
    match d {
        Dir::X => i.x,
        Dir::Y => i.y,
    }
}

impl Pipeline {
    fn add(&mut self, kind: Kind, slot: Option<Slot>, upstream: Option<usize>) -> usize {
        let id = self.stages.len();
        self.stages.push(Stage {
            kind,
            slot,
            upstream,
            downstream: None,
            input: Interval::default(),
            crop: Crop::default(),
            output: Interval::default(),
        });
        if let Some(u) = upstream {
            match &mut self.stages[u].kind {
                Kind::Split { downstream, .. } => downstream.push(id),
                _ => self.stages[u].downstream = Some(id),
            }
        }
        match self.stages[id].kind {
            Kind::Input { .. } => self.inputs.push(id),
            Kind::Output { .. } => self.outputs.push(id),
            _ => {}
        }
        id
    }

    fn input_size(&self, s: usize) -> Length2 {
        match self.stages[s].kind {
            Kind::Input { size, .. } => size,
            _ => self.output_size(self.stages[s].upstream.expect("upstream")),
        }
    }

    fn output_size(&self, s: usize) -> Length2 {
        match self.stages[s].kind {
            Kind::Crop { crop } => Length2::new(crop.x.length, crop.y.length),
            Kind::Rescale { out_size, .. } => out_size,
            _ => self.input_size(s),
        }
    }

    fn down(&self, s: usize) -> usize {
        self.stages[s].downstream.expect("downstream stage")
    }

    fn basic_reset(&mut self, s: usize) {
        let st = &mut self.stages[s];
        st.input = Interval::default();
        st.crop = Crop::default();
        st.output = Interval::default();
    }

    fn reset(&mut self, s: usize) {
        self.basic_reset(s);
        match &mut self.stages[s].kind {
            Kind::Output { complete, .. } => *complete = false,
            Kind::Split { count, .. } => *count = 0,
            _ => {}
        }
    }

    fn branch_complete(&self, s: usize) -> bool {
        match &self.stages[s].kind {
            Kind::Output { complete, .. } => *complete,
            Kind::Split { downstream, .. } => downstream.iter().all(|&d| self.branch_complete(d)),
            _ => self.branch_complete(self.down(s)),
        }
    }

    fn branch_inactive(&self, s: usize) -> bool {
        match self.stages[s].kind {
            Kind::Crop { .. } => self.stages[s].output.length == 0,
            _ => self.stages[s]
                .upstream
                .is_some_and(|u| self.branch_inactive(u)),
        }
    }

    fn push_start_up(&mut self, s: usize, out_start: i32, d: Dir) -> Result<()> {
        let up = self.stages[s].upstream;
        let first_tile = self.first_tile;
        let kind = self.stages[s].kind.clone();
        let in_start = match kind {
            Kind::Input { align, .. } => {
                let st = &mut self.stages[s];
                st.output.offset = out_start;
                st.input.offset = out_start - out_start % align.get(d);
                return Ok(());
            }
            Kind::Context { context, align } => {
                let mut i = (out_start - context).max(0);
                i -= i % align.get(d);
                i
            }
            Kind::Split { .. } => {
                let incomplete = match &self.stages[s].kind {
                    Kind::Split { downstream, .. } => downstream
                        .iter()
                        .filter(|&&x| !self.branch_complete(x))
                        .count(),
                    _ => unreachable!(),
                };
                let st = &mut self.stages[s];
                let Kind::Split { count, .. } = &mut st.kind else {
                    unreachable!()
                };
                if *count == 0 {
                    st.input = Interval::new(out_start, 0);
                } else {
                    st.input.include(out_start);
                }
                *count += 1;
                if *count == incomplete {
                    *count = 0;
                    let start = st.input.offset;
                    return self.push_start_up(up.expect("upstream"), start, d);
                }
                return Ok(());
            }
            Kind::Crop { crop } => {
                let i = out_start + crop_axis(crop, d).offset;
                check(i >= 0, "crop input start is negative")?;
                i
            }
            Kind::Rescale {
                scale,
                start_context,
                ..
            } => {
                let i = ((out_start * scale.get(d)) >> SCALE_PRECISION) - start_context.get(d);
                if first_tile && i < 0 { 0 } else { i }
            }
            Kind::Output { .. } => out_start,
        };
        let st = &mut self.stages[s];
        st.output.offset = out_start;
        st.input.offset = in_start;
        self.push_start_up(up.expect("upstream"), in_start, d)
    }

    fn push_end_down(&mut self, s: usize, in_end: i32, d: Dir) -> Result<i32> {
        let kind = self.stages[s].kind.clone();
        let in_size = self.input_size(s).get(d);
        match kind {
            Kind::Input { align, .. } => {
                let e = if in_end >= in_size {
                    in_size
                } else {
                    in_end - in_end % align.get(d)
                };
                self.stages[s].input.set_end(e);
                self.stages[s].output.set_end(e);
                let r = self.push_end_down(self.down(s), e, d)?;
                self.push_end_up(s, r, d)?;
                Ok(self.stages[s].input.end())
            }
            Kind::Context { context, align } => {
                let mut out_end = in_end;
                if in_end < in_size {
                    out_end -= out_end % align.get(d);
                    out_end -= context;
                }
                self.stages[s].input.set_end(in_end);
                self.stages[s].output.set_end(out_end);
                let r = self.push_end_down(self.down(s), out_end, d)?;
                self.push_end_up(s, r, d)?;
                Ok(self.stages[s].input.end())
            }
            Kind::Split { downstream, .. } => {
                self.stages[s].input.set_end(0);
                for &b in &downstream {
                    if self.branch_complete(b) {
                        continue;
                    }
                    let e = self.push_end_down(b, in_end, d)?;
                    if e > self.stages[s].input.end() {
                        self.stages[s].input.set_end(e);
                    }
                }
                check(
                    self.stages[s].input.length != 0,
                    "no branch can make progress",
                )?;
                let end = self.stages[s].input.end();
                for &b in &downstream {
                    if !self.branch_complete(b) {
                        self.push_end_down(b, end, d)?;
                    }
                }
                Ok(end)
            }
            Kind::Crop { crop } => {
                let c = crop_axis(crop, d);
                let out_end = (in_end - c.offset).min(c.length);
                self.stages[s].output.set_end(out_end);
                if !self.crop_valid(s, d) {
                    self.basic_reset(s);
                    return Ok(0);
                }
                self.stages[s].input.set_end(in_end);
                let r = self.push_end_down(self.down(s), out_end, d)?;
                self.push_end_up(s, r, d)?;
                Ok(self.stages[s].input.end())
            }
            Kind::Rescale {
                out_size,
                scale,
                end_context,
                kind,
                ..
            } => {
                self.stages[s].input.set_end(in_end);
                let sc = scale.get(d);
                let mut out_end = match kind {
                    RescaleKind::Downscaler => (in_end << SCALE_PRECISION) / sc,
                    RescaleKind::Resampler => {
                        let mut last = in_end - 1;
                        if in_end < in_size {
                            last -= end_context.get(d) + 2;
                        }
                        ((last << SCALE_PRECISION) + ROUND_UP) / sc + 1
                    }
                };
                out_end = out_end.min(out_size.get(d));
                let cap = self.stages[s].output.offset + self.max_tile.get(d);
                out_end = out_end.min(cap);
                self.stages[s].output.set_end(out_end);
                let r = self.push_end_down(self.down(s), out_end, d)?;
                self.push_end_up(s, r, d)?;
                let min = self.min_tile.get(d);
                if self.stages[s].output.end() < out_size.get(d)
                    && self.stages[s].input.end() > in_size - min
                {
                    self.push_end_down(s, in_size - min, d)?;
                }
                Ok(self.stages[s].input.end())
            }
            Kind::Output {
                max_align,
                min_align,
                x_mirrored,
                ..
            } => {
                let mirrored = d == Dir::X && x_mirrored;
                let off = self.stages[s].output.offset;
                let mut out_end = in_end;
                let a = align_end(out_end, in_size, max_align.get(d), mirrored);
                if a >= off + max_align.get(d) {
                    out_end = a;
                } else {
                    let a = align_end(out_end, in_size, min_align.get(d), mirrored);
                    if a > off || self.stages[s].input.offset < in_size {
                        out_end = a;
                    }
                }
                self.stages[s].input.set_end(in_end);
                self.stages[s].output.set_end(out_end);
                self.push_end_up(s, out_end, d)?;
                Ok(self.stages[s].input.end())
            }
        }
    }

    fn crop_valid(&self, s: usize, d: Dir) -> bool {
        let o = self.stages[s].output;
        let min = self.min_tile.get(d);
        o.end() >= min && o.length >= min
    }

    fn push_end_up(&mut self, s: usize, out_end: i32, d: Dir) -> Result<()> {
        let kind = self.stages[s].kind.clone();
        let in_size = self.input_size(s).get(d);
        let in_end = match kind {
            Kind::Input {
                align,
                compression_align,
                ..
            } => {
                let a = align.get(d);
                let mut e = (out_end + a - 1) / a * a;
                if e > in_size {
                    e = in_size;
                    if d == Dir::X && compression_align > 0 {
                        e = (e + compression_align - 1) / compression_align * compression_align;
                    }
                }
                e
            }
            Kind::Context { context, align } => {
                check(
                    out_end <= self.stages[s].output.end(),
                    "context: output grew",
                )?;
                let a = align.get(d);
                ((out_end + context + a - 1) / a * a).min(in_size)
            }
            Kind::Split { .. } => return Ok(()),
            Kind::Crop { crop } => {
                let e = out_end + crop_axis(crop, d).offset;
                self.stages[s].input.set_end(e);
                self.stages[s].output.set_end(out_end);
                if !self.crop_valid(s, d) {
                    self.basic_reset(s);
                }
                return Ok(());
            }
            Kind::Rescale {
                scale,
                end_context,
                kind,
                ..
            } => {
                let sc = scale.get(d);
                let e = match kind {
                    RescaleKind::Downscaler => (out_end * sc + ROUND_UP) >> SCALE_PRECISION,
                    RescaleKind::Resampler => {
                        (((out_end - 1) * sc) >> SCALE_PRECISION) + end_context.get(d) + 2 + 1
                    }
                };
                e.min(in_size)
            }
            Kind::Output { .. } => {
                check(
                    out_end == self.stages[s].output.end(),
                    "output: end changed",
                )?;
                out_end
            }
        };
        self.stages[s].output.set_end(out_end);
        self.stages[s].input.set_end(in_end);
        Ok(())
    }

    fn push_crop_down(&mut self, s: usize, iv: Interval, d: Dir) -> Result<()> {
        let kind = self.stages[s].kind.clone();
        let in_size = self.input_size(s).get(d);
        let next = match kind {
            Kind::Input { .. } => {
                check(iv == self.stages[s].input, "input: interval changed")?;
                let st = &mut self.stages[s];
                st.crop = Crop::default();
                st.output = iv;
                iv
            }
            Kind::Context { align, .. } => {
                check(iv.contains(self.stages[s].input), "context: lost pixels")?;
                let a = align.get(d);
                let st = &mut self.stages[s];
                if iv.offset % a != 0 || (iv.end() % a != 0 && iv.end() != in_size) {
                    st.output = st.input;
                } else {
                    st.output = iv;
                }
                st.input = iv;
                st.crop = st.input.crop_to(st.output);
                st.output
            }
            Kind::Split { downstream, .. } => {
                check(iv.contains(self.stages[s].input), "split: lost pixels")?;
                self.stages[s].input = iv;
                for b in downstream {
                    if !self.branch_complete(b) {
                        self.push_crop_down(b, iv, d)?;
                    }
                }
                return Ok(());
            }
            Kind::Crop { crop } => {
                if !self.crop_valid(s, d) {
                    self.basic_reset(s);
                    return Ok(());
                }
                check(iv.contains(self.stages[s].input), "crop: lost pixels")?;
                let st = &mut self.stages[s];
                st.input = iv;
                let shifted = Interval::new(iv.offset - crop_axis(crop, d).offset, iv.length);
                st.crop = shifted.crop_to(st.output);
                st.output
            }
            Kind::Rescale { .. } => {
                check(iv.contains(self.stages[s].input), "rescale: lost pixels")?;
                let st = &mut self.stages[s];
                st.crop = iv.crop_to(st.input);
                st.input = iv;
                st.output
            }
            Kind::Output { .. } => {
                let st = &mut self.stages[s];
                st.input = iv;
                st.crop = iv.crop_to(st.output);
                check(
                    st.crop.start >= 0 && st.crop.end >= 0,
                    "output: negative crop",
                )?;
                return Ok(());
            }
        };
        self.push_crop_down(self.down(s), next, d)
    }

    fn copy_out(&mut self, s: usize, t: &mut TileRegions, d: Dir) {
        let Some(slot) = self.stages[s].slot else {
            return;
        };
        if self.branch_complete(s) || self.branch_inactive(s) {
            self.basic_reset(s);
        }
        let st = &self.stages[s];
        let r = match slot {
            Slot::Input => &mut t.input,
            Slot::Context => &mut t.context,
            Slot::Crop(i) => &mut t.crop[i],
            Slot::Downscale(i) => &mut t.downscale[i],
            Slot::Resample(i) => &mut t.resample[i],
            Slot::Output(i) => &mut t.output[i],
        };
        r.set(d, st.input, st.crop, st.output);
    }

    fn tile_direction(&mut self, d: Dir, max_tiles: usize) -> Result<Vec<TileRegions>> {
        for s in 0..self.stages.len() {
            self.reset(s);
        }
        self.first_tile = true;
        let mut tiles = Vec::new();
        loop {
            check(tiles.len() < max_tiles, "too many tiles")?;
            for o in self.outputs.clone() {
                if !self.branch_complete(o) {
                    let end = self.stages[o].output.end();
                    self.push_start_up(o, end, d)?;
                }
            }
            for i in self.inputs.clone() {
                let e = self.stages[i].input.offset + self.max_tile.get(d);
                self.push_end_down(i, e, d)?;
            }
            for i in self.inputs.clone() {
                let iv = self.stages[i].input;
                self.push_crop_down(i, iv, d)?;
            }
            let mut t = TileRegions::default();
            for s in 0..self.stages.len() {
                self.copy_out(s, &mut t, d);
            }
            tiles.push(t);
            let mut done = true;
            for o in self.outputs.clone() {
                if self.branch_complete(o) {
                    continue;
                }
                if self.stages[o].output.end() >= self.output_size(o).get(d) {
                    if let Kind::Output { complete, .. } = &mut self.stages[o].kind {
                        *complete = true;
                    }
                } else {
                    done = false;
                }
            }
            self.first_tile = false;
            if done {
                return Ok(tiles);
            }
        }
    }
}

fn align_end(input_end: i32, image_size: i32, align: i32, mirrored: bool) -> i32 {
    if mirrored {
        let unflipped = (image_size - input_end + align - 1) / align * align;
        image_size - unflipped
    } else if input_end < image_size {
        input_end - input_end % align
    } else {
        input_end
    }
}

/// Tiles the pipeline described by `config` into at most `max_tiles` tiles. Returns the
/// tiles in row-major order and the grid size `(columns, rows)`.
pub fn tile_pipeline(
    config: &TilingConfig,
    max_tiles: usize,
) -> Result<(Vec<TileRegions>, Length2)> {
    let mut p = Pipeline {
        stages: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        max_tile: config.max_tile_size,
        min_tile: config.min_tile_size,
        first_tile: false,
    };
    let compression_align = if config.compressed_input {
        COMPRESSION_ALIGN
    } else {
        0
    };
    let mut align = config.input_alignment;
    align.dx = align.dx.max(compression_align);
    let input = p.add(
        Kind::Input {
            size: config.input_image_size,
            align,
            compression_align,
        },
        Some(Slot::Input),
        None,
    );
    let context = p.add(
        Kind::Context {
            context: PIPELINE_CONTEXT,
            align: Length2::new(PIPELINE_ALIGN, PIPELINE_ALIGN),
        },
        Some(Slot::Context),
        Some(input),
    );
    let split = p.add(
        Kind::Split {
            downstream: Vec::new(),
            count: 0,
        },
        None,
        Some(context),
    );
    for i in 0..NUM_BRANCHES {
        let out = config.output_image_size[i];
        if out.dx == 0 || out.dy == 0 {
            continue;
        }
        let mut prev = p.add(
            Kind::Crop {
                crop: config.crop[i],
            },
            Some(Slot::Crop(i)),
            Some(split),
        );
        if config.downscale_enables & (1 << i) != 0 {
            let f = config.downscale_factor[i];
            let right = Length2::new(
                ((f.dx + ROUND_UP) >> SCALE_PRECISION) - 1,
                ((f.dy + ROUND_UP) >> SCALE_PRECISION) - 1,
            );
            prev = p.add(
                Kind::Rescale {
                    out_size: config.downscale_image_size[i],
                    scale: f,
                    start_context: Length2::default(),
                    end_context: right,
                    kind: RescaleKind::Downscaler,
                },
                Some(Slot::Downscale(i)),
                Some(prev),
            );
        }
        if config.resample_enables & (1 << i) != 0 {
            prev = p.add(
                Kind::Rescale {
                    out_size: out,
                    scale: config.resample_factor[i],
                    start_context: Length2::new(START_CONTEXT, START_CONTEXT),
                    end_context: Length2::new(END_CONTEXT, END_CONTEXT),
                    kind: RescaleKind::Resampler,
                },
                Some(Slot::Resample(i)),
                Some(prev),
            );
        }
        p.add(
            Kind::Output {
                max_align: config.output_max_alignment[i],
                min_align: config.output_min_alignment[i],
                x_mirrored: config.output_h_mirror[i],
                complete: false,
            },
            Some(Slot::Output(i)),
            Some(prev),
        );
    }
    check(!p.outputs.is_empty(), "no output branch")?;
    let xs = p.tile_direction(Dir::X, max_tiles)?;
    let ys = p.tile_direction(Dir::Y, max_tiles / xs.len())?;
    let grid = Length2::new(xs.len() as i32, ys.len() as i32);
    let mut tiles = Vec::with_capacity(xs.len() * ys.len());
    for y in &ys {
        for x in &xs {
            tiles.push(merge(x, y));
        }
    }
    Ok((tiles, grid))
}

fn merge_region(x: &Region, y: &Region) -> Region {
    Region {
        input: Interval2 {
            x: x.input.x,
            y: y.input.y,
        },
        crop: Crop2 {
            x: x.crop.x,
            y: y.crop.y,
        },
        output: Interval2 {
            x: x.output.x,
            y: y.output.y,
        },
    }
}

fn merge(x: &TileRegions, y: &TileRegions) -> TileRegions {
    let m = |a: &[Region; NUM_BRANCHES], b: &[Region; NUM_BRANCHES]| {
        [merge_region(&a[0], &b[0]), merge_region(&a[1], &b[1])]
    };
    TileRegions {
        input: merge_region(&x.input, &y.input),
        context: merge_region(&x.context, &y.context),
        crop: m(&x.crop, &y.crop),
        downscale: m(&x.downscale, &y.downscale),
        resample: m(&x.resample, &y.resample),
        output: m(&x.output, &y.output),
    }
}

impl Interval {
    /// `self` with `crop` removed from both ends.
    pub fn cropped(self, crop: Crop) -> Interval {
        self.minus_crop(crop)
    }
}

#[cfg(test)]
mod tests;
