//! H.264 on a Rockchip VEPU541-class encoder block (`rk-mpp:rkvenc`, the VEPU540 among them),
//! through `codec.block/1`: NV12 frames in, an IDR access unit out.
//!
//! Lemnos owns the block and its register layout (`lemnos-driver-rkvenc`'s `RegisterSet`, the
//! job builder), and Styx owns the bitstream: the SPS and PPS ([`styx_h264::ParameterSets`]),
//! the values of the slice's syntax elements, and the QP. Each frame is one job: the register
//! values, a reference for each buffer the block reads or writes, and the read-back of the
//! block's status registers, from which the bitstream's length is taken.
//!
//! # No copy
//!
//! The input and output leases are buffers of the block's job stream (the session's pool): the
//! job names them by buffer and offset, and the block reads the picture and writes the bitstream
//! in place. Styx writes only the parameter sets, at the start of the output, before the block's
//! slice. Nothing else is copied: the work is written once into its own buffer, and the read-back
//! is the block's status words.
//!
//! # Not usable until Lemnos states the encoder's facts
//!
//! The layout gives each field's place and width, not what its values mean, and the meanings are
//! Lemnos's to state: register layouts and facts are Lemnos's (`docs/design.md`), and Styx
//! transcribes nothing from vendor code. [`EncoderFacts`] holds what the encoder needs and Lemnos does not
//! state yet ([`MISSING`]). [`vepu540_facts`] has none of them, so [`RkvencH264::new`] cannot be
//! given real facts and the encoder does not run on a board. The tests fill the table with
//! **test values** to run the job path on rk-mpp's played kernel. A test value is not a fact.
//!
//! What the gate proves: the job's shape (every register the encoder sets, every buffer reference
//! on the input and output leases, the read-back window), that nothing is copied, and the
//! parameter set bytes. Not the encoded picture: the played kernel runs no encoder.
//!
//! Hardware status: untested on board. Not usable until Lemnos supplies the VEPU540 encodings.

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Builder;
use lemnos_driver_rkvenc::layout::{AddressReg, Field};
use lemnos_driver_rkvenc::vepu541_h264::{READ_BACK, RegisterSet, SetError, ret, set};
use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Encoder, Implementation,
};
use styx_format::{FourCc, MediaFormat};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};
use styx_h264::{ParameterSets, SHAPE};

use crate::decoder::Session;
use crate::link::Link;

/// The name the encoder registers under.
pub const NAME: &str = "h264-rkvenc";

/// The block name `codec.block/1` gives the encoder (`rk-mpp`'s `codec` module).
pub const BLOCK: &str = "rk-mpp:rkvenc";

/// The rk-mpp block's work item that carries register values: the first register, then the
/// values (`lemnos-driver-rk-mpp`'s `codec::TAG_WRITE`; the tests check it is the same).
pub const TAG_WRITE: u32 = 1;
/// The rk-mpp block's work item that names the registers read back: the first and how many
/// (`codec::TAG_READ`).
pub const TAG_READ: u32 = 2;

/// The facts Lemnos is asked for, one line each: what the encoder needs and the layout does not
/// say. Each is a field of [`EncoderFacts`].
pub const MISSING: [&str; 6] = [
    "SRC_CFMT: the value for NV12",
    "ENC_STND: the value for H.264",
    "BSBT/BSBB/BSBR/ADR_BSBS: which is the bitstream's base, top, read pointer and write start, \
     and whether the block writes the slice header from reg103-110",
    "RFPW_H/RFPW_B/DSPW/CMVW/MEIW: which an intra-only job needs, and their sizes for a WxH NV12 \
     picture",
    "PIC_WFILL/PIC_HFILL: their value for a picture size",
    "the fields with no H.264 meaning an intra frame needs set (start command, interrupt enables, \
     RDO, motion search, thresholds, weights), with their values",
];

/// A buffer the block writes for its own use: the register that names it, and its size for a
/// `width` x `height` picture.
#[derive(Clone, Copy, Debug)]
pub struct Scratch {
    pub register: AddressReg,
    pub bytes: fn(width: u32, height: u32) -> u32,
}

