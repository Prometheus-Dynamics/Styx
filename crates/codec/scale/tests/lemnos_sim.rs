//! The hardware converter on Lemnos's simulated backend, through the real socket.
//!
//! `lemnosd` runs in this process with the simulated image scaler (see `sim/mod.rs`). The
//! converter opens its job stream, writes the input image, posts the job and takes the completion;
//! every output is compared with the software path's scalar reference.

mod sim;
mod support;

use std::path::PathBuf;
use std::time::Duration;

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_codec_api::{CodecKind, Converter, Implementation, Registry, Request, Verdict};
use styx_format::FourCc;
use styx_scale::{Lemnos, Path, Software, reason, register_lemnos, register_software};

const CLIENT: &str = "styx-scale-test";

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-scale-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts a simulated scaler service named `name`, and returns it with its socket.
fn start(name: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: CLIENT.to_string(),
        board: parse(&sim::board()).expect("board"),
        drivers: vec![entry::<sim::SimScaler>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// Converts one frame on the software path (scalar reference) and returns its visible bytes.
fn reference(input: &styx_frame::FrameLease<()>, code: FourCc, w: u32, h: u32) -> Vec<u8> {
    let pool = support::pool(2);
    let mut out = support::blank(&pool, code, w, h);
    Software::new(code, code, Path::Scalar)
        .convert(input, &mut out)
        .expect("reference convert");
    support::visible(&out)
}

#[test]
fn hardware_output_equals_the_scalar_reference_for_every_format_and_size() {
    let (service, socket) = start("every");
    let pool = support::pool(4);
    let cases = [
        ((64u32, 48u32), (32u32, 24u32)),
        ((16, 12), (40, 30)),
        ((32, 16), (32, 16)),
    ];
    let mut total = 0u64;
    for &code in sim::FORMATS.iter() {
        let mut hw = Lemnos::open(&socket, CLIENT, code).expect("open the scaler");
        for &((iw, ih), (ow, oh)) in &cases {
            let input = support::frame(&pool, code, iw, ih, 11 + iw + oh);
            let mut out = support::blank(&pool, code, ow, oh);
            hw.convert(&input, &mut out)
                .unwrap_or_else(|e| panic!("{code} {iw}x{ih} to {ow}x{oh}: {e}"));
            assert_eq!(
                support::visible(&out),
                reference(&input, code, ow, oh),
                "{code} {iw}x{ih} to {ow}x{oh}"
            );
            total += 1;
        }
        let stats = hw.stats();
        assert_eq!(stats.jobs, cases.len() as u64, "{code}");
    }
    assert_eq!(total, (sim::FORMATS.len() * cases.len()) as u64);
    println!("hardware equals the scalar reference in {total} jobs on the simulated scaler");
    service.stop();
}

#[test]
fn a_format_the_scaler_does_not_take_is_unavailable() {
    let (service, socket) = start("refused");
    let err = Lemnos::open(&socket, CLIENT, FourCc::R8).expect_err("R8 is not taken");
    assert!(err.to_string().contains("does not take"), "{err}");
    service.stop();
}

#[test]
fn the_registry_prefers_the_hardware_and_gives_the_reason() {
    let (service, socket) = start("prefer");
    let mut registry: Registry<()> = Registry::new();
    register_lemnos(&mut registry, &socket, CLIENT);
    register_software(&mut registry, Path::Fast);
    let selection = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::NV12,
            FourCc::NV12,
        ))
        .expect("selected");
    assert_eq!(
        selection.implementation().descriptor().name,
        "lemnos-image-scaler"
    );
    let text = reason(&selection);
    assert!(
        text.starts_with("lemnos-image-scaler chosen on hardware"),
        "{text}"
    );

    let pool = support::pool(4);
    let input = support::frame(&pool, FourCc::NV12, 64, 48, 5);
    let mut out = support::blank(&pool, FourCc::NV12, 32, 24);
    let Implementation::Converter(mut converter) = selection.into_implementation() else {
        panic!("a converter");
    };
    converter
        .convert(&input, &mut out)
        .expect("convert on hardware");
    assert_eq!(
        support::visible(&out),
        reference(&input, FourCc::NV12, 32, 24)
    );
    service.stop();
}

#[test]
fn without_a_scaler_the_registry_says_why_and_takes_the_software() {
    let dir = socket_dir().join("absent");
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("no-service");
    let mut registry: Registry<()> = Registry::new();
    register_lemnos(&mut registry, &missing, CLIENT);
    register_software(&mut registry, Path::Fast);
    let selection = registry
        .select(Request::new(
            CodecKind::Converter,
            FourCc::NV12,
            FourCc::NV12,
        ))
        .expect("selected");
    assert_eq!(
        selection.implementation().descriptor().name,
        "styx-scale-software"
    );
    let hardware = selection
        .decisions()
        .iter()
        .find(|d| d.descriptor.name == "lemnos-image-scaler" && d.descriptor.input == FourCc::NV12)
        .expect("the NV12 hardware candidate was considered");
    match &hardware.verdict {
        Verdict::Skipped(why) => assert!(why.starts_with("unavailable"), "{why}"),
        other => panic!("hardware should be skipped, got {other:?}"),
    }
    assert!(reason(&selection).starts_with("styx-scale-software chosen on software"));
}
