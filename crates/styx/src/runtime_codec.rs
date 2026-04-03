use std::collections::BTreeMap;

#[cfg(test)]
use std::collections::BTreeSet;

use styx_codec::{CodecDescriptor, CodecError, CodecKind, CodecRegistry};
use styx_core::prelude::FourCc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EncoderFamily {
    Turbojpeg,
    Mozjpeg,
    FfmpegMjpeg,
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderFamilySpec {
    pub family: EncoderFamily,
    pub id: &'static str,
    pub selector_id: &'static str,
    pub selector_aliases: &'static [&'static str],
    pub runtime_implementation_aliases: &'static [&'static str],
    pub runtime_name_aliases: &'static [&'static str],
    pub output_fourcc_aliases: &'static [&'static str],
    pub preview_format: &'static str,
    pub recording_codec: Option<&'static str>,
}

pub const ENCODER_FAMILY_SPECS: &[EncoderFamilySpec] = &[
    EncoderFamilySpec {
        family: EncoderFamily::Turbojpeg,
        id: "turbojpeg",
        selector_id: "turbojpeg",
        selector_aliases: &["turbojpeg"],
        runtime_implementation_aliases: &["turbojpeg"],
        runtime_name_aliases: &[],
        output_fourcc_aliases: &["MJPG", "JPEG"],
        preview_format: "mjpeg",
        recording_codec: None,
    },
    EncoderFamilySpec {
        family: EncoderFamily::Mozjpeg,
        id: "mozjpeg",
        selector_id: "mozjpeg",
        selector_aliases: &["mozjpeg"],
        runtime_implementation_aliases: &["mozjpeg"],
        runtime_name_aliases: &[],
        output_fourcc_aliases: &["MJPG", "JPEG"],
        preview_format: "mjpeg",
        recording_codec: None,
    },
    EncoderFamilySpec {
        family: EncoderFamily::FfmpegMjpeg,
        id: "ffmpeg_mjpeg",
        selector_id: "mjpeg",
        selector_aliases: &["mjpeg", "mjpg", "jpeg"],
        runtime_implementation_aliases: &["ffmpeg"],
        runtime_name_aliases: &["mjpeg"],
        output_fourcc_aliases: &["MJPG", "JPEG"],
        preview_format: "mjpeg",
        recording_codec: None,
    },
    EncoderFamilySpec {
        family: EncoderFamily::H264,
        id: "h264",
        selector_id: "h264",
        selector_aliases: &["h264", "avc"],
        runtime_implementation_aliases: &["ffmpeg", "h264_v4l2m2m"],
        runtime_name_aliases: &["h264", "avc"],
        output_fourcc_aliases: &["H264"],
        preview_format: "h264",
        recording_codec: Some("h264"),
    },
    EncoderFamilySpec {
        family: EncoderFamily::H265,
        id: "h265",
        selector_id: "h265",
        selector_aliases: &["h265", "hevc"],
        runtime_implementation_aliases: &["ffmpeg", "hevc_v4l2m2m", "h265_v4l2m2m"],
        runtime_name_aliases: &["h265", "hevc"],
        output_fourcc_aliases: &["H265", "HEVC"],
        preview_format: "h265",
        recording_codec: Some("h265"),
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCodecCapability {
    pub kind: CodecKind,
    pub fourcc: String,
    pub name: String,
    pub implementation: String,
    pub input: String,
    pub output: String,
    pub family_id: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCodecInventory {
    pub codecs: Vec<RuntimeCodecCapability>,
    pub default_encoder_selector: Option<String>,
    pub default_decoder_ids_by_capture_format: BTreeMap<String, String>,
}

pub fn encoder_family_for_descriptor(desc: &CodecDescriptor) -> Option<&'static EncoderFamilySpec> {
    if desc.kind != CodecKind::Encoder {
        return None;
    }

    ENCODER_FAMILY_SPECS.iter().find(|spec| {
        let implementation_matches = spec
            .runtime_implementation_aliases
            .iter()
            .any(|alias| desc.impl_name.eq_ignore_ascii_case(alias));
        if !implementation_matches {
            return false;
        }
        if !desc.impl_name.eq_ignore_ascii_case("ffmpeg") {
            return true;
        }

        let output = desc.output.to_string();
        spec.runtime_name_aliases
            .iter()
            .any(|alias| desc.name.eq_ignore_ascii_case(alias))
            || spec
                .output_fourcc_aliases
                .iter()
                .any(|alias| output.eq_ignore_ascii_case(alias))
    })
}

pub fn encoder_family_for_selector(selector: &str) -> Option<&'static EncoderFamilySpec> {
    let selector = selector.trim();
    if selector.is_empty() {
        return None;
    }

    let matches: Vec<_> = ENCODER_FAMILY_SPECS
        .iter()
        .filter(|spec| {
            spec.id.eq_ignore_ascii_case(selector)
                || spec.selector_id.eq_ignore_ascii_case(selector)
                || spec
                    .selector_aliases
                    .iter()
                    .any(|alias| selector.eq_ignore_ascii_case(alias))
                || spec
                    .runtime_implementation_aliases
                    .iter()
                    .any(|alias| selector.eq_ignore_ascii_case(alias))
                || spec
                    .runtime_name_aliases
                    .iter()
                    .any(|alias| selector.eq_ignore_ascii_case(alias))
        })
        .collect();

    (matches.len() == 1).then_some(matches[0])
}

pub fn preview_format_for_encoder_selector(selector: Option<&str>) -> &'static str {
    selector
        .and_then(encoder_family_for_selector)
        .map(|spec| spec.preview_format)
        .unwrap_or("unknown")
}

