//! The back end's raw input from a receiver's frame: the one pass that puts a raw frame into a
//! back-end input buffer, and what it costs.
//!
//! The back end reads 8-bit and 16-bit Bayer (and its own 10-bit and compressed forms), not CSI-2
//! packed raw: on the reference board the front end does that decode. With the receiver feeding
//! Styx directly, the pass into the input buffer is also the conversion:
//!
//! - **8-bit Bayer** (`BA81`, `GBRG`, `GRBG`, `RGGB`): rows copied as they are into an 8-bit input.
//! - **16-bit Bayer** (a 16-bit container): rows copied as they are into a 16-bit input (the
//!   samples must already be in the top bits, as the back end reads them).
//! - **CSI-2 RAW10** (`pBAA` and its orders): each row unpacked with the soft ISP's own unpack
//!   (`styx_simd::isp::unpack_raw10_row`) into 16-bit samples moved to the top bits.
//!
//! Each pass is counted in [`PISP_BE_INPUT`] with the bytes it read and wrote.

use core::sync::atomic::{AtomicUsize, Ordering};

use styx_aaa::{BayerStats, BayerStatsConfig, Cfa};
use styx_format::{BitDepth, CfaOrder, MediaFormat};
use styx_frame::FrameLease;
use styx_lemnos::pisp::format::{compute_stride_align, formats};
use styx_lemnos::pisp::uapi::{BayerOrder, ImageFormatConfig};

use crate::PispError;

/// Bytes each input row is aligned to.
const INPUT_ALIGN: u32 = 16;

/// The passes into a back-end input buffer: how many, and the bytes read from the raw frames and
/// written into the input buffers.
#[derive(Debug)]
pub struct InputCounter {
    passes: AtomicUsize,
    read: AtomicUsize,
    written: AtomicUsize,
}

impl InputCounter {
    const fn new() -> Self {
        Self {
            passes: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            written: AtomicUsize::new(0),
        }
    }

    fn record(&self, read: usize, written: usize) {
        self.passes.fetch_add(1, Ordering::Relaxed);
        self.read.fetch_add(read, Ordering::Relaxed);
        self.written.fetch_add(written, Ordering::Relaxed);
    }

    /// The site's name, as the service's `copy` lines give it.
    pub const fn site(&self) -> &'static str {
        "pisp-be-input"
    }

    pub fn passes(&self) -> usize {
        self.passes.load(Ordering::Relaxed)
    }

    pub fn bytes_read(&self) -> usize {
        self.read.load(Ordering::Relaxed)
    }

    pub fn bytes_written(&self) -> usize {
        self.written.load(Ordering::Relaxed)
    }
}

/// Every raw frame put into a back-end input buffer in this process.
pub static PISP_BE_INPUT: InputCounter = InputCounter::new();

/// How a raw frame becomes the back end's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputForm {
    /// 8-bit Bayer rows copied.
    Bayer8,
    /// 16-bit Bayer rows copied.
    Bayer16,
    /// CSI-2 RAW10 rows unpacked to 16-bit, samples in the top bits.
    Raw10Unpacked,
}

impl InputForm {
    /// The form for receiver frames of `raw`'s format, or why the back end cannot take them.
    pub fn of(raw: &MediaFormat) -> Result<Self, PispError> {
        let info = raw
            .code
            .info()
            .ok_or_else(|| PispError::Config(format!("raw format {} is unknown", raw.code)))?;
        if info.cfa.is_none() {
            return Err(PispError::Config(format!(
                "raw format {} is not Bayer",
                raw.code
            )));
        }
        match (info.bit_depth, info.csi2_bits, info.sample_bits) {
            (BitDepth::U8, None, 8) => Ok(Self::Bayer8),
            (BitDepth::U16, None, 16) => Ok(Self::Bayer16),
            (BitDepth::Packed, Some(10), _) => Ok(Self::Raw10Unpacked),
            _ => Err(PispError::Config(format!(
                "raw format {} ({} significant bits) has no back-end input form here",
                raw.code, info.sample_bits
            ))),
        }
    }

