use std::collections::{HashMap, HashSet};

use crate::{BackendHandle, BackendKind, ProbeResult, ProbedBackend, ProbedDevice};

use super::super::{ChangedDevice, InventoryDiff};

pub(crate) fn diff_devices(previous: &[ProbedDevice], next: &[ProbedDevice]) -> InventoryDiff {
    let mut next_by_key: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, device) in next.iter().enumerate() {
        for key in &device.identity.keys {
            next_by_key.entry(key.as_str()).or_default().push(index);
        }
    }

    let mut matched_next = vec![false; next.len()];
    let mut diff = InventoryDiff::default();

    for old_device in previous {
        let mut matched_index = None;
        for key in &old_device.identity.keys {
            if let Some(indices) = next_by_key.get(key.as_str()) {
                for &index in indices {
                    if !matched_next[index] {
                        matched_index = Some(index);
                        break;
                    }
                }
            }
            if matched_index.is_some() {
                break;
            }
        }

        match matched_index {
            Some(index) => {
                matched_next[index] = true;
                if device_signature(old_device) != device_signature(&next[index]) {
                    diff.changed.push(ChangedDevice {
                        before: old_device.clone(),
                        after: next[index].clone(),
                    });
                }
            }
            None => diff.removed.push(old_device.clone()),
        }
    }

    for (index, device) in next.iter().enumerate() {
        if !matched_next[index] {
            diff.added.push(device.clone());
        }
    }

    diff
}

pub(crate) fn normalize_probe_result(mut probe_result: ProbeResult) -> ProbeResult {
    for device in &mut probe_result.devices {
        normalize_probed_device(device);
    }
    probe_result
}

fn device_signature(device: &ProbedDevice) -> String {
    let mut keys = device.identity.keys.clone();
    keys.sort();

    let mut backend_signatures = device
        .backends
        .iter()
        .map(backend_signature)
        .collect::<Vec<_>>();
    backend_signatures.sort();

    let mut out = String::new();
    out.push_str(&device.identity.display);
    out.push('|');
    out.push_str(&keys.join(","));
    for backend in &backend_signatures {
        out.push('|');
        out.push_str(backend);
    }
    out
}

fn normalize_probed_device(device: &mut ProbedDevice) {
    device.identity.keys.sort();
    device.identity.keys.dedup();

    for backend in &mut device.backends {
        normalize_backend(backend);
    }

    device
        .backends
        .sort_by_key(|backend| (backend.kind, handle_signature(&backend.handle)));
}

fn normalize_backend(backend: &mut ProbedBackend) {
    backend.properties.sort();
    backend.properties.dedup();

    backend.descriptor.modes.sort_by_key(|mode| {
        (
            mode.format.code.to_u32(),
            mode.format.resolution.width.get(),
            mode.format.resolution.height.get(),
            format!("{:?}", mode.id.interval),
            format!("{:?}", mode.intervals),
            format!("{:?}", mode.interval_stepwise),
        )
    });
    backend
        .descriptor
        .controls
        .sort_by_key(|control| (control.id.0, control.name.clone()));
}

fn backend_signature(backend: &ProbedBackend) -> String {
    let mut properties = backend.properties.clone();
    properties.sort();

    let mut out = String::new();
    out.push_str(match backend.kind {
        BackendKind::V4l2 => "v4l2",
        BackendKind::Libcamera => "libcamera",
        BackendKind::Virtual => "virtual",
        BackendKind::Netcam => "netcam",
        BackendKind::File => "file",
        BackendKind::Simulation => "simulation",
    });
    out.push(':');
    out.push_str(&handle_signature(&backend.handle));
    out.push(':');
    out.push_str(&format!("{:?}", backend.descriptor));
    out.push(':');
    out.push_str(&format!("{properties:?}"));
    out
}

