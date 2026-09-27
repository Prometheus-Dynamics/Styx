//! Containers and network streams (libavformat), loaded on first use.

use std::ffi::{c_int, c_void};
use std::ptr;

use ffmpeg_sys_next as raw;

use super::codec::Parameters;
use super::error::check;
use super::packet::Packet;
use super::{Error, cstring, loader};

pub mod media {
    /// A stream's media type.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Type {
        Video,
        Audio,
    }
}

pub mod format {
    use super::*;

    /// Open a file or URL for reading.
    pub fn input(path: impl AsRef<std::path::Path>) -> Result<context::Input, Error> {
        let path = path.as_ref().to_string_lossy().into_owned();
        context::Input::open(&path, None)
    }

    /// Open a URL, aborting blocking I/O when `interrupt` returns `true`.
    pub fn input_with_interrupt(
        url: &str,
        interrupt: impl Fn() -> bool + Send + 'static,
    ) -> Result<context::Input, Error> {
        context::Input::open(url, Some(Box::new(interrupt)))
    }

    pub mod context {
        use super::*;

        type Interrupt = Box<dyn Fn() -> bool + Send>;

        /// An opened input (`AVFormatContext`).
        pub struct Input {
            ptr: *mut raw::AVFormatContext,
            // Referenced by the context's interrupt callback; boxed so its address is stable.
            _interrupt: Option<Box<Interrupt>>,
        }

        // SAFETY: used by one thread at a time.
        unsafe impl Send for Input {}

        extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
            // SAFETY: `opaque` points at the boxed closure owned by the Input.
            let interrupt = unsafe { &*(opaque as *const Interrupt) };
            interrupt() as c_int
        }

        impl Input {
            pub(super) fn open(url: &str, interrupt: Option<Interrupt>) -> Result<Self, Error> {
                let fmt = loader::format().map_err(Error::Unavailable)?;
                let url = cstring(url)?;
                let interrupt = interrupt.map(Box::new);
                // SAFETY: allocate a context, install the callback, then open into it
                // (avformat_open_input frees the context on failure).
                unsafe {
                    let mut ctx = (fmt.avformat_alloc_context)();
                    if ctx.is_null() {
                        return Err(Error::Other {
                            errno: libc::ENOMEM,
                        });
                    }
                    if let Some(cb) = &interrupt {
                        (*ctx).interrupt_callback = raw::AVIOInterruptCB {
                            callback: Some(interrupt_cb),
                            opaque: &**cb as *const Interrupt as *mut c_void,
                        };
                    }
                    check((fmt.avformat_open_input)(
                        &mut ctx,
                        url.as_ptr(),
                        ptr::null(),
                        ptr::null_mut(),
                    ))?;
                    let input = Self {
                        ptr: ctx,
                        _interrupt: interrupt,
                    };
                    check((fmt.avformat_find_stream_info)(ctx, ptr::null_mut()))?;
                    Ok(input)
                }
            }

            pub fn streams(&self) -> Streams<'_> {
                Streams { input: self }
            }

            pub fn stream(&self, index: usize) -> Option<Stream<'_>> {
                // SAFETY: valid opened context.
                let count = unsafe { (*self.ptr).nb_streams } as usize;
                (index < count).then(|| Stream {
                    // SAFETY: index < nb_streams.
                    ptr: unsafe { *(*self.ptr).streams.add(index) },
                    _input: self,
                })
            }

            /// Packets in file order, with the stream each belongs to.
            pub fn packets(&mut self) -> PacketIter<'_> {
                PacketIter { input: self }
            }
        }

        impl Drop for Input {
            fn drop(&mut self) {
                if let Ok(fmt) = loader::format() {
                    // SAFETY: owned opened context.
                    unsafe { (fmt.avformat_close_input)(&mut self.ptr) };
                }
            }
        }

        pub struct Streams<'a> {
            input: &'a Input,
        }

        impl<'a> Streams<'a> {
            pub fn best(&self, kind: media::Type) -> Option<Stream<'a>> {
                let fmt = loader::format().ok()?;
                let media = match kind {
                    media::Type::Video => raw::AVMediaType::AVMEDIA_TYPE_VIDEO,
                    media::Type::Audio => raw::AVMediaType::AVMEDIA_TYPE_AUDIO,
                };
                // SAFETY: valid opened context.
                let index = unsafe {
                    (fmt.av_find_best_stream)(self.input.ptr, media, -1, -1, ptr::null_mut(), 0)
                };
                (index >= 0)
                    .then(|| self.input.stream(index as usize))
                    .flatten()
            }
        }

        /// A stream of an input (`AVStream`), borrowed.
        pub struct Stream<'a> {
            ptr: *mut raw::AVStream,
            _input: &'a Input,
        }

        impl Stream<'_> {
            pub fn index(&self) -> usize {
                // SAFETY: valid stream of an open input.
                unsafe { (*self.ptr).index.max(0) as usize }
            }

            pub fn parameters(&self) -> Parameters {
                // SAFETY: valid stream codec parameters.
                Parameters::copy_from(unsafe { (*self.ptr).codecpar })
                    .expect("copy stream parameters")
            }

            /// Number of frames, when the container records it (0 otherwise).
            pub fn frames(&self) -> i64 {
                // SAFETY: valid stream.
                unsafe { (*self.ptr).nb_frames }
            }

            pub fn avg_frame_rate(&self) -> Rational {
                // SAFETY: valid stream.
                let r = unsafe { (*self.ptr).avg_frame_rate };
                Rational(r.num, r.den)
            }
        }

        /// A rational number (e.g. a frame rate).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Rational(pub i32, pub i32);

        impl Rational {
            pub fn numerator(&self) -> i32 {
                self.0
            }

            pub fn denominator(&self) -> i32 {
                self.1
            }
        }

        impl From<Rational> for f64 {
            fn from(r: Rational) -> f64 {
                if r.1 == 0 {
                    0.0
                } else {
                    r.0 as f64 / r.1 as f64
                }
            }
        }

        pub struct PacketIter<'a> {
            input: &'a mut Input,
        }

        impl<'a> Iterator for PacketIter<'a> {
            type Item = (Stream<'a>, Packet);

            fn next(&mut self) -> Option<Self::Item> {
                let fmt = loader::format().ok()?;
                let mut packet = Packet::empty();
                loop {
                    // SAFETY: valid opened context and owned packet.
                    let ret = unsafe { (fmt.av_read_frame)(self.input.ptr, packet.as_mut_ptr()) };
                    if ret == -libc::EAGAIN {
                        continue;
                    }
                    if ret < 0 {
                        return None;
                    }
                    let index = packet.stream();
                    // SAFETY: the stream outlives the iterator's borrow of the input.
                    let input: &'a Input = unsafe { &*(self.input as *const Input) };
                    return input.stream(index).map(|stream| (stream, packet));
                }
            }
        }
    }
}
