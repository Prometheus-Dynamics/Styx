use std::fmt;
use std::sync::OnceLock;

use crate::buffer::{
    BufferPool, BufferPoolStats, FrameLease, FrameResidency, ResidencyTransition,
    ResidencyTransitionReason, plane_layout_from_dims,
};
use crate::format::{FourCc, MediaFormat, Resolution};
use parking_lot::Mutex;

/// Rotation in 90-degree steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum Rotation90 {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

/// Frame transform applied to packed frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FrameTransform {
    /// Rotation in 90-degree steps.
    pub rotation: Rotation90,
    /// Mirror horizontally (left-right).
    pub mirror: bool,
}

impl FrameTransform {
    pub fn is_identity(&self) -> bool {
        self.rotation == Rotation90::Deg0 && !self.mirror
    }
}

/// Errors produced by packed-frame transforms.
#[derive(Debug, Clone)]
pub enum TransformError {
    UnsupportedFormat(FourCc),
    UnsupportedLayout,
    InvalidResolution,
}

impl fmt::Display for TransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransformError::UnsupportedFormat(code) => {
                write!(f, "unsupported packed format {}", code)
            }
            TransformError::UnsupportedLayout => write!(f, "unsupported frame layout"),
            TransformError::InvalidResolution => write!(f, "invalid frame resolution"),
        }
    }
}

fn packed_bytes_per_pixel(code: FourCc) -> Option<usize> {
    code.packed_bytes_per_pixel()
}

/// Runtime sizing for the process-wide packed-transform pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct TransformPoolConfig {
    pub min: usize,
    pub bytes: usize,
    pub spare: usize,
}

impl Default for TransformPoolConfig {
    fn default() -> Self {
        Self {
            min: 2,
            bytes: 1,
            spare: 4,
        }
    }
}

static TRANSFORM_POOL: OnceLock<Mutex<(BufferPool, TransformPoolConfig)>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct TransformResidencyCapabilities {
    pub accepted_inputs: &'static [FrameResidency],
    pub possible_outputs: &'static [FrameResidency],
    pub preserves_input_residency: bool,
    pub forces_copy: bool,
}

fn transform_pool(min_size: usize) -> BufferPool {
    let default_config = TransformPoolConfig {
        bytes: min_size,
        ..TransformPoolConfig::default()
    };
    let lock = TRANSFORM_POOL
        .get_or_init(|| Mutex::new((BufferPool::with_limits(2, min_size, 4), default_config)));
    let mut guard = lock.lock();
    if guard.1.bytes < min_size {
        guard.1.bytes = min_size;
        guard.0 = BufferPool::with_limits(guard.1.min, guard.1.bytes, guard.1.spare);
    }
    guard.0.clone()
}

/// Configure the process-wide packed-transform pool used by `transform_packed_frame`.
pub fn configure_transform_pool(config: TransformPoolConfig) {
    let config = TransformPoolConfig {
        min: config.min,
        bytes: config.bytes.max(1),
        spare: config.spare,
    };
    let lock = TRANSFORM_POOL.get_or_init(|| {
        Mutex::new((
            BufferPool::with_limits(config.min, config.bytes, config.spare),
            config,
        ))
    });
    let mut guard = lock.lock();
    *guard = (
        BufferPool::with_limits(config.min, config.bytes, config.spare),
        config,
    );
}

pub fn transform_pool_config() -> TransformPoolConfig {
    let lock = TRANSFORM_POOL.get_or_init(|| {
        let config = TransformPoolConfig::default();
        Mutex::new((
            BufferPool::with_limits(config.min, config.bytes, config.spare),
            config,
        ))
    });
    lock.lock().1
}

pub fn transform_pool_stats() -> Option<BufferPoolStats> {
    let lock = TRANSFORM_POOL.get()?;
    let guard = lock.lock();
    Some(guard.0.stats())
}

pub fn reset_transform_pool() {
    if let Some(lock) = TRANSFORM_POOL.get() {
        let mut guard = lock.lock();
        let config = TransformPoolConfig {
            min: 0,
            bytes: 1,
            spare: 0,
        };
        *guard = (BufferPool::with_limits(0, 1, 0), config);
    }
}

pub fn packed_transform_residency_capabilities() -> TransformResidencyCapabilities {
    TransformResidencyCapabilities {
        accepted_inputs: &[
            FrameResidency::HostOwned,
            FrameResidency::HostExternal,
            FrameResidency::Dmabuf,
        ],
        possible_outputs: &[FrameResidency::HostOwned],
        preserves_input_residency: false,
        forces_copy: true,
    }
}

