//! DRM PRIME frames from a hardware decoder, handed on without copying.
//!
//! FFmpeg gives a hardware frame as an `AVFrame` in the `drm_prime` pixel format: `data[0]` is an
//! `AVDRMFrameDescriptor` that names the frame's dma-bufs (objects) and where each plane sits in
//! them (layers, planes). A single-object frame, every plane in one dma-buf, becomes a lease over
//! a [`styx_frame::FramePool`] buffer: the buffer is the dma-buf, registered the first time it is
//! seen, and later frames in it take the same buffer by index.
//!
//! The pool does not own FFmpeg's buffer; the `AVFrame` does. The sink keeps one `AVFrame`
//! reference per buffer, and moves a new frame into that slot only when the pool hands the slot
//! out, which it does only when no lease holds it ([`styx_frame::FramePool::is_free`]). So a
//! dma-buf is never returned to the decoder while a lease on it is out.
//!
//! Identity: FFmpeg may give the same dma-buf under a different descriptor (a dup'd fd), so a
//! buffer is named by its inode (`st_dev`, `st_ino` from `fstat` on the fd), not by the fd number.
//!
//! Multi-object frames (planes spread over several dma-bufs) are refused with the layout named:
//! styx-frame's buffers are one memory each, so such a frame needs multi-memory buffers there.

use std::ffi::{c_int, c_void};
use std::fmt;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::fd::{BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use styx_codec_api::CodecError;
use styx_format::{FourCc, MAX_PLANES, MediaFormat, Resolution, drm, plane_geometry};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, Layouts,
    PlaneLayout,
};

use crate::api::Api;
use crate::sys::{
    AvDrmFrame, DRM_MAX, FRAME_DATA, FRAME_FORMAT, FRAME_HEIGHT, FRAME_WIDTH, read_int, read_ptr,
};

/// The outcome of [`crate::FfmpegDecoder::decode_prime`]: a lease the decoder supplied, or the
/// frame dropped because its buffer is still held.
pub enum Decoded<M = ()> {
    /// A lease over the decoded frame, in the sink's pool.
    Frame(FrameLease<M>),
    /// The frame landed in buffer `index`, and a lease on it is still out. The frame was dropped
    /// (counted by [`PrimeSink::held`]); nothing waited.
    Held { index: usize },
}

impl<M> fmt::Debug for Decoded<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(lease) => f.debug_tuple("Frame").field(lease).finish(),
            Self::Held { index } => f.debug_struct("Held").field("index", index).finish(),
        }
    }
}

/// A single-object DRM PRIME frame, read from its descriptor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Described {
    /// The dma-buf's descriptor, open for as long as the frame is.
    pub(crate) object_fd: RawFd,
    /// The dma-buf's size in bytes.
    pub(crate) object_size: usize,
    pub(crate) format: MediaFormat,
    pub(crate) layouts: Layouts,
}

/// Why a descriptor is not a single-object frame, naming its layout.
fn layout_message(desc: &AvDrmFrame) -> String {
    let objects = desc.nb_objects.clamp(0, DRM_MAX as c_int);
    let layers = desc.nb_layers.clamp(0, DRM_MAX as c_int);
    let mut text = format!(
        "DRM PRIME frame is {} object(s) in {} layer(s):",
        desc.nb_objects, desc.nb_layers
    );
    for layer in 0..layers as usize {
        let planes = desc.layers[layer].nb_planes.clamp(0, DRM_MAX as c_int) as usize;
        let objects_of: Vec<c_int> = desc.layers[layer].planes[..planes]
            .iter()
            .map(|plane| plane.object_index)
            .collect();
        text.push_str(&format!(" layer {layer} planes in objects {objects_of:?};"));
    }
    text.push_str(&format!(
        " styx-frame's buffers hold one memory each, so only single-object frames (all planes in one dma-buf, {objects} object(s) here) are taken; a multi-object frame needs multi-memory buffers in styx-frame"
    ));
    text
}

