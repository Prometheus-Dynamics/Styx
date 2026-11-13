//! Unit tests: the layouts' sizes, the generation's refusals, the reuse of one buffer, and the job
//! the contract encodes.

use lemnos_contract_camera_isp::{IspJob, NO_BUFFER};
use lemnos_core::{Decode, Encode, Reader, Writer};
use styx_settings::IspSettings;

use crate::{Error, Generation, Params, statistics};
use styx_aaa::Statistics;

#[test]
fn buffers_are_the_library_layouts() {
    assert_eq!(Params::new(Generation::Isp21).as_bytes().len(), 10475);
    assert_eq!(Params::new(Generation::Isp30).as_bytes().len(), 10921);
    assert_eq!(Params::new(Generation::Isp35).as_bytes().len(), 13065);
    assert!(
        Params::new(Generation::Isp21)
            .as_bytes()
            .iter()
            .all(|&b| b == 0)
    );
}

#[test]
fn isp35_is_refused_and_left_cleared() {
    let mut p = Params::new(Generation::Isp35);
    let err = p
        .apply(&IspSettings::neutral(0.0), 0, 640, 480)
        .expect_err("ISP35 is not mapped");
    assert!(matches!(err, Error::Unmapped(Generation::Isp35)));
    assert!(p.as_bytes().iter().all(|&b| b == 0));
    let mut out = Statistics::default();
    assert!(statistics(Generation::Isp35, &[0; 14542], 1, &mut out).is_none());
}

#[test]
fn a_reused_buffer_gives_the_same_bytes_as_a_fresh_one() {
    let settings = IspSettings {
        wb: [2.0, 1.0, 1.5],
        digital_gain: 1.25,
        ..IspSettings::neutral(64.0 / 1024.0)
    };
    for g in [Generation::Isp21, Generation::Isp30] {
        let mut fresh = Params::new(g);
        fresh.apply(&settings, 3, 1280, 800).unwrap();
        let mut reused = Params::new(g);
        reused.apply(&settings, 9, 640, 480).unwrap();
        reused.apply(&settings, 3, 1280, 800).unwrap();
        assert_eq!(reused.as_bytes(), fresh.as_bytes(), "{g:?}");
    }
}

#[test]
fn lens_shading_of_the_wrong_size_leaves_its_block_off() {
    let mut settings = IspSettings::neutral(0.0);
    settings.lens_shading = Some(styx_aaa::LensShading {
        width: 2,
        height: 2,
        r: vec![1.0; 4],
        g: vec![1.0; 3],
        b: vec![1.0; 4],
        generation: 0,
    });
    let mut p = Params::new(Generation::Isp21);
    p.apply(&settings, 0, 640, 480).unwrap();
    let mut without = Params::new(Generation::Isp21);
    without
        .apply(&IspSettings::neutral(0.0), 0, 640, 480)
        .unwrap();
    assert_eq!(p.as_bytes(), without.as_bytes());
}

#[test]
fn a_job_encodes_and_decodes_at_the_contract_size() {
    let job = IspJob {
        identity: 42,
        layout: 0x2101,
        config_buffer: 3,
        input_buffer: NO_BUFFER,
        outputs: [1, NO_BUFFER, NO_BUFFER, NO_BUFFER],
    };
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    job.encode(&mut w).unwrap();
    let n = w.len();
    assert!(n <= 64);
    let back = IspJob::decode(&mut Reader::new(&buf[..n])).unwrap();
    assert_eq!(back, job);
}