    /// The name the status gives this form.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bayer8 => "bayer8-copy",
            Self::Bayer16 => "bayer16-copy",
            Self::Raw10Unpacked => "raw10-unpack-to-bayer16",
        }
    }

    /// The back end's input image for frames of `raw`'s size, with its stride.
    pub fn image(self, raw: &MediaFormat) -> Result<ImageFormatConfig, PispError> {
        let r = raw.resolution;
        let dim = |v: u32, what| {
            u16::try_from(v).map_err(|_| PispError::Config(format!("raw {what} {v} is too large")))
        };
        let mut image = ImageFormatConfig {
            width: dim(r.width.get(), "width")?,
            height: dim(r.height.get(), "height")?,
            format: match self {
                Self::Bayer8 => styx_lemnos::pisp::uapi::image_format::BPS_8,
                Self::Bayer16 | Self::Raw10Unpacked => formats::BAYER16,
            },
            stride: 0,
            stride2: 0,
        };
        compute_stride_align(&mut image, INPUT_ALIGN);
        Ok(image)
    }

    /// The back end's Bayer order for `raw`'s mosaic.
    pub fn bayer(raw: &MediaFormat) -> Result<BayerOrder, PispError> {
        let cfa =
            raw.code.info().and_then(|info| info.cfa).ok_or_else(|| {
                PispError::Config(format!("raw format {} is not Bayer", raw.code))
            })?;
        Ok(match cfa {
            CfaOrder::Rggb => BayerOrder::Rggb,
            CfaOrder::Grbg => BayerOrder::Grbg,
            CfaOrder::Gbrg => BayerOrder::Gbrg,
            CfaOrder::Bggr => BayerOrder::Bggr,
        })
    }

    /// The raw statistics [`fill_with_stats`] can count for frames of `raw` in this form: the
    /// frame's size and mosaic, with the bits the pass hands over (8, 16, or the 10 of an unpacked
    /// RAW10 row) and the default zones and sampling of [`BayerStatsConfig::new`].
    pub fn stats_config(self, raw: &MediaFormat) -> Result<BayerStatsConfig, PispError> {
        let cfa =
            raw.code.info().and_then(|info| info.cfa).ok_or_else(|| {
                PispError::Config(format!("raw format {} is not Bayer", raw.code))
            })?;
        let cfa = match cfa {
            CfaOrder::Rggb => Cfa::Rggb,
            CfaOrder::Grbg => Cfa::Grbg,
            CfaOrder::Gbrg => Cfa::Gbrg,
            CfaOrder::Bggr => Cfa::Bggr,
        };
        let bits = match self {
            Self::Bayer8 => 8,
            Self::Bayer16 => 16,
            Self::Raw10Unpacked => 10,
        };
        let r = raw.resolution;
        Ok(BayerStatsConfig::new(
            r.width.get(),
            r.height.get(),
            cfa,
            bits,
        ))
    }
}

/// One pass of `raw` into `dst` (an input buffer laid out as [`InputForm::image`]), with `row` a
/// scratch row of at least the frame's width (sized once by the caller). Counted in
/// [`PISP_BE_INPUT`]. Returns the bytes read and written.
pub fn fill<M, N>(
    form: InputForm,
    raw: &FrameLease<M>,
    dst: &mut FrameLease<N>,
    row: &mut [u16],
) -> Result<(usize, usize), PispError> {
    fill_with_stats(form, raw, dst, row, None)
}

/// [`fill`], handing the rows `stats` counts to it on the way (the samples as the pass has them:
/// the unpacked RAW10 row, or the 8- or 16-bit row read once more from the input just written,
/// still in cache). The frame's statistics come from the one pass, with no second read of the
/// frame. `stats` is not reset here: the caller resets it per frame and finishes it after.
pub fn fill_with_stats<M, N>(
    form: InputForm,
    raw: &FrameLease<M>,
    dst: &mut FrameLease<N>,
    row: &mut [u16],
    mut stats: Option<&mut BayerStats>,
) -> Result<(usize, usize), PispError> {
    let width = raw.format().resolution.width.get() as usize;
    let height = raw.format().resolution.height.get() as usize;
    let src = raw
        .read()
        .ok_or_else(|| PispError::Job("the raw frame is not mapped for the CPU".into()))?;
    let from = src
        .plane(0)
        .ok_or_else(|| PispError::Job("the raw frame has no plane".into()))?;
    let mut out = dst
        .write()
        .ok_or_else(|| PispError::Job("the input buffer is not writable".into()))?;
    let mut to = out
        .plane(0)
        .ok_or_else(|| PispError::Job("the input buffer has no plane".into()))?;
    let short = || PispError::Job("a row runs past its buffer".into());
    let (row_in, row_out) = match form {
        InputForm::Bayer8 => (width, width),
        InputForm::Bayer16 => (width * 2, width * 2),
        InputForm::Raw10Unpacked => (width.div_ceil(4) * 5, width * 2),
    };
    if row.len() < width {
        return Err(PispError::Job(
            "the scratch row is shorter than the frame".into(),
        ));
    }
    for y in 0..height {
        let a = from.row_bytes(y, row_in).ok_or_else(short)?;
        let b = to.row_bytes_mut(y, row_out).ok_or_else(short)?;
        let counted = stats.as_deref().filter(|s| s.wants(y)).is_some();
        match form {
            InputForm::Bayer8 => {
                b.copy_from_slice(a);
                if counted {
                    for (v, &byte) in row[..width].iter_mut().zip(a) {
                        *v = u16::from(byte);
                    }
                }
            }
            InputForm::Bayer16 => {
                b.copy_from_slice(a);
                if counted {
                    for (v, pair) in row[..width].iter_mut().zip(a.as_chunks::<2>().0) {
                        *v = u16::from_le_bytes(*pair);
                    }
                }
            }
            InputForm::Raw10Unpacked => {
                styx_simd::isp::unpack_raw10_row(a, row, width);
                for (pair, sample) in b.as_chunks_mut::<2>().0.iter_mut().zip(&row[..width]) {
                    // The back end reads 16-bit samples in the top bits.
                    *pair = (sample << 6).to_le_bytes();
                }
            }
        }
        if counted && let Some(s) = stats.as_deref_mut() {
            s.add_row(y, &row[..width]);
        }
    }
    let (read, written) = (row_in * height, row_out * height);
    PISP_BE_INPUT.record(read, written);
    Ok((read, written))
}

