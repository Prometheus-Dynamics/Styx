use std::collections::BTreeSet;

use styx_codec::prelude::{CodecDescriptor, CodecKind, CodecRegistry, CodecRegistryConfig};
use styx_core::prelude::{FrameResidency, packed_transform_residency_capabilities};

use crate::capture_api::StyxConfig;
use crate::{BackendKind, ProbedDevice};

/// Snapshot of the capture, codec, transform, and frame-backing surface that Styx can expose.
///
/// The inventory is intentionally serializable as plain Rust data so it can be registered with
/// graph planners, inspected in tests, or rendered by service layers without binding callers to
/// a specific scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxCapabilityInventory {
    /// Capture backends discovered from probed devices.
    pub capture_backends: Vec<CaptureBackendCapability>,
    /// Codecs currently registered in the runtime codec registry.
    pub codecs: Vec<CodecCapability>,
    /// Built-in frame transforms and their residency requirements.
    pub transforms: Vec<TransformCapability>,
    /// Known frame backing types and their handoff/export behavior.
    pub backing: Vec<FrameBackingCapability>,
}

/// Planner-facing description of one capture backend and the formats it can provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureBackendCapability {
    /// Backend implementation that owns the advertised descriptor.
    pub backend: BackendKind,
    /// Unique FourCC strings advertised by the backend descriptor.
    pub formats: Vec<String>,
    /// Frame residencies the backend can produce without forcing a host copy.
    pub zero_copy_residencies: Vec<FrameResidency>,
    /// Whether frames from this backend can be exported across a process boundary.
    pub exportable: bool,
    /// Export strategies supported by this backend.
    pub export_modes: Vec<CrossProcessExportMode>,
    /// Whether cross-process handoff can preserve zero-copy semantics.
    pub zero_copy_cross_process: bool,
    /// Human-readable backend caveats for planners and diagnostics.
    pub notes: Vec<String>,
}

/// Cross-process frame export strategy for a capture or backing path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrossProcessExportMode {
    /// Export an existing DMA-BUF file descriptor.
    Dmabuf,
    /// Export shared memory through `memfd`.
    Memfd,
    /// Copy frame bytes into a new `memfd`.
    CopyToMemfd,
    /// No cross-process export path is available.
    NotExportable,
}

/// Planner-facing description of a codec implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecCapability {
    /// Decoder or encoder.
    pub kind: CodecKind,
    /// Public codec name.
    pub name: String,
    /// Implementation/backend identifier.
    pub implementation: String,
    /// Input FourCC string.
    pub input: String,
    /// Output FourCC string.
    pub output: String,
    /// Frame residencies accepted without a pre-codec materialization step.
    pub accepted_inputs: Vec<FrameResidency>,
    /// Frame residencies the codec may produce.
    pub possible_outputs: Vec<FrameResidency>,
    /// Whether the codec keeps the input frame backing unchanged.
    pub preserves_input_residency: bool,
    /// Whether the implementation is expected to use hardware acceleration.
    pub hardware_accelerated: bool,
    /// Whether the codec output can be exported across a process boundary.
    pub exportable_output_possible: bool,
}

/// Built-in frame transform capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformCapability {
    /// Stable transform identifier.
    pub id: String,
    /// Accepted input residencies.
    pub accepted_inputs: Vec<FrameResidency>,
    /// Possible output residencies.
    pub possible_outputs: Vec<FrameResidency>,
    /// Whether the transform requires a new buffer.
    pub copy_required: bool,
}

/// Known frame backing capability for handoff and export planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBackingCapability {
    /// Stable backing identifier.
    pub id: String,
    /// Residency represented by the backing type.
    pub residency: FrameResidency,
    /// Whether the backing can cross a process boundary.
    pub cross_process_export: bool,
    /// Whether graph handoff can borrow/share this backing without copying.
    pub zero_copy_graph_handoff: bool,
    /// Export strategy for this backing.
    pub export_mode: CrossProcessExportMode,
}

