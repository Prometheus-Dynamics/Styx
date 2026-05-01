use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

#[cfg(test)]
use std::collections::BTreeSet;

use styx_capture::prelude::{MediaFormat, Mode};
#[cfg(feature = "codec-jpeg-decoder")]
use styx_codec::prelude::MjpegDecoder;
use styx_codec::prelude::{
    Codec, CodecDescriptor, CodecError, CodecKind, CodecRegistry, PassthroughDecoder,
};
#[cfg(feature = "raw-decoders")]
use styx_codec::prelude::{Nv12ToRgbDecoder, YuyvToRgbDecoder};
use styx_core::prelude::{FourCc, Resolution};

use crate::frame_sizing::estimated_format_bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EncoderFamily {
    Turbojpeg,
    Mozjpeg,
    FfmpegMjpeg,
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = String))]
pub struct CodecSelector(String);

impl CodecSelector {
    /// Build a normalized runtime codec selector.
    ///
    /// Empty selectors are rejected. Matching remains case-insensitive at lookup
    /// boundaries, but storing selectors normalized avoids repeated ad hoc
    /// trimming in application code.
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = value.as_ref().trim();
        (!value.is_empty()).then(|| Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl EncoderFamily {
    pub fn spec(self) -> &'static EncoderFamilySpec {
        ENCODER_FAMILY_SPECS
            .iter()
            .find(|spec| spec.family == self)
            .expect("encoder family spec registered")
    }

    pub fn from_selector(selector: &CodecSelector) -> Option<Self> {
        let matches: Vec<_> = ENCODER_FAMILY_SPECS
            .iter()
            .filter(|spec| spec.matches_selector(selector))
            .map(|spec| spec.family)
            .collect();

        (matches.len() == 1).then_some(matches[0])
    }
}

impl std::fmt::Display for CodecSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for CodecSelector {
    type Err = CodecSelectorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value).ok_or(CodecSelectorParseError)
    }
}

impl TryFrom<&str> for CodecSelector {
    type Error = CodecSelectorParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<String> for CodecSelector {
    type Error = CodecSelectorParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl AsRef<str> for CodecSelector {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecSelectorParseError;

impl std::fmt::Display for CodecSelectorParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("codec selector cannot be empty")
    }
}

impl std::error::Error for CodecSelectorParseError {}

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

impl EncoderFamilySpec {
    pub fn selector(&self) -> CodecSelector {
        CodecSelector::new(self.selector_id).expect("encoder selector id is nonempty")
    }

    pub fn matches_selector(&self, selector: &CodecSelector) -> bool {
        let selector = selector.as_str();
        self.id == selector
            || self.selector_id == selector
            || self.selector_aliases.contains(&selector)
            || self.runtime_implementation_aliases.contains(&selector)
            || self.runtime_name_aliases.contains(&selector)
    }
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

static DEFAULT_STREAM_CODEC_SELECTOR: OnceLock<Option<CodecSelector>> = OnceLock::new();
static DEFAULT_DECODER_SELECTORS_BY_CAPTURE_FORMAT: OnceLock<BTreeMap<String, CodecSelector>> =
    OnceLock::new();

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

#[derive(Clone)]
pub struct FrameDecodePlan {
    pub decoder: Arc<dyn Codec>,
    pub shared_output_bytes: usize,
}

impl std::fmt::Debug for FrameDecodePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameDecodePlan")
            .field("decoder", self.decoder.descriptor())
            .field("shared_output_bytes", &self.shared_output_bytes)
            .finish()
    }
}

impl FrameDecodePlan {
    pub fn new(decoder: Arc<dyn Codec>, shared_output_bytes: usize) -> Self {
        Self {
            decoder,
            shared_output_bytes,
        }
    }
}

pub trait FrameDecodePlanExt {
    fn decode_to_rg24(&self) -> FrameDecodePlan;
}

impl FrameDecodePlanExt for Mode {
    fn decode_to_rg24(&self) -> FrameDecodePlan {
        decode_to_rg24_for_format(self.format)
    }
}

impl FrameDecodePlanExt for MediaFormat {
    fn decode_to_rg24(&self) -> FrameDecodePlan {
        decode_to_rg24_for_format(*self)
    }
}

pub fn decode_to_rg24_for_format(format: MediaFormat) -> FrameDecodePlan {
    let res = format.resolution;
    #[cfg(feature = "raw-decoders")]
    let width = res.width.get();
    #[cfg(feature = "raw-decoders")]
    let height = res.height.get();
    let code = format.code;
    #[cfg(feature = "raw-decoders")]
    let decoder: Arc<dyn Codec> = if code == FourCc::YUYV {
        Arc::new(YuyvToRgbDecoder::new(width, height))
    } else if code == FourCc::NV12 {
        Arc::new(Nv12ToRgbDecoder::new(width, height))
    } else {
        decode_to_rg24_fallback_decoder(code)
    };

    #[cfg(not(feature = "raw-decoders"))]
    let decoder: Arc<dyn Codec> = decode_to_rg24_fallback_decoder(code);

    FrameDecodePlan::new(decoder, shared_rg24_decode_bytes(res))
}

fn decode_to_rg24_fallback_decoder(code: FourCc) -> Arc<dyn Codec> {
    #[cfg(feature = "codec-jpeg-decoder")]
    if code.is_jpeg_encoded() {
        Arc::new(MjpegDecoder::new_for_input(code, FourCc::RG24))
    } else {
        Arc::new(PassthroughDecoder::new(code))
    }
    #[cfg(not(feature = "codec-jpeg-decoder"))]
    {
        Arc::new(PassthroughDecoder::new(code))
    }
}