/// Rotate/mirror a tightly-packed single-plane frame.
pub fn transform_packed_frame(
    frame: &FrameLease,
    transform: FrameTransform,
) -> Result<FrameLease, TransformError> {
    let meta = frame.meta();
    let format = meta.format;
    let bpp = packed_bytes_per_pixel(format.code)
        .ok_or(TransformError::UnsupportedFormat(format.code))?;
    let res = format.resolution;
    let width = res.width.get() as usize;
    let height = res.height.get() as usize;
    if width == 0 || height == 0 {
        return Err(TransformError::InvalidResolution);
    }
    let planes = frame.planes();
    if planes.len() != 1 {
        return Err(TransformError::UnsupportedLayout);
    }
    let plane = planes[0];
    let src_stride = plane.stride();
    let src = plane.data();
    let min_len = src_stride
        .checked_mul(height)
        .ok_or(TransformError::InvalidResolution)?;
    if src.len() < min_len {
        return Err(TransformError::UnsupportedLayout);
    }
    let (out_width, out_height) = match transform.rotation {
        Rotation90::Deg90 | Rotation90::Deg270 => (height, width),
        Rotation90::Deg0 | Rotation90::Deg180 => (width, height),
    };
    let out_res = Resolution::new(out_width as u32, out_height as u32)
        .ok_or(TransformError::InvalidResolution)?;
    let out_stride = out_width
        .checked_mul(bpp)
        .ok_or(TransformError::InvalidResolution)?;
    let layout = plane_layout_from_dims(out_res.width, out_res.height, bpp);
    let pool = transform_pool(layout.len);
    let mut buf = pool.lease();
    unsafe {
        buf.resize_uninit(layout.len);
    }
    let dst = buf.as_mut_slice();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mirror = transform.mirror;
    let w_out = out_width;
    let h_out = out_height;
    for y_out in 0..h_out {
        for x_out in 0..w_out {
            let x_rot = if mirror { w_out - 1 - x_out } else { x_out };
            let (x_in, y_in) = match transform.rotation {
                Rotation90::Deg0 => (x_rot, y_out),
                Rotation90::Deg90 => (y_out, height - 1 - x_rot),
                Rotation90::Deg180 => (width - 1 - x_rot, height - 1 - y_out),
                Rotation90::Deg270 => (width - 1 - y_out, x_rot),
            };
            let src_off = y_in * src_stride + x_in * bpp;
            let dst_off = y_out * out_stride + x_out * bpp;
            unsafe {
                std::ptr::copy_nonoverlapping(src_ptr.add(src_off), dst_ptr.add(dst_off), bpp);
            }
        }
    }
    let out_format = MediaFormat::new(format.code, out_res, format.color);
    let mut out_meta = meta.clone();
    out_meta.format = out_format;
    out_meta.residency = Some(FrameResidency::HostOwned);
    out_meta.last_transition = Some(ResidencyTransition {
        from: frame.residency(),
        to: FrameResidency::HostOwned,
        reason: ResidencyTransitionReason::PackedTransform,
        copied: true,
    });
    Ok(FrameLease::single_plane(
        out_meta,
        buf,
        layout.len,
        layout.stride,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufferPool, FrameMeta};
    use crate::format::ColorSpace;

    fn make_frame_rg24(width: u32, height: u32, pixels: &[u8]) -> FrameLease {
        let res = Resolution::new(width, height).unwrap();
        let fmt = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
        let layout = plane_layout_from_dims(res.width, res.height, 3);
        let pool = BufferPool::with_capacity(1, layout.len);
        let mut buf = pool.lease();
        buf.resize(layout.len);
        buf.as_mut_slice().copy_from_slice(pixels);
        FrameLease::single_plane(FrameMeta::new(fmt, 0), buf, layout.len, layout.stride)
    }

    #[test]
    fn rotate90_rg24() {
        // 2x1 image: pixels A B (RGB triplets)
        let frame = make_frame_rg24(2, 1, &[1, 0, 0, 2, 0, 0]);
        let out = transform_packed_frame(
            &frame,
            FrameTransform {
                rotation: Rotation90::Deg90,
                mirror: false,
            },
        )
        .expect("transform");
        let out_plane = out.planes()[0].data().to_vec();
        // After 90deg clockwise: 1x2 with A on top, B on bottom.
        assert_eq!(out.meta().format.resolution.width.get(), 1);
        assert_eq!(out.meta().format.resolution.height.get(), 2);
        assert_eq!(out_plane, vec![1, 0, 0, 2, 0, 0]);
    }

    #[test]
    fn mirror_rg24() {
        let frame = make_frame_rg24(2, 1, &[1, 0, 0, 2, 0, 0]);
        let out = transform_packed_frame(
            &frame,
            FrameTransform {
                rotation: Rotation90::Deg0,
                mirror: true,
            },
        )
        .expect("transform");
        let out_plane = out.planes()[0].data().to_vec();
        assert_eq!(out_plane, vec![2, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn transform_pool_config_is_runtime_configurable() {
        configure_transform_pool(TransformPoolConfig {
            min: 3,
            bytes: 128,
            spare: 5,
        });

        assert_eq!(
            transform_pool_config(),
            TransformPoolConfig {
                min: 3,
                bytes: 128,
                spare: 5,
            }
        );
    }

    #[test]
    fn repeated_hd_transforms_reuse_pool() {
        reset_transform_pool();
        configure_transform_pool(TransformPoolConfig {
            min: 1,
            bytes: 640 * 360 * 3,
            spare: 2,
        });
        for (width, height) in [(1280, 720), (1920, 1080)] {
            let pixels = vec![0x7f; width * height * 3];
            let frame = make_frame_rg24(width as u32, height as u32, &pixels);

            for transform in [
                FrameTransform {
                    rotation: Rotation90::Deg90,
                    mirror: false,
                },
                FrameTransform {
                    rotation: Rotation90::Deg180,
                    mirror: true,
                },
                FrameTransform {
                    rotation: Rotation90::Deg270,
                    mirror: false,
                },
            ] {
                let out = transform_packed_frame(&frame, transform).expect("transform");
                assert_eq!(out.planes()[0].data().len(), width * height * 3);
            }
        }

        let stats = transform_pool_stats().expect("pool stats");
        assert!(stats.chunk_size >= 1920 * 1080 * 3);
    }
}