/// Requested media path used to explain whether Styx can satisfy a capture/codec route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxPathRequest {
    /// Required input FourCC.
    pub input: String,
    /// Optional output FourCC.
    pub output: Option<String>,
    /// Reject paths that cannot export the chosen output.
    pub require_exportable: bool,
    /// Record a rejection when a path cannot stay fully zero-copy.
    pub prefer_zero_copy: bool,
}

impl StyxPathRequest {
    /// Create a path request for an input FourCC.
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: normalize_fourcc(input.into()),
            output: None,
            require_exportable: false,
            prefer_zero_copy: true,
        }
    }

    /// Require the path to produce this output FourCC.
    pub fn output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(normalize_fourcc(output.into()));
        self
    }

    /// Require exportable output when choosing capture and codec capabilities.
    pub fn require_exportable(mut self, require: bool) -> Self {
        self.require_exportable = require;
        self
    }

    /// Prefer zero-copy and report why that preference cannot be met.
    pub fn prefer_zero_copy(mut self, prefer: bool) -> Self {
        self.prefer_zero_copy = prefer;
        self
    }
}

/// Result of explaining a requested media path against a capability inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxPathPlan {
    /// Whether the inventory can satisfy the request.
    pub accepted: bool,
    /// Whether every selected step can preserve zero-copy behavior.
    pub zero_copy_possible: bool,
    /// Whether the chosen codec path uses a hardware-accelerated implementation.
    pub hardware_accelerated: bool,
    /// Ordered route steps selected by the explanation pass.
    pub steps: Vec<String>,
    /// Reasons the request could not be accepted as-is.
    pub rejected: Vec<String>,
}

/// Build a capability inventory from probed devices and the enabled codec registry.
pub fn styx_capability_inventory(devices: &[ProbedDevice]) -> StyxCapabilityInventory {
    styx_capability_inventory_with_codec_config(devices, CodecRegistryConfig::default())
}

/// Build a capability inventory using the codec limits from runtime config.
pub fn styx_capability_inventory_with_config(
    devices: &[ProbedDevice],
    config: &StyxConfig,
) -> StyxCapabilityInventory {
    styx_capability_inventory_with_codec_config(devices, config.codec_registry_config())
}

/// Build a capability inventory from probed devices and an explicitly configured codec registry.
pub fn styx_capability_inventory_with_codec_config(
    devices: &[ProbedDevice],
    codec_config: CodecRegistryConfig,
) -> StyxCapabilityInventory {
    let mut capture_backends = capture_capabilities(devices);
    capture_backends.sort_by_key(|cap| cap.backend);
    let mut codecs = codec_capabilities(codec_config);
    codecs.sort_by(|left, right| {
        codec_kind_rank(left.kind)
            .cmp(&codec_kind_rank(right.kind))
            .then_with(|| left.input.cmp(&right.input))
            .then_with(|| left.output.cmp(&right.output))
            .then_with(|| left.implementation.cmp(&right.implementation))
            .then_with(|| left.name.cmp(&right.name))
    });
    StyxCapabilityInventory {
        capture_backends,
        codecs,
        transforms: transform_capabilities(),
        backing: backing_capabilities(),
    }
}