/// Reads a single-object DRM PRIME descriptor of a `width` x `height` frame.
///
/// Refused (with a reason): a multi-object or multi-layer descriptor, a DRM format and modifier
/// with no Styx format, planes that do not fit the dma-buf, and, when `cpu_access` is set, a
/// tiled modifier (the CPU would read the tiles as rows).
pub(crate) fn describe(
    desc: &AvDrmFrame,
    width: u32,
    height: u32,
    cpu_access: bool,
) -> Result<Described, CodecError> {
    let malformed =
        |what: &str| CodecError::Failed(format!("malformed DRM PRIME descriptor: {what}"));
    if !(0..=DRM_MAX as c_int).contains(&desc.nb_objects)
        || !(0..=DRM_MAX as c_int).contains(&desc.nb_layers)
    {
        return Err(malformed("object or layer count out of range"));
    }
    if desc.nb_objects != 1 || desc.nb_layers != 1 {
        return Err(CodecError::Rejected(layout_message(desc)));
    }
    let layer = &desc.layers[0];
    if !(1..=DRM_MAX as c_int).contains(&layer.nb_planes) {
        return Err(malformed("plane count out of range"));
    }
    let planes = &layer.planes[..layer.nb_planes as usize];
    if planes.iter().any(|plane| plane.object_index != 0) {
        return Err(CodecError::Rejected(layout_message(desc)));
    }
    let object = &desc.objects[0];
    if object.fd < 0 || object.size == 0 {
        return Err(malformed("object has no descriptor or no size"));
    }
    let modifier = object.format_modifier;
    let fourcc_bytes = layer.format.to_le_bytes();
    let code = drm::from_drm(layer.format, modifier).ok_or_else(|| {
        CodecError::Rejected(format!(
            "no Styx format for DRM format {} with modifier {modifier:#x}",
            String::from_utf8_lossy(&fourcc_bytes)
        ))
    })?;
    if cpu_access && modifier != drm::DRM_FORMAT_MOD_LINEAR {
        return Err(CodecError::Rejected(format!(
            "modifier {modifier:#x} is tiled; CPU access needs a linear layout"
        )));
    }
    let geometry =
        plane_geometry(code, width, height).map_err(|e| CodecError::Rejected(e.to_string()))?;
    if geometry.len() != planes.len() {
        return Err(CodecError::Rejected(format!(
            "{code} has {} planes, the descriptor has {}",
            geometry.len(),
            planes.len()
        )));
    }

    let mut items = [PlaneLayout::default(); MAX_PLANES];
    for (i, (plane, shape)) in planes.iter().zip(geometry.iter()).enumerate() {
        let offset =
            usize::try_from(plane.offset).map_err(|_| malformed("negative plane offset"))?;
        let pitch = usize::try_from(plane.pitch).map_err(|_| malformed("negative pitch"))?;
        if shape.rows == 0 || pitch < shape.row_bytes {
            return Err(CodecError::Rejected(format!(
                "plane {i} pitch {pitch} is less than its {} bytes per row",
                shape.row_bytes
            )));
        }
        let len = pitch * (shape.rows - 1) + shape.row_bytes;
        let end = offset.saturating_add(len);
        if end > object.size {
            return Err(CodecError::Rejected(format!(
                "plane {i} runs to byte {end}, past the {}-byte dma-buf",
                object.size
            )));
        }
        items[i] = PlaneLayout::new(offset, pitch, len);
    }
    let layouts = Layouts::new(&items[..planes.len()])
        .ok_or_else(|| CodecError::Failed("plane layout does not fit a frame".into()))?;
    let resolution = Resolution::new(width, height)
        .ok_or_else(|| CodecError::Rejected("zero-sized DRM PRIME frame".into()))?;
    Ok(Described {
        object_fd: object.fd,
        object_size: object.size,
        format: MediaFormat::with_defaults(code, resolution),
        layouts,
    })
}

/// The `(st_dev, st_ino)` of the dma-buf behind `fd`.
pub(crate) fn identity(fd: RawFd) -> Result<(u64, u64), CodecError> {
    // SAFETY: `fd` is a descriptor the frame holds open, and it stays open for this call. The
    // File is wrapped in ManuallyDrop, so dropping it does not close the frame's descriptor.
    let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    file.metadata()
        .map(|meta| (meta.dev(), meta.ino()))
        .map_err(|e| CodecError::Failed(format!("fstat of the frame's dma-buf: {e}")))
}

/// The buffers a sink has registered, by inode. Sized once, to the pool's capacity.
pub(crate) struct Buffers {
    known: Vec<Known>,
}