pub fn default_stream_encoder_selector() -> Option<String> {
    let preferred_input = FourCc::new(*b"RG24");
    let entries = CodecRegistry::list_enabled_encoders().ok()?;
    let mut preferred_mjpeg: Option<String> = None;
    let mut fallback_mjpeg: Option<String> = None;
    let mut fallback_any: Option<String> = None;

    for (input, codecs) in entries {
        if input != preferred_input {
            continue;
        }
        for desc in codecs {
            if desc.kind != CodecKind::Encoder {
                continue;
            }
            let selector = encoder_family_for_descriptor(&desc)
                .map(|spec| spec.selector_id.to_string())
                .unwrap_or_else(|| desc.impl_name.to_string());
            if fallback_any.is_none() {
                fallback_any = Some(selector.clone());
            }
            if let Some(spec) = encoder_family_for_descriptor(&desc) {
                if spec.family == EncoderFamily::Turbojpeg {
                    preferred_mjpeg = Some(spec.selector_id.to_string());
                    break;
                }
                if spec.preview_format == "mjpeg" && fallback_mjpeg.is_none() {
                    fallback_mjpeg = Some(spec.selector_id.to_string());
                }
            } else if desc.name.eq_ignore_ascii_case("mjpeg") && fallback_mjpeg.is_none() {
                fallback_mjpeg = Some(selector);
            }
        }
        if preferred_mjpeg.is_some() {
            break;
        }
    }

    preferred_mjpeg.or(fallback_mjpeg).or(fallback_any)
}

fn default_decoder_selector_for_codec(descs: &[CodecDescriptor]) -> Option<String> {
    if descs.is_empty() {
        return None;
    }

    if let Some(codec) = descs.iter().find(|desc| {
        desc.name.eq_ignore_ascii_case("mjpeg") && desc.impl_name.eq_ignore_ascii_case("turbojpeg")
    }) {
        return Some(codec.impl_name.to_string());
    }

    match descs[0].input.to_u32().to_le_bytes() {
        [b'H', b'2', b'6', b'4'] => return Some("h264".to_string()),
        [b'H', b'2', b'6', b'5'] | [b'H', b'E', b'V', b'C'] => return Some("h265".to_string()),
        [b'M', b'J', b'P', b'G'] | [b'J', b'P', b'E', b'G'] => {}
        _ => {}
    }

    if let Some(codec) = descs
        .iter()
        .find(|desc| desc.impl_name.eq_ignore_ascii_case("passthrough"))
    {
        return Some(codec.impl_name.to_string());
    }

    descs.first().map(|desc| desc.impl_name.to_string())
}

