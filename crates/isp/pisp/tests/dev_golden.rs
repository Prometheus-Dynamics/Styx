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

use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_pisp::pisp::format::formats;
use styx_pisp::pisp::uapi::{BayerOrder, BeConfig, BeTilesConfig, ImageFormatConfig, Wire};
use styx_pisp::{BackEndBuilder, Geometry, IspSettings, Output};

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

fn output(code: FourCc, width: u32, height: u32, color: ColorSpace, align: u32) -> Output {
    Output {
        media: MediaFormat::new(
            code,
            Resolution::new(width, height).expect("non-empty"),
            color,
            ColorRange::Full,
        ),
        stride_align: align,
    }
}

fn geometry() -> Geometry {
    Geometry {
        input: input(),
        bayer: BayerOrder::Rggb,
        outputs: [
            output(FourCc::NV12, 1280, 800, ColorSpace::Bt601, 64),
            output(FourCc::RGB3, 640, 400, ColorSpace::Srgb, 64),
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

#[test]
fn back_end_bytes_match_devs_for_the_recorded_settings() {
    let ours = ours_all();
    assert_eq!(ours.len(), 81, "the start plus the recording's 80 frames");
    let size = std::mem::size_of::<BeTilesConfig>();
    let mut failures = String::new();
    let mut report = String::new();
    for index in GOLDEN {
        let dev = golden(index);
        assert_eq!(dev.len(), size, "dev's size equals ours");
        let (a, b) = (&ours[index], &dev);
        for (name, off, len) in blocks() {
            if a[off..off + len] == b[off..off + len] {
                report.push_str(&format!("index {index}: {name:<16} equal\n"));
                continue;
            }
            failures.push_str(&format!("index {index}, block {name}:\n"));
            failures.push_str(&diff_block(a, b, off, len));
        }
    }
    if std::env::var_os("STYX_PISP_SHOW_BLOCKS").is_some() {
        eprintln!("{report}");
    }
    assert!(
        failures.is_empty(),
        "back end bytes differ from dev's:\n{failures}"
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