#[derive(Clone, Copy)]
struct Known {
    dev: u64,
    ino: u64,
    index: usize,
}

impl Buffers {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            known: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.known.len()
    }
}

/// The pool buffer for the dma-buf a frame is in. A dma-buf first seen is dup'd and registered
/// once (read-only mapped when `cpu_access`, else handed on by descriptor only); later frames in
/// it get the same index.
pub(crate) fn resolve<M>(
    pool: &FramePool<M>,
    buffers: &mut Buffers,
    described: &Described,
    cpu_access: bool,
) -> Result<usize, CodecError> {
    let (dev, ino) = identity(described.object_fd)?;
    if let Some(known) = buffers
        .known
        .iter()
        .find(|known| known.dev == dev && known.ino == ino)
    {
        return Ok(known.index);
    }
    // SAFETY: the frame holds `object_fd` open for this call; `try_clone_to_owned` dups it, so the
    // pool owns a descriptor of its own and the frame's stays the frame's.
    let borrowed = unsafe { BorrowedFd::borrow_raw(described.object_fd) };
    let owned = borrowed
        .try_clone_to_owned()
        .map_err(|e| CodecError::Failed(format!("dup of the frame's dma-buf: {e}")))?;
    let memory: Box<dyn BufferMemory> = if cpu_access {
        Box::new(
            FdMemory::dmabuf(owned, described.object_size, false)
                .map_err(|e| CodecError::Failed(format!("mapping the dma-buf: {e}")))?,
        )
    } else {
        Box::new(FdMemory::dmabuf_unmapped(owned, described.object_size))
    };
    let index = pool
        .register(memory)
        .map_err(|e| CodecError::Rejected(format!("registering the frame's dma-buf: {e}")))?;
    // The pool refuses a registration past its capacity before this push, so the Vec (sized to
    // that capacity) never grows.
    buffers.known.push(Known { dev, ino, index });
    Ok(index)
}

/// Where a decoder's DRM PRIME frames are kept: a pool of the dma-bufs it produces (registered as
/// they first appear), and per buffer the `AVFrame` whose dma-buf it is, kept while a lease on the
/// buffer is out.
pub struct PrimeSink<M = ()> {
    api: Arc<Api>,
    pool: FramePool<M>,
    buffers: Buffers,
    /// Per buffer: the `AVFrame` holding that dma-buf, null until the buffer is first used.
    frames: Vec<*mut c_void>,
    cpu_access: bool,
    drm_pix: c_int,
    held: u64,
}

// SAFETY: the sink owns its `AVFrame`s, which are touched only through `&mut self` and freed in
// `Drop`; FFmpeg's frames may move between threads when no other thread uses them, as the decoder
// session does. The pool is `Send` when `M` is.
unsafe impl<M: Send + Sync> Send for PrimeSink<M> {}

impl<M: Default> PrimeSink<M> {
    /// A sink for frames of at most `payload` bytes in up to `capacity` dma-bufs.
    pub(crate) fn new(
        api: Arc<Api>,
        payload: usize,
        capacity: usize,
        cpu_access: bool,
    ) -> Result<Self, CodecError> {
        // SAFETY: the name is a valid NUL-terminated string for the call; the result is a pixel
        // format number or AV_PIX_FMT_NONE (-1).
        let drm_pix = unsafe { (api.util.av_get_pix_fmt)(c"drm_prime".as_ptr()) };
        if drm_pix < 0 {
            return Err(CodecError::Unavailable(
                "libavutil has no drm_prime pixel format".into(),
            ));
        }
        let pool = FramePool::with_capacity(BufferSize::new(payload, 0), capacity)
            .map_err(|e| CodecError::Failed(e.to_string()))?;
        Ok(Self {
            api,
            pool,
            buffers: Buffers::with_capacity(capacity),
            frames: vec![std::ptr::null_mut(); capacity],
            cpu_access,
            drm_pix,
            held: 0,
        })
    }
}

impl<M> PrimeSink<M> {
    /// Frames dropped because their buffer was still held.
    pub fn held(&self) -> u64 {
        self.held
    }

    /// Buffers registered so far.
    pub fn buffers(&self) -> usize {
        self.buffers.len()
    }

    /// The pool's counts (acquires, misses).
    pub fn stats(&self) -> styx_frame::PoolStats {
        self.pool.stats()
    }