/// Explain whether a requested capture/codec route is possible with the given inventory.
pub fn explain_styx_path(
    inventory: &StyxCapabilityInventory,
    request: &StyxPathRequest,
) -> StyxPathPlan {
    let input = normalize_fourcc(&request.input);
    let output = request.output.as_ref().map(normalize_fourcc);
    let mut steps = Vec::new();
    let mut rejected = Vec::new();
    let mut zero_copy_possible = true;
    let mut hardware_accelerated = false;

    let capture = inventory
        .capture_backends
        .iter()
        .find(|cap| cap.formats.iter().any(|format| format == &input));
    match capture {
        Some(cap) => {
            steps.push(format!("capture:{:?}:{input}", cap.backend));
            if request.require_exportable && !cap.exportable {
                rejected.push(format!("capture {:?} is not exportable", cap.backend));
            }
            if cap.zero_copy_residencies.is_empty() {
                zero_copy_possible = false;
                rejected.push(format!(
                    "capture {:?} has no zero-copy residency",
                    cap.backend
                ));
            }
        }
        None => rejected.push(format!("no capture backend advertises {input}")),
    }

    if let Some(output) = output {
        if input == output {
            steps.push("codec:passthrough".to_string());
        } else {
            let mut codecs = inventory.codecs.iter().filter(|cap| {
                cap.input == input
                    && cap.output == output
                    && (!request.require_exportable || cap.exportable_output_possible)
            });
            let first_codec = codecs.next();
            let codec = first_codec.and_then(|first| {
                if first.hardware_accelerated {
                    Some(first)
                } else {
                    codecs.find(|cap| cap.hardware_accelerated).or(Some(first))
                }
            });
            match codec {
                Some(cap) => {
                    steps.push(format!(
                        "codec:{:?}:{}:{}->{}",
                        cap.kind, cap.implementation, cap.input, cap.output
                    ));
                    hardware_accelerated |= cap.hardware_accelerated;
                    if !cap.preserves_input_residency {
                        zero_copy_possible = false;
                    }
                }
                None => {
                    zero_copy_possible = false;
                    rejected.push(format!("no codec can produce {output} from {input}"));
                }
            }
        }
    }

    if request.prefer_zero_copy && !zero_copy_possible {
        rejected.push("requested zero-copy preference cannot be fully preserved".to_string());
    }

    StyxPathPlan {
        accepted: rejected.is_empty(),
        zero_copy_possible,
        hardware_accelerated,
        steps,
        rejected,
    }
}

