//! The H.264 decoder: a slice in, a picture out, through a block session.
//!
//! Each job is one slice of a frame: the caller puts the slice's Annex B bytes (start code first)
//! at the start of the input lease's buffer. The parameter sets come from
//! [`H264Decoder::set_parameter_sets`]. Styx parses the slice, works out its order counts and
//! reference picture lists, builds its controls ([`crate::h264::controls`], in the V4L2 structures
//! of Lemnos), and runs the job's work ([`crate::h264::work`]) on a [`Session`] (the block link,
//! [`crate::link`]).
//!
//! A picture is the slices of one frame, and a slice with `first_mb_in_slice` 0 starts a picture
//! (arbitrary slice order is not supported). The picture enters the reference tracker when the
//! next picture starts, or at [`H264Decoder::flush`]: every slice of a picture sees the same
//! tracker, so the picture cannot be marked before its last slice. Its stamp in the tracker is
//! the stamp of its last slice's job, which is the one the block gives the capture buffer.
//!
//! Each job is stamped by the decoder ([`Stamps`]), a whole number of microseconds and unique,
//! and a reference names its picture by that stamp in the controls.
//!
//! Recovery is the block link's, as for HEVC. A slice refused by the parser, the tracker or the
//! controls is refused before any job: nothing reaches the block. A job that fails leaves the
//! tracker as it was, apart from the picture before it, whose slices all completed.
//!
//! Provenance: written for Styx from ITU-T H.264 (7.3, 8.2). The block's structures are Lemnos's
//! `lemnos_linux_sys::v4l2::h264`.

use lemnos_linux_sys::v4l2::h264 as sys;
use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Decoder, FourCc, FrameLease,
    Implementation,
};

use crate::bitstream::unescape;
use crate::decoder::Session;
use crate::h264::controls::{self, Controls, SliceView};
use crate::h264::dpb::{Dpb, NONE};
use crate::h264::poc::{Pic, PocState};
use crate::h264::ps::{Pps, Sps, parse_pps, parse_sps, peek_pps_id};
use crate::h264::slice::{SliceHeader, parse_slice_header};
use crate::h264::work::{self, Job, Setup};
use crate::h264::{NalUnit, nal_units};
use crate::link::Link;
use crate::work::Stamps;

/// The name the decoder registers under.
pub const NAME: &str = "h264-v4l2-stateless";
/// The length of the Annex B start code in front of a slice.
const START_CODE_LEN: usize = 3;

/// The picture whose slices are being decoded, not yet in the tracker.
#[derive(Debug)]
struct Current {
    /// The first slice's header: the picture's marking.
    header: SliceHeader,
    sps: Sps,
    pic: Pic,
    /// The stamp of the picture's last slice's job.
    stamp_ns: u64,
}

/// The H.264 stateless decoder over one block session.
pub struct H264Decoder {
    descriptor: CodecDescriptor,
    link: Link,
    spss: Vec<Sps>,
    ppss: Vec<Pps>,
    poc: PocState,
    dpb: Dpb,
    current: Option<Current>,
    /// Unescaped RBSP scratch. Grows once to the largest unit seen.
    scratch: Vec<u8>,
    controls: Controls,
    /// The work of one job, allocated once.
    work: Box<[u8]>,
    stamps: Stamps,
}

impl H264Decoder {
    /// A decoder over the sessions `open` opens. Nothing is opened until the first frame.
    pub fn new(
        open: impl FnMut() -> Result<Box<dyn Session>, CodecError> + Send + 'static,
    ) -> Self {
        Self {
            descriptor: descriptor(),
            link: Link::new(Box::new(open)),
            spss: Vec::new(),
            ppss: Vec::new(),
            poc: PocState::default(),
            dpb: Dpb::new(),
            current: None,
            scratch: Vec::new(),
            controls: Controls::default(),
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

    /// The reference picture tracker, read-only. It holds the pictures before the current one.
    pub fn dpb(&self) -> &Dpb {
        &self.dpb
    }

    /// Puts the picture being decoded into the tracker, as the next picture would. Call it at the
    /// end of a stream so its last picture can be a reference.
    pub fn flush(&mut self) {
        if let Some(current) = self.current.take() {
            self.dpb.mark(
                &current.header,
                &current.sps,
                current.stamp_ns,
                current.pic.stored(),
            );
            self.poc.commit(&current.header, &current.pic);
        }
    }

    /// Reads the SPSs and PPSs of an Annex B byte stream and keeps them by id: a set with an id
    /// already kept replaces it. Needs at least one SPS and one PPS.
    pub fn set_parameter_sets(&mut self, annexb: &[u8]) -> Result<(), CodecError> {
        let mut found_sps = Vec::new();
        let mut found_pps = Vec::new();
        if self.scratch.len() < annexb.len() {
            self.scratch.resize(annexb.len(), 0);
        }
        for nal in nal_units(annexb) {
            let n = unescape(nal.raw, &mut self.scratch)
                .map_err(|e| CodecError::Rejected(format!("parameter set: {e}")))?;
            match nal.unit_type {
                7 => found_sps.push(
                    parse_sps(&self.scratch[..n])
                        .map_err(|e| CodecError::Rejected(format!("SPS: {e}")))?,
                ),
                8 => {
                    let known = &self.spss;
                    let pps = parse_pps(&self.scratch[..n], |id| {
                        found_sps
                            .iter()
                            .chain(known.iter())
                            .find(|s| s.id == id)
                            .map(|s| s.chroma_format_idc)
                    })
                    .map_err(|e| CodecError::Rejected(format!("PPS: {e}")))?;
                    found_pps.push(pps);
                }
                _ => {}
            }
        }
        for sps in found_sps {
            self.spss.retain(|s| s.id != sps.id);
            self.spss.push(sps);
        }
        for pps in found_pps {
            self.ppss.retain(|p| p.id != pps.id);
            self.ppss.push(pps);
        }
        if self.spss.is_empty() || self.ppss.is_empty() {
            return Err(CodecError::Rejected("the stream has no SPS and PPS".into()));
        }
        Ok(())
    }
}

impl std::fmt::Debug for H264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264Decoder")
            .field("link", &self.link)
            .field("sps", &self.spss.len())
            .field("pps", &self.ppss.len())
            .finish()
    }
}