    /// Takes the decoded DRM PRIME frame `frame` (an `AVFrame` the caller holds a reference to)
    /// as a lease for input `id`/`timestamp_ns`, expecting Styx format `expect`. On
    /// [`Decoded::Frame`] the sink took the frame's reference; on [`Decoded::Held`] it did not,
    /// and the caller unrefs the frame.
    pub(crate) fn accept(
        &mut self,
        frame: *mut c_void,
        expect: FourCc,
        id: u64,
        timestamp_ns: u64,
    ) -> Result<Decoded<M>, CodecError> {
        // SAFETY: `frame` is a live AVFrame the caller holds; its format and size fields are set.
        let (pix, width, height, data0) = unsafe {
            (
                read_int(frame, FRAME_FORMAT),
                read_int(frame, FRAME_WIDTH) as u32,
                read_int(frame, FRAME_HEIGHT) as u32,
                read_ptr(frame, FRAME_DATA),
            )
        };
        if pix != self.drm_pix {
            return Err(CodecError::Rejected(format!(
                "decoder produced pixel format {pix}, not DRM PRIME"
            )));
        }
        if data0.is_null() {
            return Err(CodecError::Failed(
                "DRM PRIME frame has no descriptor".into(),
            ));
        }
        // SAFETY: in a DRM PRIME frame `data[0]` is its AVDRMFrameDescriptor, which lives as long
        // as the frame; the frame is alive (checked by the caller's reference).
        let described = describe(
            unsafe { &*data0.cast::<AvDrmFrame>() },
            width,
            height,
            self.cpu_access,
        )?;
        if described.format.code != expect {
            return Err(CodecError::Failed(format!(
                "decoder produced {}, the descriptor says {expect}",
                described.format.code
            )));
        }
        let index = resolve(&self.pool, &mut self.buffers, &described, self.cpu_access)?;
        // `acquire_at` succeeds only when no lease holds the buffer, which is when the AVFrame in
        // its slot is no longer in use: moving the new frame into the slot releases the old one.
        match self.pool.acquire_at(
            index,
            FrameEntry::new(id, timestamp_ns, described.format, described.layouts),
        ) {
            Ok(lease) => {
                let slot = self.slot(index)?;
                // SAFETY: `slot` is this sink's AVFrame and `frame` the caller's; both are live.
                // The move leaves `frame` empty, and the caller's unref is then a no-op.
                unsafe { (self.api.util.av_frame_move_ref)(slot, frame) };
                Ok(Decoded::Frame(lease))
            }
            Err(AcquireError::Held { index }) => {
                self.held += 1;
                Ok(Decoded::Held { index })
            }
            Err(error) => Err(CodecError::Failed(error.to_string())),
        }
    }

    /// The slot's AVFrame, allocated the first time the buffer is used.
    fn slot(&mut self, index: usize) -> Result<*mut c_void, CodecError> {
        if self.frames[index].is_null() {
            // SAFETY: takes no arguments; returns an owned frame or null.
            let frame = unsafe { (self.api.util.av_frame_alloc)() };
            if frame.is_null() {
                return Err(CodecError::Failed(
                    "out of memory for an FFmpeg frame".into(),
                ));
            }
            self.frames[index] = frame;
        }
        Ok(self.frames[index])
    }
}

impl<M> fmt::Debug for PrimeSink<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrimeSink")
            .field("pool", &self.pool)
            .field("buffers", &self.buffers.len())
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