fn handle_signature(handle: &BackendHandle) -> String {
    match handle {
        #[cfg(feature = "v4l2")]
        BackendHandle::V4l2 { path } => path.clone(),
        #[cfg(feature = "libcamera")]
        BackendHandle::Libcamera { id } => id.clone(),
        BackendHandle::Virtual => "virtual".to_string(),
        #[cfg(feature = "netcam")]
        BackendHandle::Netcam {
            url,
            width,
            height,
            fps,
        } => format!("{url}:{width}:{height}:{fps}"),
        #[cfg(feature = "file-backend")]
        BackendHandle::File {
            paths,
            fps,
            loop_forever,
        } => format!("{paths:?}:{fps}:{loop_forever}"),
        #[cfg(feature = "simulation-bevy")]
        BackendHandle::Simulation { scene_path, config } => {
            format!("{}:{config:?}", scene_path.display())
        }
    }
}

pub(crate) fn merge_probe_result(
    current: &ProbeResult,
    partial: ProbeResult,
    scoped_backends: &[BackendKind],
) -> ProbeResult {
    let scoped = scoped_backends.iter().copied().collect::<HashSet<_>>();
    let failed_backends = partial
        .errors
        .iter()
        .map(|error| error.backend)
        .collect::<HashSet<_>>();
    let successful_scoped = scoped
        .iter()
        .copied()
        .filter(|backend| !failed_backends.contains(backend))
        .collect::<HashSet<_>>();
    let mut devices = Vec::new();

    for device in &current.devices {
        let retained_backends = device
            .backends
            .iter()
            .filter(|backend| !successful_scoped.contains(&backend.kind))
            .cloned()
            .collect::<Vec<_>>();
        if !retained_backends.is_empty() {
            devices.push(ProbedDevice {
                identity: device.identity.clone(),
                backends: retained_backends,
            });
        }
    }

    for device in partial.devices {
        let device_identity = device.identity;
        let backends = device.backends;
        for backend in backends {
            if failed_backends.contains(&backend.kind) {
                continue;
            }
            merge_backend_into_devices(&mut devices, &device_identity, backend);
        }
    }

    let mut errors = current
        .errors
        .iter()
        .filter(|error| {
            !successful_scoped
                .iter()
                .any(|backend| error.backend == *backend)
        })
        .cloned()
        .collect::<Vec<_>>();
    errors.extend(partial.errors);

    ProbeResult { devices, errors }
}

fn merge_backend_into_devices(
    devices: &mut Vec<ProbedDevice>,
    identity: &crate::DeviceIdentity,
    backend: ProbedBackend,
) {
    let mut new_keys = identity.keys.iter().cloned().collect::<HashSet<_>>();
    let backend_id = backend_identity(&identity.display, &backend);
    new_keys.extend(crate::device_identity::derive_keys(
        &backend_id,
        &backend.properties,
    ));
    let new_keys_vec = new_keys.iter().cloned().collect::<Vec<_>>();
    if let Some(existing) = devices.iter_mut().find(|device| {
        device
            .identity
            .keys
            .iter()
            .any(|key| new_keys.contains(key))
    }) {
        existing
            .backends
            .retain(|existing_backend| existing_backend.kind != backend.kind);
        existing.backends.push(backend);
        for key in new_keys {
            if !existing
                .identity
                .keys
                .iter()
                .any(|existing_key| existing_key == &key)
            {
                existing.identity.keys.push(key);
            }
        }
    } else {
        devices.push(ProbedDevice {
            identity: crate::DeviceIdentity {
                display: identity.display.clone(),
                keys: new_keys_vec,
            },
            backends: vec![backend],
        });
    }
}

fn backend_identity(device_display: &str, backend: &ProbedBackend) -> String {
    match &backend.handle {
        #[cfg(feature = "v4l2")]
        BackendHandle::V4l2 { path } => path.clone(),
        #[cfg(feature = "libcamera")]
        BackendHandle::Libcamera { id } => id.clone(),
        _ => device_display.to_string(),
    }
}