/// The reason a frame is refused.
fn rejected(what: impl std::fmt::Display) -> CodecError {
    CodecError::Rejected(what.to_string())
}

impl<M> Codec<M> for H264Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Decoder<M> for H264Decoder {
    fn decode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        if self.spss.is_empty() || self.ppss.is_empty() {
            return Err(rejected("no parameter sets: call set_parameter_sets first"));
        }
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
        let source =
            u32::try_from(input.buffer_index()).map_err(|_| rejected("bitstream buffer number"))?;

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
        let NalUnit {
            nal_ref_idc,
            unit_type,
            raw,
        } = nal_units(data)
            .next()
            .ok_or_else(|| rejected("no NAL unit"))?;
        if data.get(START_CODE_LEN..START_CODE_LEN + raw.len()) != Some(raw) {
            return Err(rejected("the slice is not the input's first NAL unit"));
        }
        if unit_type != 1 && unit_type != 5 {
            return Err(rejected("the first NAL unit is not a frame slice"));
        }
        let source_len = START_CODE_LEN + raw.len();
        if self.scratch.len() < raw.len() {
            self.scratch.resize(raw.len(), 0);
        }
        let n = unescape(raw, &mut self.scratch).map_err(rejected)?;

        // The slice's parameter sets, by index, and its header. The header is owned: nothing
        // below borrows the scratch.
        let (sps_at, pps_at, header) = {
            let rbsp = &self.scratch[..n];
            let pps_id = peek_pps_id(rbsp).map_err(rejected)?;
            let pps_at = self
                .ppss
                .iter()
                .position(|p| p.id == pps_id)
                .ok_or_else(|| rejected("the slice's PPS is not known"))?;
            let sps_id = self.ppss[pps_at].sps_id;
            let sps_at = self
                .spss
                .iter()
                .position(|s| s.id == sps_id)
                .ok_or_else(|| rejected("the slice's SPS is not known"))?;
            let header = parse_slice_header(
                rbsp,
                unit_type,
                nal_ref_idc,
                &self.spss[sps_at],
                &self.ppss[pps_at],
            )
            .map_err(rejected)?;
            (sps_at, pps_at, header)
        };
        let (w, h) = self.spss[sps_at].frame_size();
        if (w, h) != (width, height) {
            return Err(rejected(format!(
                "the picture is {w}x{h}, not {width}x{height}"
            )));
        }

        // A slice that starts a picture ends the one before it: that picture goes into the
        // tracker, and this one's checks and counts see the tracker as it then is. A slice that
        // starts no picture needs the one it belongs to to be open.
        let starts_picture = header.first_mb_in_slice == 0;
        if !starts_picture && self.current.is_none() {
            return Err(rejected(
                "a slice that starts no picture, and the picture it belongs to is not open",
            ));
        }
        if starts_picture {
            self.flush();
        }
        let sps = &self.spss[sps_at];
        let pps = &self.ppss[pps_at];
        if starts_picture {
            self.dpb.check_frame_num(&header, sps).map_err(rejected)?;
        }
        let pic = match (&self.current, starts_picture) {
            (Some(current), false) => current.pic,
            _ => self.poc.compute(&header, sps).map_err(rejected)?,
        };
        let lists = self.dpb.lists(&header, sps, pic.poc).map_err(rejected)?;
        // Every active entry of a conforming slice names a reference picture: a P slice with no
        // reference (a stream that starts with it, or a lost IDR) is refused, not decoded.
        for list in 0..2 {
            if lists.list(list).contains(&NONE) {
                return Err(rejected("a reference picture the slice needs is missing"));
            }
        }

        self.controls.clear();
        let view = SliceView {
            sps,
            pps,
            header: &header,
            raw,
            poc: pic.poc,
            lists: &lists,
            dpb: &self.dpb,
        };
        controls::write(&view, &mut self.controls).map_err(rejected)?;
        let controls: Vec<(u32, &[u8])> = self.controls.iter().collect();

        let stamp_ns = self
            .stamps
            .advance()
            .ok_or_else(|| rejected("this decoder has no stamps left"))?;
        let job = Job {
            setup: Setup {
                coded: sys::PIX_FMT_H264_SLICE,
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

        // The work leaves `self` for the call, so the link can be borrowed with it.
        let work = std::mem::take(&mut self.work);
        let result = self.link.run(&work[..len]);
        self.work = work;
        result?;

        // The job completed: the picture is open with this slice's stamp. The first slice's
        // header carries the picture's marking, and the picture's counts are the first slice's.
        let current = if starts_picture {
            Current {
                header,
                sps: self.spss[sps_at].clone(),
                pic,
                stamp_ns,
            }
        } else {
            let open = self
                .current
                .take()
                .ok_or_else(|| rejected("no open picture"))?;
            Current { stamp_ns, ..open }
        };
        self.current = Some(current);
        Ok(())
    }
}

/// The decoder's descriptor: H.264 bitstream in, NV12 pictures out, on the native backend.
pub fn descriptor() -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Decoder,
        input: FourCc::H264,
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
        Ok(Implementation::Decoder(Box::new(H264Decoder::new(open))))
    })
}