impl<M> Drop for PrimeSink<M> {
    fn drop(&mut self) {
        for (index, frame) in self.frames.iter_mut().enumerate() {
            if frame.is_null() {
                continue;
            }
            if !self.pool.is_free(index) {
                // A lease outlives the sink. Freeing the AVFrame now would release its dma-buf
                // under that lease, so the reference is kept: that buffer's frame is leaked.
                continue;
            }
            // SAFETY: the slot's frame is this sink's and no lease holds it; the free call takes
            // the address of the pointer and nulls it.
            unsafe { (self.api.util.av_frame_free)(frame as *mut *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_uint};
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use styx_format::drm::{DRM_FORMAT_MOD_LINEAR, MAPPINGS, drm_fourcc};
    use styx_frame::{BufferSize, PlaneLayout};

    use super::*;
    use crate::sys::{
        AvDrmFrame, AvDrmLayer, AvDrmObject, AvDrmPlane, FRAME_DATA, FRAME_FORMAT, FRAME_HEIGHT,
        FRAME_WIDTH, write_int, write_ptr,
    };

    const W: u32 = 64;
    const H: u32 = 48;
    /// NV12 at 64x48: 64x48 luma, then 64x24 interleaved chroma.
    const NV12_BYTES: usize = 64 * 48 * 3 / 2;

    // SAFETY (the block): memfd_create is a glibc entry point with this C signature.
    unsafe extern "C" {
        fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
    }

    /// A memfd of `len` bytes: a stand-in dma-buf, identified by its inode like one.
    fn memfd(len: usize) -> OwnedFd {
        // SAFETY: the name is a valid C string and the flags are 0; the result is a new
        // descriptor or -1.
        let fd = unsafe { memfd_create(c"styx-ffmpeg-test".as_ptr(), 0) };
        assert!(fd >= 0, "memfd_create failed");
        // SAFETY: `fd` is a fresh descriptor this call owns, so it is taken exactly once here.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64).expect("size the memfd");
        OwnedFd::from(file)
    }

    fn empty_descriptor() -> AvDrmFrame {
        AvDrmFrame {
            nb_objects: 0,
            objects: std::array::from_fn(|_| AvDrmObject {
                fd: -1,
                size: 0,
                format_modifier: 0,
            }),
            nb_layers: 0,
            layers: std::array::from_fn(|_| AvDrmLayer {
                format: 0,
                nb_planes: 0,
                planes: std::array::from_fn(|_| AvDrmPlane {
                    object_index: 0,
                    offset: 0,
                    pitch: 0,
                }),
            }),
        }
    }

    /// A one-object NV12 descriptor over `fd`: luma at 0, chroma at 3072, both 64 bytes a row.
    fn nv12(fd: RawFd, size: usize, modifier: u64) -> AvDrmFrame {
        let mut desc = empty_descriptor();
        desc.nb_objects = 1;
        desc.objects[0] = AvDrmObject {
            fd,
            size,
            format_modifier: modifier,
        };
        desc.nb_layers = 1;
        desc.layers[0].format = drm_fourcc(b"NV12");
        desc.layers[0].nb_planes = 2;
        desc.layers[0].planes[0] = AvDrmPlane {
            object_index: 0,
            offset: 0,
            pitch: 64,
        };
        desc.layers[0].planes[1] = AvDrmPlane {
            object_index: 0,
            offset: 3072,
            pitch: 64,
        };
        desc
    }

    fn entry(described: &Described, id: u64, timestamp_ns: u64) -> FrameEntry {
        FrameEntry::new(id, timestamp_ns, described.format, described.layouts)
    }

    #[test]
    fn nv12_descriptor_maps_to_its_planes() {
        let fd = memfd(NV12_BYTES);
        let desc = nv12(fd.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let described = describe(&desc, W, H, false).expect("single-object NV12");
        assert_eq!(described.format.code, FourCc::NV12);
        assert_eq!(described.object_size, NV12_BYTES);
        // Luma: 47 full pitches and one row. Chroma: 23 pitches and one row of 64 bytes.
        let layouts = described.layouts;
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0], PlaneLayout::new(0, 64, 64 * 47 + 64));
        assert_eq!(layouts[1], PlaneLayout::new(3072, 64, 64 * 23 + 64));
        assert_eq!(layouts.span(), Some(4608));
    }

    #[test]
    fn multi_object_descriptor_names_its_layout() {
        let fd = memfd(NV12_BYTES);
        let mut desc = nv12(fd.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        desc.nb_objects = 2;
        desc.objects[1] = AvDrmObject {
            fd: fd.as_raw_fd(),
            size: NV12_BYTES,
            format_modifier: DRM_FORMAT_MOD_LINEAR,
        };
        desc.layers[0].planes[1].object_index = 1;
        let Err(CodecError::Rejected(why)) = describe(&desc, W, H, false) else {
            panic!("a two-object frame is refused");
        };
        assert!(why.contains("2 object(s)"), "{why}");
        assert!(why.contains("[0, 1]"), "{why}");
        assert!(why.contains("multi-memory buffers"), "{why}");
    }

    #[test]
    fn plane_past_the_dmabuf_is_refused() {
        let fd = memfd(NV12_BYTES);
        let desc = nv12(fd.as_raw_fd(), NV12_BYTES - 16, DRM_FORMAT_MOD_LINEAR);
        let Err(CodecError::Rejected(why)) = describe(&desc, W, H, false) else {
            panic!("the chroma plane runs past the buffer");
        };
        assert!(why.contains("past the"), "{why}");
    }

    #[test]
    fn tiled_modifier_is_refused_for_cpu_access() {
        // A non-linear row of the mapping table: its layout is not rows in memory.
        let Some(tiled) = MAPPINGS
            .iter()
            .find(|m| m.drm.modifier != DRM_FORMAT_MOD_LINEAR)
        else {
            panic!("the mapping table has a non-linear row");
        };
        let geometry = plane_geometry(tiled.fourcc, W, H).expect("geometry");
        let fd = memfd(4096);
        let mut desc = empty_descriptor();
        desc.nb_objects = 1;
        desc.objects[0] = AvDrmObject {
            fd: fd.as_raw_fd(),
            size: 4096,
            format_modifier: tiled.drm.modifier,
        };
        desc.nb_layers = 1;
        desc.layers[0].format = tiled.drm.fourcc;
        desc.layers[0].nb_planes = geometry.len() as c_int;
        for (plane, shape) in desc.layers[0].planes.iter_mut().zip(geometry.iter()) {
            *plane = AvDrmPlane {
                object_index: 0,
                offset: 0,
                pitch: shape.row_bytes as isize,
            };
        }
        let Err(CodecError::Rejected(why)) = describe(&desc, W, H, true) else {
            panic!("a tiled frame is refused for CPU access");
        };
        assert!(why.contains("tiled"), "{why}");
    }

    #[test]
    fn dmabuf_registers_once_and_acquire_at_holds_it() {
        let fd = memfd(NV12_BYTES);
        let desc = nv12(fd.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let described = describe(&desc, W, H, false).unwrap();
        let pool: FramePool = FramePool::with_capacity(BufferSize::new(NV12_BYTES, 0), 2).unwrap();
        let mut buffers = Buffers::with_capacity(2);

        let first = resolve(&pool, &mut buffers, &described, false).unwrap();
        assert_eq!(first, 0);
        // The same dma-buf under another descriptor (a dup'd fd): same inode, same buffer.
        let dup = fd.try_clone().unwrap();
        let again = nv12(dup.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let again = describe(&again, W, H, false).unwrap();
        assert_eq!(resolve(&pool, &mut buffers, &again, false).unwrap(), 0);
        assert_eq!((buffers.len(), pool.buffers()), (1, 1));

        let lease = pool
            .acquire_at(0, entry(&described, 7, 99))
            .expect("buffer 0 is free");
        assert_eq!(
            (lease.buffer_index(), lease.id(), lease.timestamp_ns()),
            (0, 7, 99)
        );
        assert!(!pool.is_free(0));
        assert!(matches!(
            pool.acquire_at(0, entry(&described, 8, 100)),
            Err(AcquireError::Held { index: 0 })
        ));
        drop(lease);
        assert!(pool.is_free(0));
        let lease = pool
            .acquire_at(0, entry(&described, 9, 101))
            .expect("free again");
        drop(lease);

        // A second dma-buf takes the second slot; a third is refused, not waited for.
        let other = memfd(NV12_BYTES);
        let other_desc = nv12(other.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let other_described = describe(&other_desc, W, H, false).unwrap();
        assert_eq!(
            resolve(&pool, &mut buffers, &other_described, false).unwrap(),
            1
        );
        let third = memfd(NV12_BYTES);
        let third_desc = nv12(third.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let third_described = describe(&third_desc, W, H, false).unwrap();
        let Err(CodecError::Rejected(why)) = resolve(&pool, &mut buffers, &third_described, false)
        else {
            panic!("a third dma-buf has no slot");
        };
        assert!(why.contains("already has"), "{why}");
    }

    #[test]
    fn cpu_access_reads_the_dmabuf_in_place() {
        let fd = memfd(NV12_BYTES);
        let mut file = File::from(fd.try_clone().unwrap());
        let pattern: Vec<u8> = (0..NV12_BYTES).map(|i| (i % 251) as u8).collect();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&pattern).unwrap();
        let desc = nv12(fd.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let described = describe(&desc, W, H, true).unwrap();
        let pool: FramePool = FramePool::with_capacity(BufferSize::new(NV12_BYTES, 0), 1).unwrap();
        let mut buffers = Buffers::with_capacity(1);
        let index = resolve(&pool, &mut buffers, &described, true).unwrap();
        let lease = pool.acquire_at(index, entry(&described, 1, 1)).unwrap();
        let read = lease.read().expect("mapped for reading");
        // Row 1 of luma and row 0 of chroma are read where they lie in the dma-buf.
        let luma = read.plane(0).unwrap().row_bytes(1, 64).unwrap();
        assert_eq!(luma, &pattern[64..128]);
        let chroma = read.plane(1).unwrap().row_bytes(0, 64).unwrap();
        assert_eq!(chroma, &pattern[3072..3136]);
    }

    #[test]
    fn empty_descriptor_is_refused_with_its_counts() {
        let Err(CodecError::Rejected(why)) = describe(&empty_descriptor(), W, H, false) else {
            panic!("a descriptor with no objects or layers is refused");
        };
        assert!(why.contains("0 object(s) in 0 layer(s)"), "{why}");
    }

    /// The sink end to end on FFmpeg's `AVFrame`s, with no decoder: each test frame is an
    /// `AVFrame` in `drm_prime` whose `data[0]` is the descriptor above. Needs libavutil; skips
    /// with the reason otherwise.
    #[test]
    fn sink_holds_a_dmabuf_while_its_lease_is_out() {
        let api = match crate::api::system() {
            Ok(api) => api,
            Err(why) => {
                eprintln!("skipped sink_holds_a_dmabuf_while_its_lease_is_out: {why}");
                return;
            }
        };
        let fd = memfd(NV12_BYTES);
        let mut desc = nv12(fd.as_raw_fd(), NV12_BYTES, DRM_FORMAT_MOD_LINEAR);
        let mut sink: PrimeSink = PrimeSink::new(api.clone(), NV12_BYTES, 2, false).unwrap();
        let drm_pix = sink.drm_pix;
        let make_frame = |desc: *mut AvDrmFrame| {
            // SAFETY: takes no arguments; returns an owned frame or null, checked below.
            let frame = unsafe { (api.util.av_frame_alloc)() };
            assert!(!frame.is_null());
            // SAFETY: `frame` is a fresh AVFrame; these are its format, size and data[0] fields,
            // and `desc` points at a descriptor the test keeps alive for the frame's use.
            unsafe {
                write_int(frame, FRAME_FORMAT, drm_pix);
                write_int(frame, FRAME_WIDTH, W as c_int);
                write_int(frame, FRAME_HEIGHT, H as c_int);
                write_ptr(frame, FRAME_DATA, desc.cast());
            }
            frame
        };
        let discard = |frame: *mut c_void| {
            // SAFETY: the frame is one the test owns (it was not given to the sink); unref then
            // free, and the free call nulls the local pointer.
            unsafe {
                (api.util.av_frame_unref)(frame);
                let mut frame = frame;
                (api.util.av_frame_free)(&mut frame);
            }
        };

        let first = make_frame(&mut desc);
        let Decoded::Frame(lease) = sink.accept(first, FourCc::NV12, 1, 10).unwrap() else {
            panic!("the first frame takes buffer 0");
        };
        assert_eq!(lease.buffer_index(), 0);

        // The second frame lands in the dma-buf the lease still holds: dropped, counted.
        let second = make_frame(&mut desc);
        match sink.accept(second, FourCc::NV12, 2, 20).unwrap() {
            Decoded::Held { index } => assert_eq!(index, 0),
            Decoded::Frame(_) => panic!("a held buffer is not handed out again"),
        }
        discard(second);
        assert_eq!(sink.held(), 1);

        // Released, the buffer takes the next frame.
        drop(lease);
        let third = make_frame(&mut desc);
        let Decoded::Frame(lease) = sink.accept(third, FourCc::NV12, 3, 30).unwrap() else {
            panic!("a released buffer takes a frame");
        };
        assert_eq!((lease.id(), lease.timestamp_ns()), (3, 30));
        drop(lease);
        assert_eq!(sink.buffers(), 1);
        assert_eq!(sink.stats().misses, 1);
    }
}
