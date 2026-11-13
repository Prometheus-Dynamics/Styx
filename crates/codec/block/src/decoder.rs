//! The HEVC decoder: an access unit in, a picture out, through a block session.
//!
//! Each frame is one IDR slice. The caller puts the slice's Annex B bytes (start code first) at
//! the start of the input lease's buffer; the parameter sets come from
//! [`HevcDecoder::set_parameter_sets`]. Styx parses the slice, builds its controls
//! ([`crate::hevc::controls`]) and the job's work ([`crate::work`]), and runs it on a [`Session`].
//! The session is the block: the live one posts the job on Lemnos's `codec.block/1` job stream
//! and waits for its completion; a test runs a simulated one.
//!
//! Each job is stamped by the decoder ([`crate::work::Stamps`]), not by the input's timestamp: the
//! block takes only whole microseconds, and a reference names its picture by stamp.
//!
//! Recovery is the block link's ([`crate::link`]): a failed session is reopened after a backoff
//! that starts at 250 ms and doubles to 8 s, and the decoder never gives up and never turns
//! itself off. A job the block refuses ([`CodecError::Rejected`]) leaves the session open and
//! sets no backoff.
//!
//! Provenance: the decode logic and recovery policy are moved from Lemnos
//! `drivers/m2m-decode` (`stateless.rs` and `decoder.rs`, lock 04d09360). Lemnos keeps the
//! M2M session (queues, buffers, requests).

use lemnos_linux_sys::v4l2::hevc as sys;
use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Decoder, FourCc, FrameLease,
    Implementation,
};

use crate::hevc;
use crate::hevc::controls::{START_CODE_LEN, idr_slice, pps_control, sps_control};
use crate::link::Link;
use crate::work::{self, Job, Setup, Stamps};

/// The name the decoder registers under.
pub const NAME: &str = "hevc-v4l2-stateless";

/// A block session: where a decode job runs. The live one is the Lemnos job stream; tests use a
/// simulated one.
pub trait Session: Send {
    /// Runs one job whose work is `work`, and returns the completion's read-back count.
    ///
    /// [`CodecError::Rejected`] means the block refused the work: nothing reached the block and
    /// the session stays open. [`CodecError::Failed`] or [`CodecError::Unavailable`] means the
    /// block failed: the decoder closes the session and reopens it after a backoff.
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError>;
}

/// The parameter sets the slices refer to.
struct ParameterSets {
    sps: hevc::Sps,
    pps: hevc::Pps,
}

/// The HEVC stateless decoder over one block session.
pub struct HevcDecoder {
    descriptor: CodecDescriptor,
    link: Link,
    sets: Option<ParameterSets>,
    /// Unescaped RBSP scratch for the headers. Grows once to the largest slice seen.
    scratch: Vec<u8>,
    /// The work of one job, allocated once.
    work: Box<[u8]>,
    /// The stamps of this decoder's jobs.
    stamps: Stamps,
}

impl HevcDecoder {
    /// A decoder over the sessions `open` opens. Nothing is opened until the first frame.
    pub fn new(
        open: impl FnMut() -> Result<Box<dyn Session>, CodecError> + Send + 'static,
    ) -> Self {
        Self {
            descriptor: descriptor(),
            link: Link::new(Box::new(open)),
            sets: None,
            scratch: Vec::new(),
            work: vec![0; work::WORK_BYTES].into_boxed_slice(),
            stamps: Stamps::default(),
        }
    }

    /// Whether a session is open.
    pub fn is_open(&self) -> bool {
        self.link.is_open()
    }

    /// Failed sessions in a row. 0 after a job completes.
    pub fn failures(&self) -> u32 {
        self.link.failures()
    }

    /// Reads the VPS, SPS and PPS of an Annex B byte stream and keeps the SPS and PPS the
    /// slices are decoded with. A later call replaces them.
    pub fn set_parameter_sets(&mut self, annexb: &[u8]) -> Result<(), CodecError> {
        let mut sps = None;
        let mut pps = None;
        if self.scratch.len() < annexb.len() {
            self.scratch.resize(annexb.len(), 0);
        }
        for nal in hevc::nal_units(annexb) {
            let n = hevc::unescape(nal.raw, &mut self.scratch)
                .map_err(|e| CodecError::Rejected(format!("parameter set: {e:?}")))?;
            match nal.unit_type {
                33 => {
                    sps = Some(
                        hevc::parse_sps(&self.scratch[..n])
                            .map_err(|e| CodecError::Rejected(format!("SPS: {e:?}")))?,
                    )
                }
                34 => {
                    pps = Some(
                        hevc::parse_pps(&self.scratch[..n])
                            .map_err(|e| CodecError::Rejected(format!("PPS: {e:?}")))?,
                    )
                }
                _ => {}
            }
        }
        match (sps, pps) {
            (Some(sps), Some(pps)) => {
                self.sets = Some(ParameterSets { sps, pps });
                Ok(())
            }
            _ => Err(CodecError::Rejected("the stream has no SPS and PPS".into())),
        }
    }
}

