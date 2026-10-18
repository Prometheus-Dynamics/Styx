//! Styx frames through a Daedalus GPU shader node: each camera frame is converted to RGBA8,
//! a WGSL compute shader (`contrast_boost.wgsl`) boosts its contrast on the GPU (wgpu), and the
//! result is shown in a preview window. The other Daedalus examples (`daedalus_frames`,
//! `daedalus_multicam`) stay on the CPU.
//!
//! ```sh
//! cargo run -p styx-examples --features daedalus-gpu,preview-window --bin daedalus_gpu_node
//! ```

mod real_gpu {
    use std::{borrow::Cow, time::Duration};

    use daedalus::{
        engine::{Engine, EngineConfig, GpuBackend, MetricsLevel},
        gpu::shader::{Access, BindingData, BindingKind, ShaderBinding, ShaderContext},
        macros::{node, plugin},
        runtime::{NodeError, plugins::PluginRegistry},
    };
    use styx::extras::preview_window::PreviewWindow;
    use styx::prelude::*;

    #[node(
        id = "gpu.frame_lease.contrast_boost",
        inputs("frame"),
        outputs("frame"),
        shader = "contrast_boost.wgsl"
    )]
    fn contrast_boost(frame: FrameLease, ctx: ShaderContext) -> Result<FrameLease, NodeError> {
        let mut rgba = frame
            .to_rgba8()
            .ok_or_else(|| NodeError::Handler("frame could not be converted to RGBA8".into()))?;
        let res = rgba.meta().format.resolution;
        let width = res.width.get();
        let height = res.height.get();
        let packed = packed_rgba(&rgba, width, height)?;
        let bindings = [
            ShaderBinding {
                binding: 0,
                kind: BindingKind::Texture2D,
                access: Access::ReadOnly,
                data: BindingData::TextureRgba8 {
                    width,
                    height,
                    bytes: Cow::Borrowed(&packed),
                },
                readback: false,
            },
            ShaderBinding {
                binding: 1,
                kind: BindingKind::StorageTexture2D,
                access: Access::WriteOnly,
                data: BindingData::TextureAlloc { width, height },
                readback: true,
            },
        ];
        let output = ctx
            .dispatch_first(&bindings, None, None, Some([width, height, 1]))
            .map_err(|err| NodeError::Handler(format!("shader dispatch failed: {err}")))?;
        let bytes = output
            .buffers
            .get(&1)
            .ok_or_else(|| NodeError::Handler("shader did not return rgba8 output".into()))?;
        write_rgba(&mut rgba, width, height, bytes)?;
        Ok(rgba)
    }

    // Styx's `daedalus` feature registers `FrameLease` under `styx:framelease`.
    #[plugin(id = "example.gpu_frame_lease", nodes(contrast_boost))]
    struct GpuFrameLeasePlugin;

    fn packed_rgba(frame: &FrameLease, width: u32, height: u32) -> Result<Vec<u8>, NodeError> {
        let planes = frame.planes();
        let plane = planes
            .first()
            .ok_or_else(|| NodeError::Handler("frame had no image plane".into()))?;
        let row_bytes = width as usize * 4;
        let rows = height as usize;
        let stride = plane.stride();
        let data = plane.data();
        if stride < row_bytes
            || data.len() < stride.saturating_mul(rows.saturating_sub(1)) + row_bytes
        {
            return Err(NodeError::Handler(
                "frame plane is smaller than RGBA dimensions".into(),
            ));
        }
        if stride == row_bytes {
            return Ok(data[..row_bytes * rows].to_vec());
        }
        let mut packed = Vec::with_capacity(row_bytes * rows);
        for row in 0..rows {
            let start = row * stride;
            packed.extend_from_slice(&data[start..start + row_bytes]);
        }
        Ok(packed)
    }

    fn write_rgba(
        frame: &mut FrameLease,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<(), NodeError> {
        let row_bytes = width as usize * 4;
        let rows = height as usize;
        if bytes.len() < row_bytes * rows {
            return Err(NodeError::Handler(
                "shader output was smaller than RGBA dimensions".into(),
            ));
        }
        let mut planes = frame.planes_mut();
        let plane = planes
            .first_mut()
            .ok_or_else(|| NodeError::Handler("frame had no writable image plane".into()))?;
        let stride = plane.stride();
        let data = plane.data();
        if stride < row_bytes
            || data.len() < stride.saturating_mul(rows.saturating_sub(1)) + row_bytes
        {
            return Err(NodeError::Handler(
                "frame plane is smaller than RGBA dimensions".into(),
            ));
        }
        for row in 0..rows {
            let dst = row * stride;
            let src = row * row_bytes;
            data[dst..dst + row_bytes].copy_from_slice(&bytes[src..src + row_bytes]);
        }
        Ok(())
    }

    fn build_runtime() -> Result<
        daedalus::engine::HostGraph<daedalus::runtime::handler_registry::HandlerRegistry>,
        Box<dyn std::error::Error>,
    > {
        let mut registry = PluginRegistry::new();
        let plugin = GpuFrameLeasePlugin::new();
        registry.install(&plugin)?;

        let node = plugin.contrast_boost.alias("contrast");
        let graph = registry
            .graph_builder()?
            .inputs(|g| {
                g.input("frame");
            })
            .outputs(|g| {
                g.output("preview");
            })
            .nodes(|g| {
                g.add_handle_with_compute(&node, daedalus::ComputeAffinity::GpuPreferred);
            })
            .try_edges(|g| {
                let contrast = g.node("contrast");
                g.try_connect("frame", &contrast.input("frame"))?;
                g.try_connect(&contrast.output("frame"), "preview")?;
                Ok(())
            })?
            .build();

        let engine = Engine::new(
            EngineConfig::from(GpuBackend::Device).with_metrics_level(MetricsLevel::Detailed),
        )?;
        Ok(engine.compile_registry(&registry, graph)?)
    }

    fn selected_mode<'a>(device: &'a ProbedDevice, selected: &SelectedCamera) -> Option<&'a Mode> {
        device
            .backends
            .iter()
            .find(|backend| backend.kind == selected.backend)
            .and_then(|backend| {
                backend
                    .descriptor
                    .modes
                    .iter()
                    .find(|mode| mode.id == selected.mode)
            })
    }

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        // Build the graph first: plugin and graph errors show up before a camera is opened.
        let mut runtime = build_runtime()?;
        let selected = CameraRequest::new()
            .backend_priority([BackendKind::V4l2, BackendKind::Libcamera])
            .format_priority([
                FourCc::new(*b"YUYV"),
                FourCc::new(*b"RG24"),
                FourCc::new(*b"RGB4"),
            ])
            .max_resolution(1280, 720)
            .fastest_interval()
            .select()?;
        println!(
            "camera: {} backend={:?} mode={:?}",
            selected.device.identity.display, selected.backend, selected.mode
        );

        let mode = selected_mode(&selected.device, &selected)
            .cloned()
            .ok_or("selected camera mode was not found in descriptor")?;
        let handle = selected.start()?;
        let mut preview = PreviewWindow::for_mode("daedalus gpu frame lease", &mode).ok();

        let mut frames = 0usize;
        while frames < 240 {
            match handle.recv_blocking(Duration::from_millis(16)) {
                RecvOutcome::Data(frame) => {
                    runtime.push("frame", frame);
                    let telemetry = runtime.tick()?;
                    for processed in runtime.drain_owned::<FrameLease>("preview")? {
                        frames += 1;
                        if let Some(window) = preview.as_mut()
                            && !window.show_if_open(&processed)
                        {
                            handle.stop();
                            return Ok(());
                        }
                        if frames.is_multiple_of(30) {
                            println!("#{frames}");
                            println!("{}", telemetry.compact_snapshot());
                        }
                    }
                }
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => break,
            }
        }

        handle.stop();
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    real_gpu::main()
}