pub fn default_decoder_ids_by_capture_format() -> BTreeMap<String, String> {
    let mut defaults = BTreeMap::new();
    let Ok(entries) = CodecRegistry::list_enabled_codecs() else {
        return defaults;
    };

    for (input, codecs) in entries {
        let decoder_descs: Vec<_> = codecs
            .into_iter()
            .filter(|desc| desc.kind == CodecKind::Decoder)
            .collect();
        if decoder_descs.is_empty() {
            continue;
        }
        let key = String::from_utf8_lossy(&input.to_u32().to_le_bytes())
            .trim()
            .to_ascii_uppercase();
        if key.is_empty() {
            continue;
        }
        if let Some(selector) = default_decoder_selector_for_codec(&decoder_descs) {
            defaults.insert(key, selector);
        }
    }

    defaults
}

pub fn default_decoder_selector_for_capture_format(fourcc: FourCc) -> Option<String> {
    let defaults = default_decoder_ids_by_capture_format();
    let key = String::from_utf8_lossy(&fourcc.to_u32().to_le_bytes())
        .trim()
        .to_ascii_uppercase();
    defaults
        .get(&key)
        .cloned()
        .or_else(|| defaults.get("ANY").cloned())
}

pub fn runtime_codec_inventory() -> Result<RuntimeCodecInventory, CodecError> {
    let mut codecs: Vec<RuntimeCodecCapability> = CodecRegistry::list_enabled_codecs()?
        .into_iter()
        .flat_map(|(fourcc, descs)| {
            descs.into_iter().map(move |desc| RuntimeCodecCapability {
                kind: desc.kind,
                fourcc: fourcc.to_string(),
                name: desc.name.to_string(),
                implementation: desc.impl_name.to_string(),
                input: desc.input.to_string(),
                output: desc.output.to_string(),
                family_id: encoder_family_for_descriptor(&desc).map(|spec| spec.id),
            })
        })
        .collect();

    codecs.sort_by(|left, right| {
        let left_kind = match left.kind {
            CodecKind::Decoder => 0u8,
            CodecKind::Encoder => 1u8,
        };
        let right_kind = match right.kind {
            CodecKind::Decoder => 0u8,
            CodecKind::Encoder => 1u8,
        };
        left_kind
            .cmp(&right_kind)
            .then_with(|| left.fourcc.cmp(&right.fourcc))
            .then_with(|| left.input.cmp(&right.input))
            .then_with(|| left.output.cmp(&right.output))
            .then_with(|| left.implementation.cmp(&right.implementation))
    });

    Ok(RuntimeCodecInventory {
        codecs,
        default_encoder_selector: default_stream_encoder_selector(),
        default_decoder_ids_by_capture_format: default_decoder_ids_by_capture_format(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_family_selector_aliases_resolve_runtime_variants() {
        assert_eq!(
            encoder_family_for_selector("h264_v4l2m2m").map(|spec| spec.id),
            Some("h264")
        );
        assert_eq!(
            encoder_family_for_selector("hevc_v4l2m2m").map(|spec| spec.id),
            Some("h265")
        );
        assert_eq!(
            encoder_family_for_selector("mjpeg").map(|spec| spec.id),
            Some("ffmpeg_mjpeg")
        );
        assert_eq!(encoder_family_for_selector("ffmpeg"), None);
    }

    #[test]
    fn preview_format_matches_runtime_aliases() {
        assert_eq!(
            preview_format_for_encoder_selector(Some("turbojpeg")),
            "mjpeg"
        );
        assert_eq!(
            preview_format_for_encoder_selector(Some("h264_v4l2m2m")),
            "h264"
        );
        assert_eq!(
            preview_format_for_encoder_selector(Some("hevc_v4l2m2m")),
            "h265"
        );
    }

    #[test]
    fn encoder_family_ids_are_unique() {
        let ids: BTreeSet<_> = ENCODER_FAMILY_SPECS.iter().map(|spec| spec.id).collect();
        assert_eq!(ids.len(), ENCODER_FAMILY_SPECS.len());
    }
}
