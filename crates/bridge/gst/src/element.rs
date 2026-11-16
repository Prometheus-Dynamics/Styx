//! The `styxsrc` element: a `GstPushSrc` that delivers Styx frames from the shared ring. Rust does
//! the frame work (the [`Source`] connects to the producer, holds each frame, and reconnects with
//! backoff); this file turns each frame into a `GstBuffer` whose memory is the frame's own memory.
//! Mapped frames are wrapped read-only, and frames with a descriptor are exported as dma-buf
//! memory. Nothing is copied. A frame is given back to the producer when GStreamer frees the memory
//! that holds it.

use std::ffi::CStr;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use gstreamer as gst;
use gstreamer_allocators as gst_allocators;
use gstreamer_base as gst_base;
use gstreamer_video as gst_video;

use gst::glib;
use gst::prelude::*;
use gst_allocators::DmaBufAllocator;
use gst_allocators::prelude::DmaBufAllocatorExtManual;
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use crate::source::{Delivered, Source};

/// How long one `create` waits for a frame before it checks for a flush again.
const POLL: Duration = Duration::from_millis(50);

/// Registers `styxsrc` in `plugin`.
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "styxsrc",
        gst::Rank::NONE,
        StyxSrc::static_type(),
    )
}

glib::wrapper! {
    /// The `styxsrc` element.
    pub struct StyxSrc(ObjectSubclass<imp::StyxSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

mod imp {
    use super::*;

    #[derive(Default, Debug)]
    pub struct StyxSrc {
        /// The `socket` property: the producer's socket path.
        socket: Mutex<Option<String>>,
        /// The source and what the element last set, touched only by the streaming thread and the
        /// start and stop calls.
        state: Mutex<State>,
        /// Set by `unlock`, so a `create` in progress gives up and returns a flushing flow.
        flushing: AtomicBool,
    }

    #[derive(Default, Debug)]
    pub(super) struct State {
        /// The producer's link, open between `start` and `stop`.
        pub(super) source: Option<Source>,
        /// The caps last set, to set them again only when the frame's format, size or rate changes.
        pub(super) caps: Option<CapsKey>,
        /// The dma-buf allocator, made on the first frame that has a descriptor.
        pub(super) dmabuf: Option<DmaBufAllocator>,
    }

    /// What the element's caps describe.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct CapsKey {
        /// The caps name: a raw format such as `NV12`, or `image/jpeg`.
        pub(super) format: &'static CStr,
        pub(super) width: u32,
        pub(super) height: u32,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StyxSrc {
        const NAME: &'static str = "StyxSrc";
        type Type = super::StyxSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for StyxSrc {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_live(true);
            obj.set_format(gst::Format::Time);
        }

        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("socket")
                        .nick("Socket")
                        .blurb("Unix socket of the Styx producer (the shared ring)")
                        .mutable_ready()
                        .build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "socket" => {
                    if let Ok(socket) = value.get::<Option<String>>() {
                        *lock(&self.socket) = socket;
                    }
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "socket" => lock(&self.socket).clone().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for StyxSrc {}

    impl ElementImpl for StyxSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Styx frame source",
                    "Source/Video",
                    "Frames from a Styx camera over the shared ring, without copying",
                    "Styx",
                )
            });
            Some(&METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps: gst::Caps = "video/x-raw; image/jpeg"
                    .parse()
                    .expect("the static caps parse");
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("the static pad template is valid"),
                ]
            });
            PAD_TEMPLATES.as_ref()
        }
    }

    impl BaseSrcImpl for StyxSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let socket = lock(&self.socket).clone().ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["the socket property is not set"]
                )
            })?;
            lock(&self.state).source = Some(Source::new(PathBuf::from(socket)));
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            // Dropping the source closes the producer's socket. Frames already handed on keep their
            // memory, and give their frames back when GStreamer frees it.
            let mut state = lock(&self.state);
            state.source = None;
            state.caps = None;
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    impl PushSrcImpl for StyxSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let mut state = lock(&self.state);
            let delivered = loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                let source = state.source.as_mut().ok_or(gst::FlowError::Error)?;
                if let Some(frame) = source.next(POLL) {
                    break frame;
                }
            };

            let compressed = delivered.compressed();
            let name = delivered.format().to_str().unwrap_or_default();
            let planes = delivered.layouts().len().min(4);
            let format = if compressed {
                gst_video::VideoFormat::Unknown
            } else {
                gst_video::VideoFormat::from_string(name)
            };
            if !compressed && (format == gst_video::VideoFormat::Unknown || planes == 0) {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["frame format {} is not supported", name]
                );
                return Err(gst::FlowError::Error);
            }

            let key = CapsKey {
                format: delivered.format(),
                width: delivered.width(),
                height: delivered.height(),
            };
            if state.caps != Some(key) {
                let caps = if compressed {
                    gst::Caps::builder("image/jpeg")
                        .field("width", key.width as i32)
                        .field("height", key.height as i32)
                        .build()
                } else {
                    gst_video::VideoInfo::builder(format, key.width, key.height)
                        .build()
                        .and_then(|info| info.to_caps())
                        .map_err(|_| gst::FlowError::NotNegotiated)?
                };
                self.obj()
                    .set_caps(&caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                state.caps = Some(key);
            }

            // What the buffer needs from the frame, read before the frame moves into its memory.
            let pts = delivered.pts_ns();
            let mut offsets = [0_usize; 4];
            let mut strides = [0_i32; 4];
            for (index, plane) in delivered.layouts().iter().take(4).enumerate() {
                offsets[index] = plane.offset;
                strides[index] = plane.stride as i32;
            }

            let Some(memory) = frame_memory(&mut state.dmabuf, delivered) else {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::Read,
                    ["a frame is neither mapped nor a descriptor"]
                );
                return Err(gst::FlowError::Error);
            };

            let mut buffer = gst::Buffer::new();
            {
                let buffer = buffer.make_mut();
                buffer.append_memory(memory);
                if !compressed {
                    gst_video::VideoMeta::add_full(
                        buffer,
                        gst_video::VideoFrameFlags::empty(),
                        format,
                        key.width,
                        key.height,
                        &offsets[..planes],
                        &strides[..planes],
                    )
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                }
                buffer.set_pts(gst::ClockTime::from_nseconds(pts));
            }
            Ok(CreateSuccess::NewBuffer(buffer))
        }
    }
}

