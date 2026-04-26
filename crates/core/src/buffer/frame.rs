use smallvec::{SmallVec, smallvec};
use std::{num::NonZeroU32, sync::Arc};

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
use super::pool::{BufferLease, BufferPool};

/// External backing for frames when zero-copy sharing external memory.
pub trait ExternalBacking: Send + Sync {
    fn plane_data(&self, index: usize) -> Option<&[u8]>;

    fn backing_bytes(&self) -> Option<usize> {
        None
    }

    fn backing_kind(&self) -> &'static str {
        "external"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::HostExternal
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

#[derive(Debug)]
pub struct PlaneMut<'a> {
    data: &'a mut [u8],
    stride: usize,
}

impl<'a> Plane<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> PlaneMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PlaneLayout {
    pub offset: usize,
    pub len: usize,
    pub stride: usize,
}

pub struct FrameLease {
    meta: FrameMeta,
    buffers: SmallVec<[BufferLease; 3]>,
    layouts: SmallVec<[PlaneLayout; 3]>,
    external: Option<Arc<dyn ExternalBacking>>,
}

impl FrameLease {
    pub fn single_plane(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        buffer.resize(len);
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    /// # Safety
    /// The caller must write every byte of the buffer before the frame is read.
    pub unsafe fn single_plane_uninit(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        unsafe { buffer.resize_uninit(len) };
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    pub fn multi_plane(
        mut meta: FrameMeta,
        buffers: SmallVec<[BufferLease; 3]>,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Self {
        debug_assert_eq!(buffers.len(), layouts.len());
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            buffers,
            layouts,
            external: None,
        }
    }

    pub fn from_external(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        backing: Arc<dyn ExternalBacking>,
    ) -> Self {
        if meta.residency.is_none() {
            meta.residency = Some(backing.residency());
        }
        meta.mutability = FrameMutability::ReadOnly;
        Self {
            meta,
            buffers: SmallVec::new(),
            layouts,
            external: Some(backing),
        }
    }

    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut FrameMeta {
        &mut self.meta
    }

    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    pub fn residency(&self) -> FrameResidency {
        self.meta
            .residency
            .unwrap_or_else(|| match self.external.as_ref() {
                Some(backing) => backing.residency(),
                None => default_owned_residency(self.meta.format.code),
            })
    }

    pub fn mutability(&self) -> FrameMutability {
        self.meta.mutability
    }

    pub fn payload_bytes(&self) -> usize {
        self.layouts.iter().map(|layout| layout.len).sum()
    }

    pub fn external_backing_bytes(&self) -> Option<usize> {
        self.external.as_ref().map(|backing| {
            backing
                .backing_bytes()
                .unwrap_or_else(|| self.payload_bytes())
        })
    }

    pub fn external_backing_kind(&self) -> Option<&'static str> {
        self.external.as_ref().map(|backing| backing.backing_kind())
    }

    pub fn planes(&self) -> SmallVec<[Plane<'_>; 3]> {
        if let Some(backing) = &self.external {
            self.layouts
                .iter()
                .enumerate()
                .map(|(idx, layout)| {
                    let slice = backing
                        .plane_data(idx)
                        .map(|s| {
                            let end = layout.offset.saturating_add(layout.len);
                            s.get(layout.offset..end).unwrap_or(&[])
                        })
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        } else {
            self.layouts
                .iter()
                .zip(self.buffers.iter())
                .map(|(layout, buf)| {
                    let slice = buf
                        .as_slice()
                        .get(layout.offset..layout.offset + layout.len)
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        }
    }

    pub fn planes_mut(&mut self) -> SmallVec<[PlaneMut<'_>; 3]> {
        if self.external.is_some() {
            return self
                .layouts
                .iter()
                .map(|layout| PlaneMut {
                    data: &mut [],
                    stride: layout.stride,
                })
                .collect();
        }
        self.layouts
            .iter()
            .zip(self.buffers.iter_mut())
            .map(|(layout, buf)| {
                let len = layout.offset + layout.len;
                if buf.len() < len {
                    buf.resize(len);
                }
                let slice = buf
                    .as_mut_slice()
                    .get_mut(layout.offset..layout.offset + layout.len)
                    .unwrap_or(&mut []);
                PlaneMut {
                    data: slice,
                    stride: layout.stride,
                }
            })
            .collect()
    }

    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.layouts.clone()
    }

    pub fn plane_strides(&self) -> SmallVec<[usize; 3]> {
        self.layouts.iter().map(|l| l.stride).collect()
    }

    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        FrameMeta,
        SmallVec<[PlaneLayout; 3]>,
        SmallVec<[Vec<u8>; 3]>,
    ) {
        let layouts = self.layouts.clone();
        if self.external.is_some() {
            (self.meta, layouts, SmallVec::new())
        } else {
            let buffers = self.buffers.into_iter().map(|lease| lease.take()).collect();
            (self.meta, layouts, buffers)
        }
    }

    pub fn materialize_owned(&self) -> Self {
        let max_len = self
            .layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(1)
            .max(1);
        let pool = BufferPool::with_limits(self.layouts.len().max(1), max_len, self.layouts.len());
        let buffers = self
            .planes()
            .into_iter()
            .zip(self.layouts.iter().copied())
            .map(|(plane, layout)| {
                let required = layout.offset.saturating_add(layout.len);
                let mut lease = pool.lease();
                lease.resize(required);
                if layout.len > 0 {
                    let dst = &mut lease.as_mut_slice()[layout.offset..required];
                    dst.copy_from_slice(plane.data());
                }
                lease
            })
            .collect();
        let mut meta = self.meta.clone();
        meta.residency = Some(FrameResidency::HostOwned);
        meta.mutability = FrameMutability::Mutable;
        FrameLease::multi_plane(meta, buffers, self.layouts.clone())
    }
}

pub fn plane_layout_from_dims(
    width: NonZeroU32,
    height: NonZeroU32,
    bytes_per_pixel: usize,
) -> PlaneLayout {
    let stride = width.get() as usize * bytes_per_pixel;
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

pub fn plane_layout_with_stride(
    _width: NonZeroU32,
    height: NonZeroU32,
    stride: usize,
) -> PlaneLayout {
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

fn default_owned_residency(code: crate::format::FourCc) -> FrameResidency {
    if matches!(
        code,
        c if c == crate::format::FourCc::new(*b"MJPG")
            || c == crate::format::FourCc::new(*b"JPEG")
            || c == crate::format::FourCc::new(*b"H264")
            || c == crate::format::FourCc::new(*b"H265")
            || c == crate::format::FourCc::new(*b"HEVC")
    ) {
        FrameResidency::CompressedPacket
    } else {
        FrameResidency::HostOwned
    }
}
