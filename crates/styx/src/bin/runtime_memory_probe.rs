use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use styx::codec::prelude::{CodecKind, CodecRegistry};
use styx::memory::runtime_memory_report;
use styx::prelude::{
    CameraRequest, MediaPipelineBuilder, RecvOutcome, StyxConfig, StyxServiceConfig,
    StyxServiceRuntime,
};

fn main() -> ExitCode {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "idle".to_string());
    match mode.as_str() {
        "idle" | "snapshot" => {
            print_report(&mode);
            ExitCode::SUCCESS
        }
        "probe" => {
            let devices = styx::probe_all_with_errors();
            println!("mode: {mode}");
            println!(
                "probe: {} devices, {} errors",
                devices.devices.len(),
                devices.errors.len()
            );
            println!("{}", runtime_memory_report());
            ExitCode::SUCCESS
        }
        "codec-registry" => {
            let decoders = CodecRegistry::list_enabled_decoders()
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|(_, descs)| descs.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            let encoders = CodecRegistry::list_enabled_encoders()
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|(_, descs)| descs.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            println!("mode: {mode}");
            println!("codec-registry: {decoders} decoders, {encoders} encoders");
            println!("{}", runtime_memory_report());
            ExitCode::SUCCESS
        }
        "capture-open" => run_capture_open(),
        "capture-read-loop" => run_capture_read_loop(),
        "capture-publish-writer" => run_capture_publish_writer(),
        "full-service-idle" => run_full_service_idle(),
        "full-service-snapshot" => run_full_service_snapshot(),
        _ => {
            eprintln!(
                "usage: runtime_memory_probe [idle|snapshot|probe|codec-registry|capture-open|capture-read-loop|capture-publish-writer|full-service-idle|full-service-snapshot]"
            );
            ExitCode::from(2)
        }
    }
}

fn print_report(mode: &str) {
    let report = runtime_memory_report();
    println!("mode: {mode}");
    println!("{report}");
}

fn run_capture_open() -> ExitCode {
    match open_camera_for_probe() {
        Ok(handle) => {
            println!("mode: capture-open");
            println!(
                "capture: backend={} mode={:?}",
                handle.backend(),
                handle.mode().id
            );
            println!("{}", handle.runtime_memory_report());
            handle.stop();
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("capture-open failed: {err}");
            println!("{}", runtime_memory_report());
            ExitCode::from(1)
        }
    }
}

fn run_capture_read_loop() -> ExitCode {
    let frames_target = std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30);
    match open_camera_for_probe() {
        Ok(handle) => {
            let mut frames = 0u64;
            let mut empty = 0u64;
            while frames < frames_target {
                match handle.recv_blocking(Duration::from_millis(50)) {
                    RecvOutcome::Data(frame) => {
                        frames += 1;
                        std::hint::black_box(frame.payload_bytes());
                    }
                    RecvOutcome::Empty => {
                        empty += 1;
                        if empty > frames_target.saturating_mul(20).max(20) {
                            break;
                        }
                    }
                    RecvOutcome::Closed => break,
                }
            }
            println!("mode: capture-read-loop");
            println!("capture-read-loop: frames={frames} empty_polls={empty}");
            println!("{}", handle.runtime_memory_report());
            handle.stop();
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("capture-read-loop failed: {err}");
            println!("{}", runtime_memory_report());
            ExitCode::from(1)
        }
    }
}

fn run_capture_publish_writer() -> ExitCode {
    let frames_target = frames_arg_or(30);
    let registry = match CodecRegistry::with_enabled_codecs() {
        Ok(registry) => Some(registry.handle()),
        Err(err) => {
            eprintln!("codec registry unavailable; falling back to raw writer: {err}");
            None
        }
    };
    match open_camera_for_probe() {
        Ok(handle) => {
            let mut frames = 0u64;
            let mut encoded_frames = 0u64;
            let mut raw_frames = 0u64;
            let mut encode_errors = 0u64;
            let mut empty = 0u64;
            let mut writer = CountingSink::default();
            while frames < frames_target {
                match handle.recv_blocking(Duration::from_millis(50)) {
                    RecvOutcome::Data(frame) => {
                        frames += 1;
                        let frame = if let Some(registry) = &registry {
                            let code = frame.meta().format.code;
                            let encoder = registry.lookup_auto_kind_by_name(
                                code,
                                CodecKind::Encoder,
                                "mjpeg",
                            );
                            let Ok(encoder) = encoder else {
                                raw_frames += 1;
                                write_frame_to_sink(&mut writer, &frame);
                                continue;
                            };
                            match encoder.process(frame) {
                                Ok(encoded) => {
                                    encoded_frames += 1;
                                    encoded
                                }
                                Err(err) => {
                                    eprintln!("encode failed; dropping writer frame: {err}");
                                    encode_errors += 1;
                                    continue;
                                }
                            }
                        } else {
                            raw_frames += 1;
                            frame
                        };
                        write_frame_to_sink(&mut writer, &frame);
                    }
                    RecvOutcome::Empty => {
                        empty += 1;
                        if empty > frames_target.saturating_mul(20).max(20) {
                            break;
                        }
                    }
                    RecvOutcome::Closed => break,
                }
            }
            println!("mode: capture-publish-writer");
            println!(
                "capture-publish-writer: frames={frames} encoded_frames={encoded_frames} raw_frames={raw_frames} encode_errors={encode_errors} empty_polls={empty} bytes_written={}",
                writer.bytes
            );
            println!("{}", handle.runtime_memory_report());
            handle.stop();
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("capture-publish-writer failed: {err}");
            println!("{}", runtime_memory_report());
            ExitCode::from(1)
        }
    }
}