/// A frame's bytes for `Memory::from_slice`: the frame is owned by the memory, so it is given back
/// when GStreamer frees the memory. `as_ref` is the producer's bytes, never a copy.
struct FrameBytes {
    frame: Delivered,
    len: usize,
}

impl AsRef<[u8]> for FrameBytes {
    fn as_ref(&self) -> &[u8] {
        self.frame
            .data()
            .and_then(|bytes| bytes.get(..self.len))
            .unwrap_or(&[])
    }
}

/// The GQuark under which a dma-buf memory holds its frame.
static FRAME_QUARK: LazyLock<glib::Quark> =
    LazyLock::new(|| glib::Quark::from_str("styx-frame-token"));

/// The memory that holds `frame`: a dma-buf over the frame's descriptor when it has one (as the
/// C element did, for memfd and dma-buf alike), else the mapped bytes wrapped read-only. `None`
/// when the frame has neither.
fn frame_memory(dmabuf: &mut Option<DmaBufAllocator>, frame: Delivered) -> Option<gst::Memory> {
    if let Some(fd) = frame.raw_fd() {
        let allocator = dmabuf.get_or_insert_with(DmaBufAllocator::new);
        // SAFETY: `fd` is the frame's own descriptor, open for as long as `frame` lives, and it is
        // only borrowed here to duplicate it before `frame` can go anywhere.
        let duplicate = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned();
        if let Ok(owned) = duplicate {
            let size = frame.payload_len();
            // SAFETY: `owned` is a valid descriptor this code owns, and the allocator takes
            // ownership of it (it is closed when the memory is freed), so nothing else closes it.
            if let Ok(memory) = unsafe { allocator.alloc_dmabuf::<OwnedFd>(owned, size) } {
                hold_frame(&memory, frame);
                return Some(memory);
            }
        }
    }
    frame.data()?;
    let len = frame.payload_len();
    Some(gst::Memory::from_slice(FrameBytes { frame, len }))
}

/// Holds `frame` for as long as `memory` lives: the frame is given back when GStreamer frees the
/// memory.
fn hold_frame(memory: &gst::Memory, frame: Delivered) {
    let token = Box::into_raw(Box::new(frame));
    // SAFETY: `memory` is a live GstMemory, which begins with its GstMiniObject header. The token is
    // a fresh box, and `release_frame` takes it back exactly once, when the memory is freed.
    unsafe {
        gst::ffi::gst_mini_object_set_qdata(
            memory.as_mut_ptr().cast(),
            glib::translate::IntoGlib::into_glib(*FRAME_QUARK),
            token.cast(),
            Some(release_frame),
        );
    }
}

/// Runs when GStreamer frees the memory that holds a frame: the frame goes back to the producer.
///
/// # Safety
///
/// `token` must be a box made by [`hold_frame`], and GStreamer calls this once for it.
unsafe extern "C" fn release_frame(token: glib::ffi::gpointer) {
    // SAFETY: `token` is the `Box<Delivered>` that `hold_frame` made, and this is its only release.
    drop(unsafe { Box::from_raw(token.cast::<Delivered>()) });
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