/// The encoder's facts about the block: what Lemnos is asked to state ([`MISSING`]).
#[derive(Clone, Copy, Debug)]
pub struct EncoderFacts {
    /// `SRC_CFMT` for NV12.
    pub source_nv12: i64,
    /// `ENC_STND` for H.264.
    pub standard_h264: i64,
    /// The register that holds the bitstream buffer's first byte.
    pub bitstream_base: AddressReg,
    /// The register that holds the end of the room the block may write to.
    pub bitstream_top: AddressReg,
    /// The register that holds the read pointer.
    pub bitstream_read: AddressReg,
    /// The register that holds where the block starts writing.
    pub bitstream_start: AddressReg,
    /// Whether the block writes the slice header itself, from the syntax registers.
    pub writes_slice_header: bool,
    /// `PIC_WFILL` for a width, and `PIC_HFILL` for a height.
    pub fill: fn(u32) -> i64,
    /// The buffers the block writes for itself, carved from one scratch buffer in this order.
    pub scratch: &'static [Scratch],
    /// The other fields the job sets, with their values.
    pub start: &'static [(Field, i64)],
}

/// The VEPU540's facts, as Lemnos states them. Lemnos states none yet, so this is always the
/// error naming them.
///
/// # Errors
///
/// [`CodecError::Unavailable`], naming every fact of [`MISSING`].
pub fn vepu540_facts() -> Result<EncoderFacts, CodecError> {
    Err(CodecError::Unavailable(format!(
        "{NAME}: not usable until Lemnos supplies the VEPU540 encodings: {}",
        MISSING.join("; ")
    )))
}

/// How many bytes of work one job takes at most: the header, 64 references, three items and the
/// two register windows.
const WORK_BYTES: usize = 4096;

/// The read-back's bytes: every register from `READ_BACK.first`, four bytes each.
const READ_BACK_BYTES: usize = 4 * READ_BACK.count as usize;

/// The alignment the bitstream buffer's base needs ([`EncoderFacts::bitstream_base`]'s own,
/// checked again at each frame).
fn align_up(value: u32, align: u32) -> Option<u32> {
    value.checked_next_multiple_of(align.max(1))
}

fn rejected(what: impl std::fmt::Display) -> CodecError {
    CodecError::Rejected(format!("{NAME}: {what}"))
}

fn set_error(what: &str, error: SetError) -> CodecError {
    rejected(format!("{what}: {error:?}"))
}

/// The H.264 encoder on one rkvenc block session.
pub struct RkvencH264 {
    descriptor: CodecDescriptor,
    link: Link,
    facts: EncoderFacts,
    qp: u8,
    /// The job stream buffer the block's own writes go to, and its length. The caller holds a
    /// lease on it for as long as the encoder runs.
    scratch: (u32, u32),
    sets: ParameterSets,
    regs: RegisterSet,
    work: Vec<u8>,
    item: Vec<u8>,
    read_back: Vec<u8>,
    idr_pic_id: u32,
}

impl std::fmt::Debug for RkvencH264 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RkvencH264")
            .field("qp", &self.qp)
            .field("scratch", &self.scratch)
            .finish_non_exhaustive()
    }
}

impl RkvencH264 {
    /// An encoder on the session `open` makes, with `facts` about the block, at a fixed `qp`.
    /// `scratch` is a job stream buffer (number and length) the caller holds a lease on: the
    /// block's own writes ([`EncoderFacts::scratch`]) are carved from it.
    ///
    /// Everything a frame needs is allocated here, once.
    ///
    /// # Errors
    ///
    /// [`CodecError::Rejected`] for a QP above 51, or facts that do not let the encoder write a
    /// stream (a block that does not write the slice header).
    pub fn new(
        open: impl FnMut() -> Result<Box<dyn Session>, CodecError> + Send + 'static,
        facts: EncoderFacts,
        scratch: (u32, u32),
        qp: u8,
    ) -> Result<Self, CodecError> {
        if qp > 51 {
            return Err(rejected(format!("QP {qp} is above 51")));
        }
        if !facts.writes_slice_header {
            return Err(rejected(
                "the block does not write the slice header, and this encoder writes none",
            ));
        }
        Ok(Self {
            descriptor: descriptor(),
            link: Link::new(Box::new(open)),
            facts,
            qp,
            scratch,
            sets: ParameterSets::new(),
            regs: RegisterSet::new(),
            work: vec![0; WORK_BYTES],
            item: Vec::with_capacity(4 * (1 + lemnos_driver_rkvenc::vepu541_h264::L2_WORDS)),
            read_back: vec![0; READ_BACK_BYTES],
            idr_pic_id: 0,
        })
    }

