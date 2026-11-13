//! Dev's back-end configuration bytes for the recorded settings, against Styx's.
//!
//! The recording is `styx-controller`'s replay (`crates/algo/controller/tests/data/dev-expected.jsonl`):
//! the start settings, then one line per frame. Dev's configuration for each was dumped at a
//! fixed geometry (1280x800 16-bit Bayer input, output 0 NV12 at full size, output 1 RGB at half
//! size with the 2x2 box, temporal denoise buffers in the input's format) by dev's
//! `BeConfigBuilder`, as the raw bytes of its `BeTilesConfig`. Index 0 is the start settings; index
//! `k` is recording frame `k - 1`. The goldens committed here are indices 0, 1, 2, 10, 40 and 80.
//!
//! The comparison is byte for byte over the whole tiles config. A difference is reported per
//! block with its offset and the bytes on each side.

use std::path::PathBuf;

use styx_pisp::pisp::format::formats;
use styx_pisp::pisp::uapi::{BayerOrder, BeConfig, BeTilesConfig, ImageFormatConfig, Wire};
use styx_pisp::{BackEndBuilder, Geometry, IspSettings};

/// Dev's indices committed as goldens.
const GOLDEN: [usize; 6] = [0, 1, 2, 10, 40, 80];

/// The blocks of `BeConfig` by name and offset (within the tiles config).
fn blocks() -> Vec<(&'static str, usize, usize)> {
    use std::mem::{offset_of, size_of};
    let base = offset_of!(BeTilesConfig, config);
    macro_rules! block {
        ($($f:ident),*) => {
            vec![$((stringify!($f), base + offset_of!(BeConfig, $f), 0usize),)*]
        };
    }
    let mut b: Vec<(&'static str, usize, usize)> = block!(
        global,
        input_format,
        decompress,
        dpc,
        geq,
        tdn_input_format,
        tdn_decompress,
        tdn,
        tdn_compress,
        tdn_output_format,
        sdn,
        blc,
        stitch_compress,
        stitch_output_format,
        stitch_input_format,
        stitch_decompress,
        stitch,
        lsc,
        wbg,
        cdn,
        cac,
        debin,
        tonemap,
        demosaic,
        ccm,
        sat_control,
        ycbcr,
        sharpen,
        false_colour,
        sh_fc_combine,
        ycbcr_inverse,
        gamma,
        csc,
        downscale,
        resample,
        output_format,
        hog,
        axi
    );
    // Each block runs to the next block's start (the last to the end of the config).
    b.sort_by_key(|(_, off, _)| *off);
    let end = base + size_of::<BeConfig>();
    for i in 0..b.len() {
        let next = b.get(i + 1).map_or(end, |n| n.1);
        b[i].2 = next - b[i].1;
    }
    b
}

fn input() -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: 1280,
        height: 800,
        format: formats::BAYER16,
        stride: 0,
        stride2: 0,
    };
    styx_pisp::pisp::format::compute_stride_align(&mut f, 16);
    f
}

fn output(width: u16, height: u16, format: u32, align: u32) -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width,
        height,
        format,
        stride: 0,
        stride2: 0,
    };
    styx_pisp::pisp::format::compute_stride_align(&mut f, align);
    f
}

fn geometry() -> Geometry {
    Geometry {
        input: input(),
        bayer: BayerOrder::Rggb,
        outputs: [
            output(1280, 800, formats::NV12, 64),
            output(640, 400, formats::RGB888, 64),
        ],
        tdn: Some(input()),
        slice_at: None,
    }
}

fn recording() -> Vec<serde_json::Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../algo/controller/tests/data/dev-expected.jsonl");
    std::fs::read_to_string(path)
        .expect("the controller recording")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect()
}

fn exposure(sensor: &serde_json::Value) -> f64 {
    let e = &sensor["exposure"];
    (e["secs"].as_f64().unwrap() + e["nanos"].as_f64().unwrap() / 1e9)
        * sensor["analogue_gain"].as_f64().unwrap()
}

fn golden(index: usize) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/data/dev-frame{index:03}.bin"));
    std::fs::read(path).expect("a golden")
}

/// Every frame's config bytes, by dev's index (see the module docs).
fn ours_all() -> Vec<Vec<u8>> {
    let lines = recording();
    let mut b = BackEndBuilder::new(&geometry()).expect("the geometry prepares");
    let mut out = Vec::new();
    let start = &lines[0]["start"];
    let mut isp: IspSettings =
        serde_json::from_value(start["isp"].clone()).expect("start settings");
    let mut expo = exposure(&start["sensor"]);
    let mut dump = |b: &BackEndBuilder| out.push(config_bytes(b.config()));
    b.update(&isp, expo * isp.flicker).expect("start frame");
    dump(&b);
    for line in &lines[1..] {
        if !line["sensor"].is_null() {
            expo = exposure(&line["sensor"]);
        }
        isp = serde_json::from_value(line["isp"].clone()).expect("frame settings");
        b.update(&isp, expo * isp.flicker).expect("frame");
        dump(&b);
    }
    out
}

