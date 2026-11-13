//! Scalers and format converters for Styx: resize, and channel or chroma-order conversion, as
//! [`styx_codec_api::Converter`]s.
//!
//! Two implementations of the same request:
//!
//! - [`Lemnos`] (`lemnos-image-scaler`): the chip's `image.scaler/1` through Lemnos, for a device
//!   that provides the contract (the RGA, the G2D). It takes one format in and out, the same, and
//!   changes only the size.
//! - [`Software`] (`styx-scale-software`): on the CPU. Any resize, and the exact conversions
//!   between formats of one family (see [`layout::convertible`]). `styx-simd` rows where they fit,
//!   scalar code elsewhere. YUV to RGB needs a colour matrix and is not here yet.
//!
//! The registry takes candidates in registration order within a backend, and both are native.
//! Register the hardware first with [`register_lemnos`], then the software with
//! [`register_software`]: the registry tries the hardware, and when it cannot be built (no service,
//! no scaler, the format not taken) its decision says why, and the software is chosen. [`reason`]
//! states that for a selection.
//!
//! Per-frame calls allocate nothing once the converter has run a frame of its size. The hardware
//! path copies the input into the stream's input buffer and the completion into the output frame;
//! [`Stats`] counts both.

mod kernel;
pub mod layout;
mod lemnos;
mod software;

use std::path::PathBuf;

use styx_codec_api::{Candidate, CodecError, Implementation, Registry, Selection};

pub use kernel::{Path, convert_pixels, resample};
pub use layout::{Filter, Kind, Rgb};
pub use lemnos::{
    IMAGE_SCALER_TAG, IN_BUFFER, JOB_TIMEOUT, Lemnos, NAME as LEMNOS_NAME, OUT_BUFFER, Stats,
};
pub use software::{NAME as SOFTWARE_NAME, Software};

/// Registers the hardware converter for every format the layout module handles, through the
/// Lemnos service at `socket` as `client_name`. Each is tried on its own, when a request names
/// its format; a candidate whose service or scaler is missing is skipped with the reason.
pub fn register_lemnos<M: 'static>(
    registry: &mut Registry<M>,
    socket: impl Into<PathBuf>,
    client_name: impl Into<String>,
) {
    let socket: PathBuf = socket.into();
    let client_name: String = client_name.into();
    for &code in layout::FORMATS.iter() {
        let (socket, client_name) = (socket.clone(), client_name.clone());
        let candidate = Candidate::new(lemnos::descriptor(code), move || {
            Ok(Implementation::Converter(Box::new(Lemnos::open(
                &socket,
                &client_name,
                code,
            )?)))
        });
        registry.register(candidate);
    }
}

/// Registers the software converter for every pair of formats [`layout::convertible`] accepts,
/// on `path`.
pub fn register_software<M: 'static>(registry: &mut Registry<M>, path: Path) {
    for &input in layout::FORMATS.iter() {
        for &output in layout::FORMATS.iter() {
            if !layout::convertible(input, output) {
                continue;
            }
            let descriptor = software::describe(input, output);
            registry.register(Candidate::new(
                descriptor,
                move || -> Result<Implementation<M>, CodecError> {
                    Ok(Implementation::Converter(Box::new(Software::new(
                        input, output, path,
                    ))))
                },
            ));
        }
    }
}

/// What a selection chose, and why: the chosen implementation's name and where it runs, then
/// every candidate's decision in the order the registry considered them.
pub fn reason<M>(selection: &Selection<M>) -> String {
    let chosen = selection.implementation().descriptor().name;
    let where_ = if chosen == LEMNOS_NAME {
        "hardware"
    } else {
        "software"
    };
    let decisions: Vec<String> = selection
        .decisions()
        .iter()
        .map(|d| d.to_string())
        .collect();
    format!("{chosen} chosen on {where_}; {}", decisions.join("; "))
}