    /// Whether the block session is open.
    pub fn is_open(&self) -> bool {
        self.link.is_open()
    }

    /// The register values of the last job (for the tests and for a board run's record).
    pub fn registers(&self) -> &RegisterSet {
        &self.regs
    }

    fn set(&mut self, field: Field, value: i64, what: &str) -> Result<(), CodecError> {
        self.regs.set(field, value).map_err(|e| set_error(what, e))
    }

    fn address(
        &mut self,
        reg: AddressReg,
        buffer: u32,
        offset: u32,
        what: &str,
    ) -> Result<(), CodecError> {
        self.regs
            .set_stream_address(reg, buffer, offset)
            .map_err(|e| set_error(what, e))
    }

    /// The register values of one IDR picture of `width` x `height`, its source planes and its
    /// bitstream room.
    fn fill(
        &mut self,
        (width, height): (u32, u32),
        source: (u32, PlaneLayout, PlaneLayout),
        bitstream: (u32, u32, u32, u32),
    ) -> Result<(), CodecError> {
        let facts = self.facts;
        self.regs.clear();
        // The picture: its size in 8-pixel units less one, and its fill.
        self.set(set::reg013::ENC_STND, facts.standard_h264, "ENC_STND")?;
        self.set(
            set::reg012::PIC_WD8_M1,
            i64::from(width.div_ceil(8) - 1),
            "PIC_WD8_M1",
        )?;
        self.set(
            set::reg012::PIC_HD8_M1,
            i64::from(height.div_ceil(8) - 1),
            "PIC_HD8_M1",
        )?;
        self.set(set::reg012::PIC_WFILL, (facts.fill)(width), "PIC_WFILL")?;
        self.set(set::reg012::PIC_HFILL, (facts.fill)(height), "PIC_HFILL")?;
        self.set(set::reg013::PIC_QP, i64::from(self.qp), "PIC_QP")?;

        // The source: NV12, its two planes' strides and places in the input buffer.
        let (input, luma, chroma) = source;
        self.set(set::reg017::SRC_CFMT, facts.source_nv12, "SRC_CFMT")?;
        self.set(set::reg069::SRC_STRD0, luma.stride as i64, "SRC_STRD0")?;
        self.set(set::reg069::SRC_STRD1, chroma.stride as i64, "SRC_STRD1")?;
        let offset = |p: PlaneLayout| u32::try_from(p.offset).map_err(|_| rejected("plane offset"));
        self.address(set::reg070::ADR_SRC0, input, offset(luma)?, "ADR_SRC0")?;
        self.address(set::reg071::ADR_SRC1, input, offset(chroma)?, "ADR_SRC1")?;

        // The slice's syntax elements, as Styx's parameter sets have them: an IDR, all I, CAVLC,
        // the frame number 0, the deblocking filter on.
        self.set(set::reg103::NAL_REF_IDC, 3, "NAL_REF_IDC")?;
        self.set(set::reg103::NAL_UNIT_TYPE, 5, "NAL_UNIT_TYPE")?;
        self.set(
            set::reg104::MAX_FNUM,
            i64::from(SHAPE.log2_max_frame_num - 4),
            "MAX_FNUM",
        )?;
        self.set(set::reg105::ETPY_MODE, i64::from(SHAPE.cabac), "ETPY_MODE")?;
        self.set(
            set::reg105::CSIP_FLAG,
            i64::from(SHAPE.constrained_intra_pred),
            "CSIP_FLAG",
        )?;
        self.set(
            set::reg105::NUM_REF0_IDX,
            i64::from(SHAPE.num_ref_idx_l0_default - 1),
            "NUM_REF0_IDX",
        )?;
        self.set(
            set::reg105::PIC_INIT_QP,
            i64::from(SHAPE.pic_init_qp),
            "PIC_INIT_QP",
        )?;
        self.set(
            set::reg105::DBF_CP_FLG,
            i64::from(SHAPE.deblocking_filter_control_present),
            "DBF_CP_FLG",
        )?;
        // slice_type 2: I.
        self.set(set::reg106::SLI_TYPE, 2, "SLI_TYPE")?;
        self.set(set::reg106::PPS_ID, i64::from(SHAPE.ids), "PPS_ID")?;
        self.set(set::reg106::FRM_NUM, 0, "FRM_NUM")?;
        self.set(
            set::reg107::IDR_PIC_ID,
            i64::from(self.idr_pic_id),
            "IDR_PIC_ID",
        )?;
        self.set(set::reg108::DIS_DBLK_IDC, 0, "DIS_DBLK_IDC")?;

        // The bitstream: the room in the output buffer, the start after the parameter sets.
        let (output, base, top, start) = bitstream;
        self.address(facts.bitstream_base, output, base, "the bitstream base")?;
        self.address(facts.bitstream_top, output, top, "the bitstream top")?;
        self.address(
            facts.bitstream_read,
            output,
            base,
            "the bitstream read pointer",
        )?;
        self.address(facts.bitstream_start, output, start, "the bitstream start")?;

        // The block's own writes, one after another in the scratch buffer at their alignments.
        let (scratch, room) = self.scratch;
        let mut at = 0u32;
        for target in facts.scratch {
            at = align_up(at, target.register.align).ok_or_else(|| rejected("scratch offset"))?;
            let bytes = (target.bytes)(width, height);
            let end = at
                .checked_add(bytes)
                .filter(|&end| end <= room)
                .ok_or_else(|| {
                    rejected(format!(
                        "the scratch buffer holds {room} bytes, the block's writes need more"
                    ))
                })?;
            self.address(target.register, scratch, at, "a scratch write")?;
            at = end;
        }

        for &(field, value) in facts.start {
            self.set(field, value, "a start field")?;
        }
        Ok(())
    }