fn capture_capabilities(devices: &[ProbedDevice]) -> Vec<CaptureBackendCapability> {
    let mut out = Vec::new();
    for backend in devices.iter().flat_map(|device| &device.backends) {
        let mut formats: Vec<_> = backend
            .descriptor
            .modes
            .iter()
            .map(|mode| mode.format.code.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        formats.sort();
        out.push(CaptureBackendCapability {
            backend: backend.kind,
            formats,
            zero_copy_residencies: backend_zero_copy_residencies(backend.kind),
            exportable: backend_exportable(backend.kind),
            export_modes: backend_export_modes(backend.kind),
            zero_copy_cross_process: backend_zero_copy_cross_process(backend.kind),
            notes: backend_notes(backend.kind),
        });
    }
    out
}

fn codec_capabilities(config: CodecRegistryConfig) -> Vec<CodecCapability> {
    let Ok(codecs) = CodecRegistry::list_enabled_codecs_with_config(config) else {
        return Vec::new();
    };
    codecs
        .into_iter()
        .flat_map(|(_, descs)| descs)
        .map(codec_capability_from_descriptor)
        .collect()
}

fn codec_capability_from_descriptor(desc: CodecDescriptor) -> CodecCapability {
    let accepted_inputs = codec_accepted_inputs(&desc);
    let possible_outputs = codec_possible_outputs(&desc);
    let preserves_input_residency = desc.impl_name.eq_ignore_ascii_case("passthrough");
    let hardware_accelerated = is_hardware_codec(&desc);
    let exportable_output_possible = possible_outputs.iter().any(|residency| {
        matches!(
            residency,
            FrameResidency::HostExternal | FrameResidency::Dmabuf
        )
    });
    CodecCapability {
        kind: desc.kind,
        name: desc.name.to_string(),
        implementation: desc.impl_name.to_string(),
        input: desc.input.to_string(),
        output: desc.output.to_string(),
        accepted_inputs,
        possible_outputs,
        preserves_input_residency,
        hardware_accelerated,
        exportable_output_possible,
    }
}

fn transform_capabilities() -> Vec<TransformCapability> {
    let caps = packed_transform_residency_capabilities();
    vec![TransformCapability {
        id: "packed_cpu_transform".to_string(),
        accepted_inputs: caps.accepted_inputs.to_vec(),
        possible_outputs: caps.possible_outputs.to_vec(),
        copy_required: true,
    }]
}

fn backing_capabilities() -> Vec<FrameBackingCapability> {
    vec![
        FrameBackingCapability {
            id: "owned".to_string(),
            residency: FrameResidency::HostOwned,
            cross_process_export: false,
            zero_copy_graph_handoff: true,
            export_mode: CrossProcessExportMode::CopyToMemfd,
        },
        FrameBackingCapability {
            id: "external".to_string(),
            residency: FrameResidency::HostExternal,
            cross_process_export: true,
            zero_copy_graph_handoff: true,
            export_mode: CrossProcessExportMode::Memfd,
        },
        FrameBackingCapability {
            id: "dmabuf".to_string(),
            residency: FrameResidency::Dmabuf,
            cross_process_export: true,
            zero_copy_graph_handoff: true,
            export_mode: CrossProcessExportMode::Dmabuf,
        },
        FrameBackingCapability {
            id: "compressed_packet".to_string(),
            residency: FrameResidency::CompressedPacket,
            cross_process_export: true,
            zero_copy_graph_handoff: true,
            export_mode: CrossProcessExportMode::Memfd,
        },
    ]
}

fn backend_zero_copy_residencies(kind: BackendKind) -> Vec<FrameResidency> {
    match kind {
        BackendKind::V4l2 => vec![
            FrameResidency::HostExternal,
            FrameResidency::CompressedPacket,
        ],
        BackendKind::Libcamera => vec![FrameResidency::Dmabuf],
        BackendKind::Virtual => vec![FrameResidency::HostExternal],
        BackendKind::Netcam => vec![FrameResidency::CompressedPacket],
        BackendKind::File => vec![
            FrameResidency::HostExternal,
            FrameResidency::CompressedPacket,
        ],
        BackendKind::Simulation => vec![FrameResidency::HostOwned, FrameResidency::HostExternal],
    }
}

fn backend_exportable(kind: BackendKind) -> bool {
    !backend_export_modes(kind)
        .iter()
        .all(|mode| *mode == CrossProcessExportMode::NotExportable)
}

fn backend_zero_copy_cross_process(kind: BackendKind) -> bool {
    matches!(
        kind,
        BackendKind::V4l2
            | BackendKind::Libcamera
            | BackendKind::Virtual
            | BackendKind::Netcam
            | BackendKind::File
            | BackendKind::Simulation
    )
}

fn backend_export_modes(kind: BackendKind) -> Vec<CrossProcessExportMode> {
    match kind {
        BackendKind::V4l2 => vec![
            CrossProcessExportMode::Dmabuf,
            CrossProcessExportMode::Memfd,
            CrossProcessExportMode::CopyToMemfd,
        ],
        BackendKind::Libcamera => vec![CrossProcessExportMode::Dmabuf],
        BackendKind::Virtual => vec![CrossProcessExportMode::Memfd],
        BackendKind::Netcam | BackendKind::File | BackendKind::Simulation => {
            vec![
                CrossProcessExportMode::Memfd,
                CrossProcessExportMode::CopyToMemfd,
            ]
        }
    }
}

fn backend_notes(kind: BackendKind) -> Vec<String> {
    match kind {
        BackendKind::V4l2 => {
            vec![
                "mmap-backed frames can stay external on supported formats".into(),
                "zero-copy mmap buffers export as dma-buf; copied fallback frames use memfd-backed shared buffers".into(),
            ]
        }
        BackendKind::Libcamera => {
            vec!["native dma-buf planes are exported by duplicating plane fds".into()]
        }
        BackendKind::Virtual => vec!["virtual frames use memfd-backed shared buffers".into()],
        BackendKind::Netcam => vec![
            "compressed packets avoid pixel copies until decode".into(),
            "packet backing uses memfd when shared allocation succeeds; otherwise export_or_copy_memfd provides the boundary copy".into(),
        ],
        BackendKind::File => vec![
            "file replay may decode or reuse compressed packets".into(),
            "shared replay output uses memfd; owned decoded output can cross process by copy-to-memfd".into(),
        ],
        BackendKind::Simulation => vec![
            "readback can be host-owned or memfd-backed external staging".into(),
            "owned readback crosses process through copy-to-memfd fallback".into(),
        ],
    }
}

fn codec_accepted_inputs(desc: &CodecDescriptor) -> Vec<FrameResidency> {
    match desc.kind {
        CodecKind::Decoder if desc.input.is_compressed() => vec![
            FrameResidency::CompressedPacket,
            FrameResidency::HostOwned,
            FrameResidency::HostExternal,
            FrameResidency::Dmabuf,
        ],
        CodecKind::Decoder | CodecKind::Encoder => vec![
            FrameResidency::HostOwned,
            FrameResidency::HostExternal,
            FrameResidency::Dmabuf,
        ],
    }
}

fn codec_possible_outputs(desc: &CodecDescriptor) -> Vec<FrameResidency> {
    match desc.kind {
        CodecKind::Decoder => vec![
            FrameResidency::HostOwned,
            FrameResidency::HostExternal,
            FrameResidency::Dmabuf,
        ],
        CodecKind::Encoder if desc.output.is_compressed() => vec![FrameResidency::CompressedPacket],
        CodecKind::Encoder => vec![FrameResidency::HostOwned],
    }
}

fn is_hardware_codec(desc: &CodecDescriptor) -> bool {
    desc.is_hardware_accelerated()
}

fn normalize_fourcc(value: impl AsRef<str>) -> String {
    value.as_ref().trim().to_ascii_uppercase()
}

fn codec_kind_rank(kind: CodecKind) -> u8 {
    match kind {
        CodecKind::Decoder => 0,
        CodecKind::Encoder => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use styx_capture::prelude::{
        CaptureDescriptor, ColorSpace, Interval, MediaFormat, Mode, ModeId, Resolution,
    };
    use styx_core::prelude::FourCc;

    fn device(format: FourCc) -> ProbedDevice {
        device_with_backend(format, BackendKind::Virtual)
    }

    fn device_with_backend(format: FourCc, backend: BackendKind) -> ProbedDevice {
        let interval = Interval {
            numerator: NonZeroU32::new(1).unwrap(),
            denominator: NonZeroU32::new(30).unwrap(),
        };
        let media = MediaFormat::new(format, Resolution::new(2, 2).unwrap(), ColorSpace::Srgb);
        let mode = Mode {
            id: ModeId {
                format: media,
                interval: Some(interval),
            },
            format: media,
            intervals: smallvec::smallvec![interval],
            interval_stepwise: None,
        };
        ProbedDevice {
            identity: crate::DeviceIdentity {
                display: "cap-test".into(),
                keys: vec!["cap-test".into()],
            },
            backends: vec![crate::ProbedBackend {
                kind: backend,
                handle: crate::BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes: vec![mode],
                    controls: vec![],
                },
                properties: vec![],
            }],
        }
    }

    #[test]
    fn inventory_reports_capture_backing_and_transform_capabilities() {
        let inventory = styx_capability_inventory(&[device(FourCc::RG24)]);

        assert!(inventory.capture_backends.iter().any(|cap| {
            cap.backend == BackendKind::Virtual
                && cap.formats == vec!["RG24"]
                && cap
                    .zero_copy_residencies
                    .contains(&FrameResidency::HostExternal)
        }));
        assert!(inventory.backing.iter().any(|cap| {
            cap.id == "dmabuf"
                && cap.cross_process_export
                && cap.zero_copy_graph_handoff
                && cap.export_mode == CrossProcessExportMode::Dmabuf
        }));
        assert!(
            inventory
                .transforms
                .iter()
                .any(|cap| cap.id == "packed_cpu_transform" && cap.copy_required)
        );
    }

    #[test]
    fn inventory_accepts_codec_limits_from_runtime_config() {
        let config = StyxConfig::new().codec_max_dimensions(3840, 2160);
        let inventory = styx_capability_inventory_with_config(&[device(FourCc::RG24)], &config);

        assert_eq!(inventory.capture_backends.len(), 1);
        assert_eq!(inventory.capture_backends[0].formats, vec!["RG24"]);
    }

    #[test]
    fn capture_capabilities_report_cross_process_export_modes() {
        let devices = vec![
            device_with_backend(FourCc::RG24, BackendKind::V4l2),
            device_with_backend(FourCc::RG24, BackendKind::Libcamera),
            device_with_backend(FourCc::MJPG, BackendKind::Netcam),
        ];
        let inventory = styx_capability_inventory(&devices);
        let v4l2 = inventory
            .capture_backends
            .iter()
            .find(|cap| cap.backend == BackendKind::V4l2)
            .expect("v4l2 cap");
        let libcamera = inventory
            .capture_backends
            .iter()
            .find(|cap| cap.backend == BackendKind::Libcamera)
            .expect("libcamera cap");
        let netcam = inventory
            .capture_backends
            .iter()
            .find(|cap| cap.backend == BackendKind::Netcam)
            .expect("netcam cap");

        assert!(v4l2.export_modes.contains(&CrossProcessExportMode::Dmabuf));
        assert!(v4l2.export_modes.contains(&CrossProcessExportMode::Memfd));
        assert!(v4l2.zero_copy_cross_process);
        assert_eq!(libcamera.export_modes, vec![CrossProcessExportMode::Dmabuf]);
        assert!(libcamera.zero_copy_cross_process);
        assert!(netcam.export_modes.contains(&CrossProcessExportMode::Memfd));
        assert!(
            netcam
                .export_modes
                .contains(&CrossProcessExportMode::CopyToMemfd)
        );
        assert!(netcam.exportable);
    }

    #[test]
    fn path_explain_accepts_capture_passthrough() {
        let inventory = styx_capability_inventory(&[device(FourCc::RG24)]);
        let plan = explain_styx_path(
            &inventory,
            &StyxPathRequest::new("rg24")
                .output("RG24")
                .require_exportable(true),
        );

        assert!(plan.accepted, "{:?}", plan.rejected);
        assert!(plan.zero_copy_possible);
        assert!(plan.steps.iter().any(|step| step == "codec:passthrough"));
    }

    #[test]
    fn path_explain_rejects_missing_capture_format() {
        let inventory = styx_capability_inventory(&[device(FourCc::RG24)]);
        let plan = explain_styx_path(&inventory, &StyxPathRequest::new("MJPG").output("RG24"));

        assert!(!plan.accepted);
        assert!(
            plan.rejected
                .iter()
                .any(|reason| reason.contains("no capture backend advertises MJPG"))
        );
    }

    #[test]
    fn path_explain_reports_accelerated_codec_choice() {
        let mut inventory = styx_capability_inventory(&[device(FourCc::RG24)]);
        inventory.codecs.push(CodecCapability {
            kind: CodecKind::Encoder,
            name: "h264_v4l2m2m".into(),
            implementation: "ffmpeg".into(),
            input: "RG24".into(),
            output: "H264".into(),
            accepted_inputs: vec![FrameResidency::HostExternal],
            possible_outputs: vec![FrameResidency::CompressedPacket],
            preserves_input_residency: false,
            hardware_accelerated: true,
            exportable_output_possible: false,
        });
        let plan = explain_styx_path(&inventory, &StyxPathRequest::new("RG24").output("H264"));

        assert!(plan.hardware_accelerated);
        assert!(
            plan.steps
                .iter()
                .any(|step| step.contains("codec:Encoder:ffmpeg:RG24->H264"))
        );
    }
}