impl std::fmt::Debug for HevcDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HevcDecoder")
            .field("link", &self.link)
            .finish()
    }
}

/// The reason a frame is refused: its input is not an IDR slice the parameter sets describe.
fn rejected(what: impl std::fmt::Display) -> CodecError {
    CodecError::Rejected(what.to_string())
}

impl<M> Codec<M> for HevcDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Decoder<M> for HevcDecoder {
    fn decode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let Some(sets) = self.sets.as_ref() else {
            return Err(rejected("no parameter sets: call set_parameter_sets first"));
        };
        let format = output.format();
        if format.code != FourCc::NV12 {
            return Err(rejected(format!("picture format {}", format.code)));
        }
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let picture =
            u32::try_from(output.buffer_index()).map_err(|_| rejected("picture buffer number"))?;

        // The slice: the input's first NAL unit, with its start code, at the buffer's start.
        let au = input
            .read()
            .ok_or_else(|| rejected("the input has no CPU view"))?;
        let data = au
            .plane(0)
            .ok_or_else(|| rejected("the input has no plane"))?
            .data;
        if data.get(..START_CODE_LEN) != Some(&[0, 0, 1]) {
            return Err(rejected("the slice does not start with a start code"));
        }
        let nal = hevc::nal_units(data)
            .next()
            .ok_or_else(|| rejected("no NAL unit"))?;
        if data.get(START_CODE_LEN..START_CODE_LEN + nal.raw.len()) != Some(nal.raw) {
            return Err(rejected("the slice is not the input's first NAL unit"));
        }
        let source_len = START_CODE_LEN + nal.raw.len();
        let source =
            u32::try_from(input.buffer_index()).map_err(|_| rejected("bitstream buffer number"))?;

        if self.scratch.len() < nal.raw.len() {
            self.scratch.resize(nal.raw.len(), 0);
        }
        let n = hevc::unescape(nal.raw, &mut self.scratch).map_err(rejected)?;
        let header =
            hevc::parse_slice_header(&self.scratch[..n], nal.unit_type, &sets.sps, &sets.pps)
                .map_err(rejected)?;
        let idr = idr_slice(
            &header,
            nal.raw,
            nal.unit_type,
            nal.temporal_id_plus1,
            source_len,
        )
        .map_err(rejected)?;
        let sps = sps_control(&sets.sps).map_err(rejected)?;
        let pps = pps_control(&sets.pps).map_err(rejected)?;

        let slice_bytes = idr.slice;
        let decode_bytes = idr.decode;
        let controls: [(u32, &[u8]); 6] = [
            (sys::CID_SPS, sps.as_bytes()),
            (sys::CID_PPS, pps.as_bytes()),
            (sys::CID_SLICE_PARAMS, slice_bytes.as_bytes()),
            (sys::CID_DECODE_PARAMS, decode_bytes.as_bytes()),
            (
                sys::CID_DECODE_MODE,
                &sys::DECODE_MODE_FRAME_BASED.to_le_bytes(),
            ),
            (sys::CID_START_CODE, &sys::START_CODE_ANNEX_B.to_le_bytes()),
        ];
        let stamp_ns = self
            .stamps
            .advance()
            .ok_or_else(|| rejected("this decoder has no stamps left"))?;
        let job = Job {
            setup: Setup {
                coded: sys::PIX_FMT_HEVC_SLICE,
                width,
                height,
                picture: FourCc::NV12.to_u32(),
                planes: 1,
            },
            stamp_ns,
            source,
            source_len: u32::try_from(source_len).map_err(|_| rejected("slice length"))?,
            picture,
            controls: &controls,
        };
        let len = work::build(&mut self.work, &job)
            .map_err(|_| rejected("the job's work does not fit the block"))?;

        // The work leaves `self` for the call, so the session can be borrowed with it.
        let work = std::mem::take(&mut self.work);
        let result = self.link.run(&work[..len]);
        self.work = work;
        result
    }
}

/// The decoder's descriptor: HEVC bitstream in, NV12 pictures out, on the native backend.
pub fn descriptor() -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Decoder,
        input: FourCc::HEVC,
        output: FourCc::NV12,
        backend: Backend::Native,
        name: NAME,
    }
}

/// A registry candidate for the decoder. `open` opens a session on the block; the candidate
/// builds a fresh decoder over it each time the registry asks.
pub fn candidate<M: 'static>(
    open: impl Fn() -> Result<Box<dyn Session>, CodecError> + Send + Sync + Clone + 'static,
) -> Candidate<M> {
    Candidate::new(descriptor(), move || {
        let open = open.clone();
        Ok(Implementation::Decoder(Box::new(HevcDecoder::new(open))))
    })
}
