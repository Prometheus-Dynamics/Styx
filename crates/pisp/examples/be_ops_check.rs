//! On a CM5 / Pi 5: each `be::ops` op run once on the back end, memory to memory on dma-heap
//! buffers, its output measured against a CPU reference. Prints, per op, the job time and how
//! many output samples differ from the reference and by how much.
//!
//! ```text
//! be_ops_check [WIDTHxHEIGHT]      default 640x480; needs the `pispbe` node group 0 free
//! ```
//!
//! - `downscale` (by 2, output 1), `box2` and `pyramid2` (both outputs, `run2`) against
//!   `(a+b+c+d+2)>>2` and the 4x4 mean rounded half up: which codes differ and whether the
//!   rounding truncates instead.
//! - `resample` (Lanczos 3 to half size) against the 2x2 mean: a sanity bound, not exactness.
//! - `lut` (a square-root curve) against the table and against [`LutFit`]'s prediction.
//! - `filter5x5` (a zero-sum high-pass, centre 24) against the input: how much it moves.
//! - `demosaic_ccm` on flat Bayer patches with the identity matrix: the RGB each patch comes
//!   out as.
//!
//! Every op but demosaic takes `GREY` through the greyscale Bayer path, so a run also says the
//! input node accepts it.

use std::os::fd::AsFd;
use std::time::Duration;

use styx_kernel::Mapping;
use styx_kernel::dma_heap::{Access, DmaBuf, DmaHeap, sync};
use styx_pisp::be::defaults;
use styx_pisp::be::ops::{self, BeOp, Pixels};
use styx_pisp::device::BeImageDevice;
use styx_pisp::uapi::BayerOrder;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A run: the job time, output 0 and output 1 read back, and the op as opened.
type Ran = (Duration, Vec<u8>, Option<Vec<u8>>, BeOp);

struct Buf {
    buf: DmaBuf,
    map: Mapping,
}

fn alloc(heap: &DmaHeap, len: u32) -> Result<Buf> {
    let buf = heap.allocate((len as usize).next_multiple_of(4096))?;
    let map = buf.map()?;
    Ok(Buf { buf, map })
}

/// The test image: a ramp both ways plus a fixed pseudo-random texture, every code used.
fn sample(x: usize, y: usize) -> u8 {
    let n = (x as u32).wrapping_mul(2_654_435_761) ^ (y as u32).wrapping_mul(40_503);
    ((x + 2 * y) as u32 + (n >> 27)) as u8
}

/// Writes `w`x`h` grey samples into `b` at `stride`.
fn fill_grey(b: &mut Buf, w: usize, h: usize, stride: usize) -> Result<()> {
    sync(b.buf.as_fd(), Access::Write, true)?;
    let m = b.map.as_mut_slice();
    for y in 0..h {
        for x in 0..w {
            m[y * stride + x] = sample(x, y);
        }
    }
    sync(b.buf.as_fd(), Access::Write, false)?;
    Ok(())
}

/// The grey image read back from `b` (`w`x`h` at `stride`).
fn read(b: &Buf, w: usize, h: usize, stride: usize) -> Result<Vec<u8>> {
    sync(b.buf.as_fd(), Access::Read, true)?;
    let m = b.map.as_slice();
    let out = (0..h)
        .flat_map(|y| m[y * stride..][..w].iter().copied())
        .collect();
    sync(b.buf.as_fd(), Access::Read, false)?;
    Ok(out)
}

/// `f`x`f` block means of the test image, rounded half up.
fn mean_ref(w: usize, h: usize, f: usize) -> Vec<u8> {
    let (ow, oh) = (w / f, h / f);
    let mut out = Vec::with_capacity(ow * oh);
    for y in 0..oh {
        for x in 0..ow {
            let s: usize = (0..f * f)
                .map(|k| usize::from(sample(x * f + k % f, y * f + k / f)))
                .sum();
            out.push(((s + f * f / 2) / (f * f)) as u8);
        }
    }
    out
}

/// Differing samples, the largest difference, and how many are exactly one below (truncation).
fn compare(name: &str, got: &[u8], want: &[u8]) {
    let mut differ = 0;
    let mut max = 0;
    let mut below = 0;
    for (&g, &w) in got.iter().zip(want) {
        if g != w {
            differ += 1;
            max = max.max(g.abs_diff(w));
            below += usize::from(g + 1 == w);
        }
    }
    println!(
        "  {name}: {differ} of {} differ, max {max}, {below} exactly one below",
        want.len()
    );
}

struct Runner {
    heap: DmaHeap,
    size: (u16, u16),
}

impl Runner {
    /// Opens `op`, fills a grey input when `grey`, runs it once (both outputs if it has two)
    /// and returns the job time, the outputs (read at their strides) and the op as opened.
    fn run(&self, mut op: BeOp, input: impl FnOnce(&mut Buf, &BeOp) -> Result<()>) -> Result<Ran> {
        let mut dev = BeImageDevice::open(0, &mut op)?;
        let mut inb = alloc(&self.heap, dev.input_len())?;
        input(&mut inb, &op)?;
        let out = alloc(&self.heap, dev.output_len())?;
        let out1 = dev
            .output1_len()
            .map(|l| alloc(&self.heap, l))
            .transpose()?;
        let timeout = Duration::from_secs(2);
        let t = match &out1 {
            Some(o1) => dev.run2(inb.buf.as_fd(), out.buf.as_fd(), o1.buf.as_fd(), timeout)?,
            None => dev.run(inb.buf.as_fd(), out.buf.as_fd(), timeout)?,
        };
        let plane = |b: &Buf, f: &styx_pisp::uapi::ImageFormatConfig| {
            let bytes = if op.out_fourcc == *b"RGB3" { 3 } else { 1 };
            read(
                b,
                usize::from(f.width) * bytes,
                usize::from(f.height),
                f.stride as usize,
            )
        };
        let got = plane(&out, &op.out)?;
        let got1 = match (&out1, &op.out1) {
            (Some(b), Some(f)) => Some(plane(b, f)?),
            _ => None,
        };
        dev.stop()?;
        Ok((t, got, got1, op))
    }

