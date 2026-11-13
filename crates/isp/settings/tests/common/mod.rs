//! A settings value with every field set, shared by the tests.

#![allow(dead_code)]

use std::num::NonZeroU32;

use styx_aaa::{
    CdnParams, DenoiseParams, GeqParams, LensShading, Pwl, SdnParams, SharpenParams, TdnParams,
    ZoneGrid,
};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_settings::IspSettings;

pub fn res(w: u32, h: u32) -> Resolution {
    Resolution {
        width: NonZeroU32::new(w).unwrap(),
        height: NonZeroU32::new(h).unwrap(),
    }
}

pub fn rich() -> IspSettings {
    IspSettings {
        from_frame: 42,
        black_level: 0.0625,
        black_channels: Some([0.0625, 0.0630, 0.0640, 0.0650]),
        wb: [1.5, 1.0, 2.25],
        digital_gain: 1.125,
        flicker: 1.03125,
        flicker_bands: Some(vec![1.0, 1.25, 0.75]),
        ccm: [1.5, -0.25, -0.25, -0.125, 1.25, -0.125, 0.0, -0.5, 1.5],
        gamma: Some(Pwl::new(vec![(0.0, 0.0), (0.5, 0.25), (1.0, 1.0)]).unwrap()),
        lens_shading: Some(LensShading {
            width: 2,
            height: 2,
            r: vec![1.0, 1.125, 1.25, 1.5],
            g: vec![1.0, 1.0625, 1.0625, 1.125],
            b: vec![1.0, 1.25, 1.375, 1.625],
            generation: 7,
        }),
        denoise: DenoiseParams {
            noise_constant: 0.5,
            noise_slope: 0.25,
            sdn: Some(SdnParams {
                noise_constant: 1.0,
                noise_slope: 2.0,
                noise_constant2: 3.0,
                noise_slope2: 4.0,
                strength: 0.5,
            }),
            cdn: Some(CdnParams {
                threshold: 8.0,
                strength: 0.75,
            }),
            tdn: Some(TdnParams {
                noise_constant: 2.5,
                noise_slope: 0.5,
                threshold: 0.125,
            }),
            geq: Some(GeqParams {
                offset: 4.0,
                slope: 0.0625,
            }),
            dpc: 1,
        },
        sharpen: Some(SharpenParams {
            threshold: 1.25,
            strength: 0.75,
            limit: 2.0,
        }),
        histogram_weights: Some(ZoneGrid {
            width: 2,
            height: 2,
            zones: vec![0.0, 1.0, 2.0, 0.5],
        }),
        outputs: vec![
            MediaFormat::with_defaults(FourCc::NV12, res(1920, 1080)),
            MediaFormat::with_defaults(FourCc::YUYV, res(640, 480)),
        ],
    }
}