    /// The job's work in `codec.block/1`'s form, into `self.work`: a reference per address
    /// register, then the write side, the second window when set, and the read-back. Returns its
    /// length.
    fn build_work(&mut self) -> Result<usize, CodecError> {
        let job = self.regs.codec_job().map_err(|e| set_error("the job", e))?;
        let full = || rejected("the job's work does not fit its buffer");
        let mut builder = Builder::new(&mut self.work).ok_or_else(full)?;
        for r in job.refs {
            builder
                .reference(BufferRef {
                    slot: u32::from(r.register),
                    buffer: r.buffer,
                    offset: r.offset,
                    length: 0,
                })
                .ok_or_else(full)?;
        }
        self.item.clear();
        self.item.extend_from_slice(&0u32.to_le_bytes());
        for w in job.write {
            self.item.extend_from_slice(&w.to_le_bytes());
        }
        builder.item(TAG_WRITE, &self.item).ok_or_else(full)?;
        if let Some(run) = job.l2 {
            self.item.clear();
            self.item
                .extend_from_slice(&u32::from(run.first).to_le_bytes());
            for w in run.registers {
                self.item.extend_from_slice(&w.to_le_bytes());
            }
            builder.item(TAG_WRITE, &self.item).ok_or_else(full)?;
        }
        let mut read = [0u8; 8];
        read[..4].copy_from_slice(&u32::from(job.read.first).to_le_bytes());
        read[4..].copy_from_slice(&u32::from(job.read.count).to_le_bytes());
        builder.item(TAG_READ, &read).ok_or_else(full)?;
        Ok(builder.finish())
    }

    /// The bitstream's length from the read-back: `BS_LGTH` of the status registers.
    fn bitstream_len(&self, count: u32) -> Result<u32, CodecError> {
        let field = ret::st_bsl::BS_LGTH;
        let word = usize::from(field.word)
            .checked_sub(usize::from(READ_BACK.first))
            .ok_or_else(|| rejected("BS_LGTH is before the read-back window"))?;
        let bytes = self
            .read_back
            .get(4 * word..4 * word + 4)
            .filter(|_| 4 * word + 4 <= count as usize)
            .ok_or_else(|| {
                CodecError::Failed(format!(
                    "{NAME}: the read-back ({count} bytes) has no BS_LGTH"
                ))
            })?;
        let value = u32::from_le_bytes(bytes.try_into().expect("four bytes"));
        let mask = if field.width >= 32 {
            u32::MAX
        } else {
            (1u32 << field.width) - 1
        };
        Ok((value >> field.lsb) & mask)
    }
}

