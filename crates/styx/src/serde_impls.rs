use serde::{Deserialize, Serialize};

#[cfg(any(feature = "file-backend", feature = "simulation-bevy"))]
use std::path::PathBuf;

use crate::BackendHandle;

impl Serialize for BackendHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            #[derive(Serialize)]
            #[serde(tag = "type", rename_all = "snake_case")]
            enum HumanHandle<'a> {
                _Marker(std::marker::PhantomData<&'a ()>),
                #[cfg(feature = "v4l2")]
                V4l2 {
                    path: &'a str,
                },
                #[cfg(feature = "libcamera")]
                Libcamera {
                    id: &'a str,
                },
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: &'a str,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
                #[cfg(feature = "simulation-bevy")]
                Simulation {
                    scene_path: String,
                    config: crate::capture_api::SimulationDeviceConfig,
                },
            }

            let human = match self {
                #[cfg(feature = "v4l2")]
                BackendHandle::V4l2 { path } => HumanHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                BackendHandle::Libcamera { id } => HumanHandle::Libcamera { id },
                BackendHandle::Virtual => HumanHandle::Virtual,
                #[cfg(feature = "netcam")]
                BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => HumanHandle::Netcam {
                    url,
                    width: *width,
                    height: *height,
                    fps: *fps,
                },
                #[cfg(feature = "file-backend")]
                BackendHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => HumanHandle::File {
                    paths: paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    fps: *fps,
                    loop_forever: *loop_forever,
                },
                #[cfg(feature = "simulation-bevy")]
                BackendHandle::Simulation { scene_path, config } => HumanHandle::Simulation {
                    scene_path: scene_path.to_string_lossy().to_string(),
                    config: config.clone(),
                },
            };
            human.serialize(serializer)
        } else {
            #[derive(Serialize)]
            enum BinaryHandle<'a> {
                _Marker(std::marker::PhantomData<&'a ()>),
                #[cfg(feature = "v4l2")]
                V4l2(&'a str),
                #[cfg(feature = "libcamera")]
                Libcamera(&'a str),
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: &'a str,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
                #[cfg(feature = "simulation-bevy")]
                Simulation {
                    scene_path: String,
                    config: crate::capture_api::SimulationDeviceConfig,
                },
            }
            let bin = match self {
                #[cfg(feature = "v4l2")]
                BackendHandle::V4l2 { path } => BinaryHandle::V4l2(path),
                #[cfg(feature = "libcamera")]
                BackendHandle::Libcamera { id } => BinaryHandle::Libcamera(id),
                BackendHandle::Virtual => BinaryHandle::Virtual,
                #[cfg(feature = "netcam")]
                BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BinaryHandle::Netcam {
                    url,
                    width: *width,
                    height: *height,
                    fps: *fps,
                },
                #[cfg(feature = "file-backend")]
                BackendHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BinaryHandle::File {
                    paths: paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    fps: *fps,
                    loop_forever: *loop_forever,
                },
                #[cfg(feature = "simulation-bevy")]
                BackendHandle::Simulation { scene_path, config } => BinaryHandle::Simulation {
                    scene_path: scene_path.to_string_lossy().to_string(),
                    config: config.clone(),
                },
            };
            bin.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for BackendHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            #[derive(Deserialize)]
            #[serde(tag = "type", rename_all = "snake_case")]
            enum HumanHandle {
                #[cfg(feature = "v4l2")]
                V4l2 {
                    path: String,
                },
                #[cfg(feature = "libcamera")]
                Libcamera {
                    id: String,
                },
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: String,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
                #[cfg(feature = "simulation-bevy")]
                Simulation {
                    scene_path: String,
                    config: crate::capture_api::SimulationDeviceConfig,
                },
            }
            let human = HumanHandle::deserialize(deserializer)?;
            let handle = match human {
                #[cfg(feature = "v4l2")]
                HumanHandle::V4l2 { path } => BackendHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                HumanHandle::Libcamera { id } => BackendHandle::Libcamera { id },
                HumanHandle::Virtual => BackendHandle::Virtual,
                #[cfg(feature = "netcam")]
                HumanHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                },
                #[cfg(feature = "file-backend")]
                HumanHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BackendHandle::File {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                    fps,
                    loop_forever,
                },
                #[cfg(feature = "simulation-bevy")]
                HumanHandle::Simulation { scene_path, config } => BackendHandle::Simulation {
                    scene_path: PathBuf::from(scene_path),
                    config,
                },
            };
            Ok(handle)
        } else {
            #[derive(Deserialize)]
            enum BinaryHandle {
                #[cfg(feature = "v4l2")]
                V4l2(String),
                #[cfg(feature = "libcamera")]
                Libcamera(String),
                Virtual,
                #[cfg(feature = "netcam")]
                Netcam {
                    url: String,
                    width: u32,
                    height: u32,
                    fps: u32,
                },
                #[cfg(feature = "file-backend")]
                File {
                    paths: Vec<String>,
                    fps: u32,
                    loop_forever: bool,
                },
                #[cfg(feature = "simulation-bevy")]
                Simulation {
                    scene_path: String,
                    config: crate::capture_api::SimulationDeviceConfig,
                },
            }
            let bin = BinaryHandle::deserialize(deserializer)?;
            let handle = match bin {
                #[cfg(feature = "v4l2")]
                BinaryHandle::V4l2(path) => BackendHandle::V4l2 { path },
                #[cfg(feature = "libcamera")]
                BinaryHandle::Libcamera(id) => BackendHandle::Libcamera { id },
                BinaryHandle::Virtual => BackendHandle::Virtual,
                #[cfg(feature = "netcam")]
                BinaryHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                } => BackendHandle::Netcam {
                    url,
                    width,
                    height,
                    fps,
                },
                #[cfg(feature = "file-backend")]
                BinaryHandle::File {
                    paths,
                    fps,
                    loop_forever,
                } => BackendHandle::File {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                    fps,
                    loop_forever,
                },
                #[cfg(feature = "simulation-bevy")]
                BinaryHandle::Simulation { scene_path, config } => BackendHandle::Simulation {
                    scene_path: PathBuf::from(scene_path),
                    config,
                },
            };
            Ok(handle)
        }
    }
}