#[cfg(test)]
mod tests {
    use styx_format::{FourCc, Resolution};
    use styx_frame::{BufferSize, FrameEntry, FramePool, packed_layouts};

    use super::*;

    const W: u32 = 8;
    const H: u32 = 2;

    fn lease(pool: &FramePool<()>, code: FourCc, bytes: &[u8]) -> FrameLease<()> {
        let format = MediaFormat::with_defaults(code, Resolution::new(W, H).expect("non-empty"));
        let layouts = packed_layouts(code, W, H, Some(16), None).expect("layout");
        let mut lease = pool
            .acquire(FrameEntry::new(1, 0, format, layouts))
            .expect("a free buffer");
        {
            let mut planes = lease.write().expect("writable");
            let dst = planes.bytes();
            dst[..bytes.len()].copy_from_slice(bytes);
        }
        lease
    }

    fn input(pool: &FramePool<()>, form: InputForm) -> FrameLease<()> {
        let code = if form == InputForm::Bayer8 {
            FourCc::RGGB
        } else {
            FourCc::RG16
        };
        lease(pool, code, &[])
    }

    #[test]
    fn eight_bit_bayer_is_copied_row_by_row_and_counted() {
        let pool = FramePool::host(2, BufferSize::new(4096, 0)).expect("pool");
        // Rows at a 16-byte stride: 8 samples then padding.
        let mut raw = vec![0u8; 32];
        for y in 0..2 {
            for x in 0..8 {
                raw[y * 16 + x] = (y * 8 + x) as u8 + 1;
            }
        }
        let form = InputForm::of(&MediaFormat::with_defaults(
            FourCc::RGGB,
            Resolution::new(W, H).expect("non-empty"),
        ))
        .expect("a form");
        assert_eq!(form, InputForm::Bayer8);
        let src = lease(&pool, FourCc::RGGB, &raw);
        let mut dst = input(&pool, form);
        let before = (PISP_BE_INPUT.passes(), PISP_BE_INPUT.bytes_read());
        let mut row = vec![0u16; 8];
        assert_eq!(
            fill(form, &src, &mut dst, &mut row).expect("filled"),
            (16, 16)
        );
        assert!(PISP_BE_INPUT.passes() > before.0);
        assert!(PISP_BE_INPUT.bytes_read() >= before.1 + 16);
        let read = dst.read().expect("readable");
        let plane = read.plane(0).expect("plane");
        for y in 0..2 {
            assert_eq!(
                plane.row_bytes(y, 8).expect("row"),
                &raw[y * 16..y * 16 + 8]
            );
        }
    }