    fn grey(&self, op: BeOp) -> Result<Ran> {
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        self.run(op, |b, op| fill_grey(b, w, h, op.input.stride as usize))
    }
}

fn main() -> Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "640x480".into());
    let (w, h) = arg
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?)))
        .ok_or("size: WIDTHxHEIGHT")?;
    let heap = DmaHeap::open("linux,cma").or_else(|_| DmaHeap::open("system"))?;
    let r = Runner { heap, size: (w, h) };
    let (wu, hu) = (usize::from(w), usize::from(h));
    println!(
        "PiSP back end ops, GREY {w}x{h}, buffers from the {} heap",
        r.heap.name()
    );
    let half = mean_ref(wu, hu, 2);

    let report = |name: &str, res: Result<Ran>| match res {
        Ok((t, got, got1, op)) => {
            println!(
                "{name}: {:.3} ms, out {}x{} stride {}",
                t.as_secs_f64() * 1e3,
                op.out.width,
                op.out.height,
                op.out.stride
            );
            Some((got, got1))
        }
        Err(e) => {
            println!("{name}: FAILED: {e}");
            None
        }
    };

    if let Some((got, _)) = report(
        "downscale x2 (output 1)",
        r.grey(ops::downscale(Pixels::Grey8, (w, h), 2)?),
    ) {
        compare("vs (a+b+c+d+2)>>2", &got, &half);
    }
    if let Some((got, _)) = report("box2 (output 0)", r.grey(ops::box2(Pixels::Grey8, (w, h))?)) {
        compare("vs (a+b+c+d+2)>>2", &got, &half);
    }
    if let Some((got, got1)) = report(
        "pyramid2 (run2)",
        r.grey(ops::pyramid2(Pixels::Grey8, (w, h))?),
    ) {
        compare("level 1 vs (a+b+c+d+2)>>2", &got, &half);
        match got1 {
            Some(g1) => compare("level 2 vs 4x4 mean", &g1, &mean_ref(wu, hu, 4)),
            None => println!("  level 2: no second output"),
        }
    }
    let lanczos = ops::resample(Pixels::Grey8, (w, h), (w / 2, h / 2), &defaults::LANCZOS3)?;
    if let Some((got, _)) = report("resample Lanczos 3 to half", r.grey(lanczos)) {
        compare("vs the 2x2 mean (sanity only)", &got, &half);
    }
    let table: [u8; 256] =
        core::array::from_fn(|v| ((v as f64 / 255.0).sqrt() * 255.0).round() as u8);
    let (op, fit) = ops::lut(Pixels::Grey8, (w, h), &table)?;
    if let Some((got, _)) = report("lut sqrt", r.grey(op)) {
        let want: Vec<u8> = (0..hu)
            .flat_map(|y| (0..wu).map(move |x| table[usize::from(sample(x, y))]))
            .collect();
        println!(
            "  LutFit predicts {} inexact codes, max {}",
            fit.inexact_codes, fit.max_error
        );
        compare("vs the table", &got, &want);
    }
    let mut k = [-1i8; 25];
    k[12] = 24;
    if let Some((got, _)) = report(
        "filter5x5 high-pass",
        r.grey(ops::filter5x5(Pixels::Grey8, (w, h), &k)?),
    ) {
        let input: Vec<u8> = (0..hu)
            .flat_map(|y| (0..wu).map(move |x| sample(x, y)))
            .collect();
        compare("vs the input", &got, &input);
    }
    // Demosaic: four flat quadrants, each one 16-bit level for R, G and B, identity matrix.
    let patches: [[u16; 3]; 4] = [
        [64 << 8, 128 << 8, 192 << 8],
        [200 << 8, 50 << 8, 100 << 8],
        [255 << 8, 255 << 8, 255 << 8],
        [16 << 8, 16 << 8, 16 << 8],
    ];
    let ident = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let op = ops::demosaic_ccm(BayerOrder::Rggb, (w, h), ident, [0.0; 3])?;
    let quad = |x: usize, y: usize| usize::from(x >= wu / 2) + 2 * usize::from(y >= hu / 2);
    let res = r.run(op, |b, op| {
        let stride = op.input.stride as usize;
        sync(b.buf.as_fd(), Access::Write, true)?;
        let m = b.map.as_mut_slice();
        for y in 0..hu {
            for x in 0..wu {
                let c = patches[quad(x, y)];
                // RGGB: R at even/even, B at odd/odd, G elsewhere.
                let v = match (x % 2, y % 2) {
                    (0, 0) => c[0],
                    (1, 1) => c[2],
                    _ => c[1],
                };
                m[y * stride + 2 * x..][..2].copy_from_slice(&v.to_le_bytes());
            }
        }
        sync(b.buf.as_fd(), Access::Write, false)?;
        Ok(())
    });
    if let Some((got, _)) = report("demosaic_ccm identity", res) {
        for (q, p) in patches.iter().enumerate() {
            let (x, y) = ((q % 2) * wu / 2 + wu / 4, (q / 2) * hu / 2 + hu / 4);
            let at = (y * wu + x) * 3;
            println!(
                "  patch {q}: in {:?} -> out {:?}",
                p.map(|v| v >> 8),
                &got[at..at + 3]
            );
        }
    }
    Ok(())
}
