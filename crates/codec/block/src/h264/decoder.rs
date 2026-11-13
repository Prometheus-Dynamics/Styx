//! The H.264 decoder: a frame's slice in, a picture out, through a block session.
//!
//! Each job is one slice of a frame: the caller puts the slice's Annex B bytes (start code first)
//! at the start of the input lease's buffer. The parameter sets come from
//! [`H264Decoder::set_parameter_sets`]. Styx parses the slice, works out its order counts and
//! reference picture lists, has the [`Layout`] write its controls, builds the job's work
//! ([`crate::h264::work`]) and runs it on a [`Session`] (the block link, [`crate::link`]). After
//! the job completes, the picture is marked in the reference tracker. A job that fails leaves the
//! tracker and the order counts as they were.
//!
//! Recovery is the block link's, as for HEVC. A slice refused by the parser, the tracker or the
//! layout is refused before any job: nothing reaches the block.
//!
//! Provenance: written for Styx from ITU-T H.264 (7.3, 8.2). Lemnos's stateless H.264 block is
//! not at the lock: see [`crate::h264::controls`].

use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Decoder, FourCc, FrameLease,
    Implementation,
};

use crate::bitstream::unescape;
use crate::decoder::Session;
use crate::h264::controls::{Controls, Layout, SliceView};
use crate::h264::dpb::{Dpb, NONE};
use crate::h264::poc::PocState;
use crate::h264::ps::{Pps, Sps, parse_pps, parse_sps, peek_pps_id};
use crate::h264::slice::parse_slice_header;
use crate::h264::work::{self, Job, Setup};
use crate::h264::{NalUnit, nal_units};
use crate::link::Link;

/// The name the decoder registers under.
pub const NAME: &str = "h264-v4l2-stateless";
/// The coded format of a stateless H.264 slice, as its fourcc (the kernel's `S264`). The block
/// checks it; the structures behind it are Lemnos's.
pub const CODED_FORMAT: FourCc = FourCc::new(*b"S264");
/// The length of the Annex B start code in front of a slice.
const START_CODE_LEN: usize = 3;
/// The most reference frames one job names: the DPB's slots.
const MAX_REFERENCES: usize = crate::h264::dpb::DPB_SIZE;

/// The H.264 stateless decoder over one block session.
pub struct H264Decoder {
    descriptor: CodecDescriptor,
    link: Link,
    layout: Box<dyn Layout>,
    spss: Vec<Sps>,
    ppss: Vec<Pps>,
    poc: PocState,
    dpb: Dpb,
    /// Unescaped RBSP scratch. Grows once to the largest unit seen.
    scratch: Vec<u8>,
    controls: Controls,
    /// The work of one job, allocated once.
    work: Box<[u8]>,
}

impl H264Decoder {
    /// A decoder over the sessions `open` opens, writing each slice's controls with `layout`.
    /// Nothing is opened until the first frame.
    pub fn new(
        open: impl FnMut() -> Result<Box<dyn Session>, CodecError> + Send + 'static,
        layout: impl Layout + 'static,
    ) -> Self {
        Self {
            descriptor: descriptor(),
            link: Link::new(Box::new(open)),
            layout: Box::new(layout),
            spss: Vec::new(),
            ppss: Vec::new(),
            poc: PocState::default(),
            dpb: Dpb::new(),
            scratch: Vec::new(),
            controls: Controls::default(),
            work: vec![0; work::WORK_BYTES].into_boxed_slice(),
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

    /// The reference picture tracker, read-only.
    pub fn dpb(&self) -> &Dpb {
        &self.dpb
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
        let stamp_ns = input.timestamp_ns();

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
        let rbsp = &self.scratch[..n];
        let pps_id = peek_pps_id(rbsp).map_err(rejected)?;
        let pps = self
            .ppss
            .iter()
            .find(|p| p.id == pps_id)
            .ok_or_else(|| rejected("the slice's PPS is not known"))?;
        let sps = self
            .spss
            .iter()
            .find(|s| s.id == pps.sps_id)
            .ok_or_else(|| rejected("the slice's SPS is not known"))?;
        let header =
            parse_slice_header(rbsp, unit_type, nal_ref_idc, sps, pps).map_err(rejected)?;
        let (w, h) = sps.frame_size();
        if (w, h) != (width, height) {
            return Err(rejected(format!(
                "the picture is {w}x{h}, not {width}x{height}"
            )));
        }
        self.dpb.check_frame_num(&header, sps).map_err(rejected)?;
        let pic = self.poc.compute(&header, sps).map_err(rejected)?;
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
            poc: pic.poc,
            lists: &lists,
            dpb: &self.dpb,
        };
        self.layout.write(&view, &mut self.controls)?;

        // An IDR picture takes no reference: the frames the tracker holds are cleared by its
        // marking, and the job names none of them.
        let mut references = [(0u8, 0u32); MAX_REFERENCES];
        let mut count = 0;
        if !header.is_idr() {
            for (slot, frame) in self.dpb.frames() {
                references[count] = (slot, frame.buffer);
                count += 1;
            }
        }
        let controls: Vec<(u32, &[u8])> = self.controls.iter().collect();
        let job = Job {
            setup: Setup {
                coded: CODED_FORMAT.to_u32(),
                width,
                height,
                picture: FourCc::NV12.to_u32(),
                planes: 1,
            },
            stamp_ns,
            source,
            source_len: u32::try_from(source_len).map_err(|_| rejected("slice length"))?,
            picture,
            references: &references[..count],
            controls: &controls,
        };
        let len = work::build(&mut self.work, &job)
            .map_err(|_| rejected("the job's work does not fit the block"))?;

        // The work leaves `self` for the call, so the link can be borrowed with it.
        let work = std::mem::take(&mut self.work);
        let result = self.link.run(&work[..len]);
        self.work = work;
        result?;
        self.dpb.mark(&header, sps, picture, pic.stored());
        self.poc.commit(&header, &pic);
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

/// A registry candidate for the decoder. `open` opens a session on the block and `layout` makes
/// the layout of the controls; the candidate builds a fresh decoder over both each time the
/// registry asks.
pub fn candidate<M: 'static>(
    open: impl Fn() -> Result<Box<dyn Session>, CodecError> + Send + Sync + Clone + 'static,
    layout: impl Fn() -> Box<dyn Layout> + Send + Sync + 'static,
) -> Candidate<M> {
    Candidate::new(descriptor(), move || {
        let open = open.clone();
        let layout = layout();
        Ok(Implementation::Decoder(Box::new(H264Decoder {
            descriptor: descriptor(),
            link: Link::new(Box::new(open)),
            layout,
            spss: Vec::new(),
            ppss: Vec::new(),
            poc: PocState::default(),
            dpb: Dpb::new(),
            scratch: Vec::new(),
            controls: Controls::default(),
            work: vec![0; work::WORK_BYTES].into_boxed_slice(),
        })))
    })
}