    #[test]
    fn csi2_raw10_is_unpacked_with_its_samples_in_the_top_bits() {
        let pool = FramePool::host(2, BufferSize::new(4096, 0)).expect("pool");
        // One row of 8 samples in two 5-byte groups: samples 0x3ff, 0x001, 0x200, 0x155, then
        // 0..3 shifted.
        let samples: [u16; 8] = [0x3ff, 0x001, 0x200, 0x155, 0, 1, 2, 3];
        let mut row_bytes = [0u8; 10];
        for (g, group) in samples.chunks(4).enumerate() {
            for (lane, &s) in group.iter().enumerate() {
                row_bytes[g * 5 + lane] = (s >> 2) as u8;
                row_bytes[g * 5 + 4] |= ((s & 3) as u8) << (2 * lane);
            }
        }
        let mut raw = vec![0u8; 32];
        raw[..10].copy_from_slice(&row_bytes);
        raw[16..26].copy_from_slice(&row_bytes);
        let format =
            MediaFormat::with_defaults(FourCc::RGGB10P, Resolution::new(W, H).expect("non-empty"));
        let form = InputForm::of(&format).expect("a form");
        assert_eq!(form, InputForm::Raw10Unpacked);
        assert_eq!(form.image(&format).expect("image").format, formats::BAYER16);
        let src = lease(&pool, FourCc::RGGB10P, &raw);
        let mut dst = input(&pool, form);
        let mut row = vec![0u16; 8];
        // 10 bytes read and 16 written per row.
        assert_eq!(
            fill(form, &src, &mut dst, &mut row).expect("filled"),
            (20, 32)
        );
        let read = dst.read().expect("readable");
        let plane = read.plane(0).expect("plane");
        for y in 0..2 {
            let got: Vec<u16> = plane
                .row_bytes(y, 16)
                .expect("row")
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes(*p))
                .collect();
            let want: Vec<u16> = samples.iter().map(|s| s << 6).collect();
            assert_eq!(got, want, "row {y}");
        }
    }

    #[test]
    fn the_pass_counts_the_statistics_of_the_rows_it_copies_in_each_form() {
        let pool = FramePool::host(4, BufferSize::new(4096, 0)).expect("pool");
        let r = Resolution::new(W, H).expect("non-empty");
        // RGGB at R 400, G 200, B 100 (10-bit), as RAW10 packed and as 8-bit (divided by 4).
        let samples: [u16; 8] = [400, 200, 400, 200, 400, 200, 400, 200];
        let lower: [u16; 8] = [200, 100, 200, 100, 200, 100, 200, 100];
        let pack = |row: &[u16; 8]| {
            let mut bytes = [0u8; 10];
            for (g, group) in row.chunks(4).enumerate() {
                for (lane, &s) in group.iter().enumerate() {
                    bytes[g * 5 + lane] = (s >> 2) as u8;
                    bytes[g * 5 + 4] |= ((s & 3) as u8) << (2 * lane);
                }
            }
            bytes
        };
        let mut raw10 = vec![0u8; 32];
        raw10[..10].copy_from_slice(&pack(&samples));
        raw10[16..26].copy_from_slice(&pack(&lower));
        let mut raw8 = vec![0u8; 32];
        for x in 0..8 {
            raw8[x] = (samples[x] / 4) as u8;
            raw8[16 + x] = (lower[x] / 4) as u8;
        }
        for (code, bytes, scale) in [
            (FourCc::RGGB10P, raw10, 1023.0),
            (FourCc::RGGB, raw8, 255.0 * 4.0),
        ] {
            let format = MediaFormat::with_defaults(code, r);
            let form = InputForm::of(&format).expect("a form");
            let mut config = form.stats_config(&format).expect("stats");
            (config.zones, config.row_step, config.col_step) = ((1, 1), 1, 1);
            let mut stats = BayerStats::new(config, 0);
            let src = lease(&pool, code, &bytes);
            let mut dst = input(&pool, form);
            let mut row = vec![0u16; 8];
            fill_with_stats(form, &src, &mut dst, &mut row, Some(&mut stats)).expect("filled");
            assert_eq!(stats.quads(), 4, "{code}");
            let mut out = styx_aaa::Statistics::new(
                styx_aaa::ZoneGrid::new(0, 0),
                None,
                Default::default(),
                None,
            );
            stats.finish_into(&mut out);
            let (r, g, b) = out.colour.zones[0].mean();
            assert!((r - 400.0 / scale).abs() < 1e-3, "{code} r {r}");
            assert!((g - 200.0 / scale).abs() < 1e-3, "{code} g {g}");
            assert!((b - 100.0 / scale).abs() < 1e-3, "{code} b {b}");
        }
    }

    #[test]
    fn formats_the_back_end_cannot_take_are_refused_by_name() {
        let r = Resolution::new(W, H).expect("non-empty");
        let err = InputForm::of(&MediaFormat::with_defaults(FourCc::NV12, r)).unwrap_err();
        assert!(err.to_string().contains("not Bayer"), "{err}");
    }
}