pub fn shared_rg24_decode_bytes(resolution: Resolution) -> usize {
    estimated_format_bytes(
        FourCc::RG24,
        resolution.width.get() as usize,
        resolution.height.get() as usize,
    )
    .unwrap_or(64 * 1024)
    .max(64 * 1024)
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
    let selector = CodecSelector::new(selector)?;
    encoder_family_for_codec_selector(&selector)
}

pub fn encoder_family_for_codec_selector(
    selector: &CodecSelector,
) -> Option<&'static EncoderFamilySpec> {
    EncoderFamily::from_selector(selector).map(EncoderFamily::spec)
}

pub fn preview_format_for_encoder_selector(selector: Option<&str>) -> &'static str {
    selector
        .and_then(encoder_family_for_selector)
        .map(|spec| spec.preview_format)
        .unwrap_or("unknown")
}

pub fn preview_format_for_codec_selector(selector: Option<&CodecSelector>) -> &'static str {
    selector
        .and_then(encoder_family_for_codec_selector)
        .map(|spec| spec.preview_format)
        .unwrap_or("unknown")
}

pub fn default_stream_encoder_selector() -> Option<String> {
    default_stream_codec_selector().map(|selector| selector.to_string())
}

pub fn default_stream_codec_selector() -> Option<CodecSelector> {
    DEFAULT_STREAM_CODEC_SELECTOR
        .get_or_init(compute_default_stream_codec_selector)
        .clone()
}

fn compute_default_stream_codec_selector() -> Option<CodecSelector> {
    let preferred_input = FourCc::RG24;
    let entries = CodecRegistry::list_enabled_encoders().ok()?;
    let mut preferred_mjpeg: Option<CodecSelector> = None;
    let mut fallback_mjpeg: Option<CodecSelector> = None;
    let mut fallback_any: Option<CodecSelector> = None;

    for (input, codecs) in entries {
        if input != preferred_input {
            continue;
        }
        for desc in codecs {
            if desc.kind != CodecKind::Encoder {
                continue;
            }
            let selector = encoder_family_for_descriptor(&desc)
                .map(|spec| spec.selector())
                .or_else(|| CodecSelector::new(desc.impl_name))?;
            if fallback_any.is_none() {
                fallback_any = Some(selector.clone());
            }
            if let Some(spec) = encoder_family_for_descriptor(&desc) {
                if spec.family == EncoderFamily::Turbojpeg {
                    preferred_mjpeg = Some(spec.selector());
                    break;
                }
                if spec.preview_format == "mjpeg" && fallback_mjpeg.is_none() {
                    fallback_mjpeg = Some(spec.selector());
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

fn default_decoder_selector_for_codec(descs: &[CodecDescriptor]) -> Option<CodecSelector> {
    if descs.is_empty() {
        return None;
    }

    if let Some(codec) = descs.iter().find(|desc| {
        desc.name.eq_ignore_ascii_case("mjpeg") && desc.impl_name.eq_ignore_ascii_case("turbojpeg")
    }) {
        return CodecSelector::new(codec.impl_name);
    }

    match descs[0].input.to_u32().to_le_bytes() {
        [b'H', b'2', b'6', b'4'] => return CodecSelector::new("h264"),
        [b'H', b'2', b'6', b'5'] | [b'H', b'E', b'V', b'C'] => {
            return CodecSelector::new("h265");
        }
        [b'M', b'J', b'P', b'G'] | [b'J', b'P', b'E', b'G'] => {}
        _ => {}
    }

    if let Some(codec) = descs
        .iter()
        .find(|desc| desc.impl_name.eq_ignore_ascii_case("passthrough"))
    {
        return CodecSelector::new(codec.impl_name);
    }

    descs
        .first()
        .and_then(|desc| CodecSelector::new(desc.impl_name))
}

pub fn default_decoder_ids_by_capture_format() -> BTreeMap<String, String> {
    default_decoder_selectors_by_capture_format()
        .into_iter()
        .map(|(key, selector)| (key, selector.to_string()))
        .collect()
}

pub fn default_decoder_selectors_by_capture_format() -> BTreeMap<String, CodecSelector> {
    DEFAULT_DECODER_SELECTORS_BY_CAPTURE_FORMAT
        .get_or_init(compute_default_decoder_selectors_by_capture_format)
        .clone()
}

fn compute_default_decoder_selectors_by_capture_format() -> BTreeMap<String, CodecSelector> {
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
    default_decoder_codec_selector_for_capture_format(fourcc).map(|selector| selector.to_string())
}

pub fn default_decoder_codec_selector_for_capture_format(fourcc: FourCc) -> Option<CodecSelector> {
    let defaults = default_decoder_selectors_by_capture_format();
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
    fn codec_selector_normalizes_and_rejects_empty_values() {
        let selector: CodecSelector = " H264_V4L2M2M ".parse().expect("selector");
        assert_eq!(selector.as_str(), "h264_v4l2m2m");
        assert_eq!(
            encoder_family_for_codec_selector(&selector).map(|spec| spec.id),
            Some("h264")
        );
        assert_eq!(preview_format_for_codec_selector(Some(&selector)), "h264");
        assert!("  ".parse::<CodecSelector>().is_err());
    }

    #[test]
    fn default_decoder_selection_returns_typed_selectors() {
        let descs = [CodecDescriptor {
            kind: CodecKind::Decoder,
            input: FourCc::H264,
            output: FourCc::RG24,
            name: "h264",
            impl_name: "ffmpeg",
        }];

        let selector = default_decoder_selector_for_codec(&descs).expect("selector");
        assert_eq!(selector.as_str(), "h264");
    }

    #[test]
    fn encoder_family_ids_are_unique() {
        let ids: BTreeSet<_> = ENCODER_FAMILY_SPECS.iter().map(|spec| spec.id).collect();
        assert_eq!(ids.len(), ENCODER_FAMILY_SPECS.len());
    }
}