fn config_bytes(c: &BeTilesConfig) -> Vec<u8> {
    let mut out = vec![0u8; std::mem::size_of::<BeTilesConfig>()];
    let n = c.write_to(&mut out).expect("the config fits its buffer");
    assert_eq!(n, out.len(), "Wire writes the whole config");
    out
}

/// Blocks whose bytes differ from dev's for a reason the library at 04d09360 makes, named here
/// (not written by Styx): see each reason. Every other block must match byte for byte.
const GAP_BLOCKS: [(&str, &str); 9] = [
    (
        "debin",
        "the library leaves the debin coefficients zero; dev writes its defaults with DEBIN off (no effect)",
    ),
    (
        "demosaic",
        "the demosaic parameters are the caller's and the library has no defaults at this rev; dev's are enabled",
    ),
    (
        "ycbcr",
        "the YCbCr encoding (libpisp's full-range BT.601 'jpeg') is not in the library at this rev; the CSC and YCbCr enables are off",
    ),
    (
        "ycbcr_inverse",
        "as ycbcr: the inverse encoding is not in the library at this rev",
    ),
    (
        "csc",
        "as ycbcr: output 0's colour space conversion needs the encoding the library does not carry",
    ),
    (
        "sharpen",
        "the sharpen defaults and scaling are not in the library at this rev; Styx refuses sharpening settings",
    ),
    (
        "sh_fc_combine",
        "as sharpen: the sharpen/false-colour combine is the library's default at dev's rev",
    ),
    (
        "false_colour",
        "the false-colour block is the library's default at dev's rev; its enable is off here",
    ),
    (
        "resample",
        "output 0's resampler coefficients are dev's defaults with output 0's resampler off; output 1 (the 2x2 box) must match",
    ),
];

/// `rgb_enables` bits dev sets that the library does not carry here: YCbCr, false colour,
/// sharpen, inverse YCbCr and CSC0 (see the gaps above).
const GAP_RGB: u32 = (1 << 3) | (1 << 4) | (1 << 5) | (1 << 7) | (1 << 9);

#[test]
fn back_end_bytes_match_devs_for_the_recorded_settings() {
    let ours = ours_all();
    assert_eq!(ours.len(), 81, "the start plus the recording's 80 frames");
    let size = std::mem::size_of::<BeTilesConfig>();
    let mut failures = String::new();
    for index in GOLDEN {
        let dev = golden(index);
        assert_eq!(dev.len(), size, "dev's size equals ours");
        let (a, b) = (&ours[index], &dev);
        for (name, off, len) in blocks() {
            if a[off..off + len] == b[off..off + len] {
                continue;
            }
            let gap = GAP_BLOCKS.iter().find(|g| g.0 == name);
            let ok = match name {
                // Only the gap bits of `rgb_enables` (bytes 4..8 of `global`) may differ.
                "global" => {
                    let w = |x: &[u8]| u32::from_le_bytes(x[4..8].try_into().unwrap());
                    let rgb = off..off + len;
                    a[rgb.start..rgb.start + 4] == b[rgb.start..rgb.start + 4]
                        && (w(&a[rgb.clone()]) & !GAP_RGB) == (w(&b[rgb.clone()]) & !GAP_RGB)
                        && a[rgb.start + 8..rgb.end] == b[rgb.start + 8..rgb.end]
                }
                // Output 1 (the box) must match: its coefficients follow output 0's in the block.
                "resample" => a[off + len / 2..off + len] == b[off + len / 2..off + len],
                _ => gap.is_some(),
            };
            if !ok {
                failures.push_str(&format!("index {index}, block {name}:\n"));
                failures.push_str(&diff_block(a, b, off, len));
            } else if std::env::var_os("STYX_PISP_SHOW_GAPS").is_some() && index == 0 {
                eprintln!(
                    "gap {name} at {off} (+{len}), index {index}:\n{}",
                    diff_block(a, b, off, len)
                );
            }
        }
    }
    assert!(
        failures.is_empty(),
        "back end bytes differ from dev's outside the named gaps:\n{failures}"
    );
}

/// The differing byte runs of one block (at most four), as [`diff`] shows them.
fn diff_block(a: &[u8], b: &[u8], off: usize, len: usize) -> String {
    let mut s = String::new();
    let mut i = 0;
    let mut runs = 0;
    while i < len {
        if a[off + i] == b[off + i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && a[off + i] != b[off + i] {
            i += 1;
        }
        runs += 1;
        if runs <= 4 {
            let show = |x: &[u8]| x[..x.len().min(8)].to_vec();
            s.push_str(&format!(
                "    bytes {}..{} ours {:02x?} dev {:02x?}\n",
                off + start,
                off + i,
                show(&a[off + start..off + i]),
                show(&b[off + start..off + i])
            ));
        }
    }
    s
}