fn run_full_service_snapshot() -> ExitCode {
    let frames_target = frames_arg_or(30);
    let service = Arc::new(Mutex::new(StyxServiceRuntime::with_config(
        StyxServiceConfig {
            max_retained_events: 128,
        },
    )));
    let refresh = service
        .lock()
        .ok()
        .map(|mut service| service.refresh_devices());
    let request = CameraRequest::new().config(probe_config());
    let selected = match request.select() {
        Ok(selected) => selected,
        Err(err) => {
            eprintln!("full-service-snapshot camera selection failed: {err}");
            println!("{}", runtime_memory_report());
            return ExitCode::from(1);
        }
    };
    let mut pipeline = match MediaPipelineBuilder::new(selected.capture_request())
        .service_runtime(service)
        .raw_frames()
        .start()
    {
        Ok(pipeline) => pipeline,
        Err(err) => {
            eprintln!("full-service-snapshot pipeline start failed: {err}");
            println!("{}", runtime_memory_report());
            return ExitCode::from(1);
        }
    };
    let mut frames = 0u64;
    let mut empty = 0u64;
    while frames < frames_target {
        match pipeline.next_blocking(Duration::from_millis(50)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                std::hint::black_box(frame.payload_bytes());
            }
            RecvOutcome::Empty => {
                empty += 1;
                if empty > frames_target.saturating_mul(20).max(20) {
                    break;
                }
            }
            RecvOutcome::Closed => break,
        }
    }
    println!("mode: full-service-snapshot");
    if let Some(refresh) = refresh {
        println!(
            "service-refresh: added={} removed={} changed={}",
            refresh.diff.added.len(),
            refresh.diff.removed.len(),
            refresh.diff.changed.len()
        );
    }
    println!("full-service-snapshot: frames={frames} empty_polls={empty}");
    println!("{}", pipeline.runtime_memory_report());
    pipeline.stop();
    ExitCode::SUCCESS
}

fn run_full_service_idle() -> ExitCode {
    let service = Arc::new(Mutex::new(StyxServiceRuntime::with_config(
        StyxServiceConfig {
            max_retained_events: 128,
        },
    )));
    let refresh = service
        .lock()
        .ok()
        .map(|mut service| service.refresh_devices());
    println!("mode: full-service-idle");
    if let Some(refresh) = refresh {
        println!(
            "service-refresh: added={} removed={} changed={}",
            refresh.diff.added.len(),
            refresh.diff.removed.len(),
            refresh.diff.changed.len()
        );
    }
    println!("{}", runtime_memory_report());
    ExitCode::SUCCESS
}

fn open_camera_for_probe() -> Result<styx::prelude::CaptureHandle, styx::prelude::CaptureError> {
    CameraRequest::new().config(probe_config()).start()
}

fn probe_config() -> StyxConfig {
    StyxConfig::new()
        .capture_queue_depth(2)
        .libcamera_prefault_request_pools(false)
        .libcamera_stop_when_idle(true)
}

fn frames_arg_or(default: u64) -> u64 {
    std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

#[derive(Default)]
struct CountingSink {
    bytes: u64,
}

fn write_frame_to_sink(writer: &mut CountingSink, frame: &styx::prelude::FrameLease) {
    let planes = frame.planes();
    let bytes = planes.iter().map(|plane| plane.data().len()).sum::<usize>() as u64;
    writer.bytes = writer
        .bytes
        .saturating_add(std::mem::size_of::<u64>() as u64)
        .saturating_add(bytes);
    std::hint::black_box(bytes);
}