impl<M> Codec<M> for RkvencH264 {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Encoder<M> for RkvencH264 {
    /// One IDR access unit: the SPS and PPS Styx writes at the start of the output's plane, then
    /// the block's slice. The output's plane is the bitstream's room; its offset in the buffer
    /// must take the base register's alignment.
    fn encode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let format = *input.format();
        if format.code != FourCc::NV12 {
            return Err(rejected(format!("encodes NV12, not {}", format.code)));
        }
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let planes = &input.entry().layouts;
        let (Some(&luma), Some(&chroma)) = (planes.first(), planes.get(1)) else {
            return Err(rejected("an NV12 input has two planes"));
        };
        let input_buffer =
            u32::try_from(input.buffer_index()).map_err(|_| rejected("input buffer number"))?;
        let output_buffer =
            u32::try_from(output.buffer_index()).map_err(|_| rejected("output buffer number"))?;
        let room = *output
            .entry()
            .layouts
            .first()
            .ok_or_else(|| rejected("the output has no plane"))?;
        let base = u32::try_from(room.offset).map_err(|_| rejected("output offset"))?;
        let base_align = self.facts.bitstream_base.align.max(1);
        if !base.is_multiple_of(base_align) {
            return Err(rejected(format!(
                "the output plane starts at {base}, not on {base_align} bytes"
            )));
        }

        // The parameter sets, written once into the output's own bytes.
        let sets = self.sets.write(width as usize, height as usize)?;
        let header = sets.len();
        {
            let mut write = output
                .write()
                .ok_or_else(|| rejected("the output is shared or not writable"))?;
            let plane = write
                .plane(0)
                .ok_or_else(|| rejected("the output has no plane"))?;
            plane
                .data
                .get_mut(..header)
                .ok_or_else(|| rejected("the output is smaller than the parameter sets"))?
                .copy_from_slice(sets);
        }
        let room_len = u32::try_from(room.len).map_err(|_| rejected("output length"))?;
        let top_align = self.facts.bitstream_top.align.max(1);
        let top = base + room_len / top_align * top_align;
        let start = base + header as u32;
        if start >= top {
            return Err(rejected("the output has no room after the parameter sets"));
        }

        self.fill(
            (width, height),
            (input_buffer, luma, chroma),
            (output_buffer, base, top, start),
        )?;
        let len = self.build_work()?;
        let work = std::mem::take(&mut self.work);
        let mut read_back = std::mem::take(&mut self.read_back);
        let ran = self.link.run_reading(&work[..len], &mut read_back);
        self.work = work;
        self.read_back = read_back;
        let count = ran?;
        let slice = self.bitstream_len(count)?;
        let total = header + slice as usize;
        if total > room.len {
            return Err(CodecError::Failed(format!(
                "{NAME}: the block wrote {slice} bytes past {header} into {} of room",
                room.len
            )));
        }
        self.idr_pic_id ^= 1;
        let layouts = Layouts::new(&[PlaneLayout::new(room.offset, total, total)])
            .ok_or_else(|| CodecError::Failed("one plane does not fit a layout".into()))?;
        let entry = FrameEntry::new(
            input.id(),
            input.timestamp_ns(),
            MediaFormat::new(FourCc::H264, format.resolution, format.color, format.range),
            layouts,
        );
        output
            .set_entry(entry)
            .map_err(|error| rejected(error.to_string()))
    }
}

/// The encoder's descriptor: NV12 pictures in, H.264 out, on the native backend.
pub fn descriptor() -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Encoder,
        input: FourCc::NV12,
        output: FourCc::H264,
        backend: Backend::Native,
        name: NAME,
    }
}

/// The encoder as a registry candidate. It is built with [`vepu540_facts`], so until Lemnos
/// states them the candidate does not build, and the registry's decision says why.
pub fn candidate<M: 'static>(
    open: impl Fn() -> Result<Box<dyn Session>, CodecError> + Send + Sync + Clone + 'static,
    scratch: (u32, u32),
    qp: u8,
) -> Candidate<M> {
    Candidate::new(descriptor(), move || {
        let facts = vepu540_facts()?;
        Ok(Implementation::Encoder(Box::new(RkvencH264::new(
            open.clone(),
            facts,
            scratch,
            qp,
        )?)))
    })
}
