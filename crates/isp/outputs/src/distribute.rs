//! The frames of a plan: one ISP pass per configured output, one conversion per fallback, and a
//! share of the output's lease for each consumer that takes it.
//!
//! Each configured output has its own [`FramePool`], made once in [`Distributor::new`]. A frame
//! takes a buffer from each pool it needs, has the producer write into it, and hands every
//! consumer a share of that lease: no copy, one atomic count. A consumer routed to a pool whose
//! buffers are all held by consumers still holding earlier frames gets nothing for that frame
//! (the frame is dropped for it, and counted); the producer never waits.

use std::fmt;

use styx_codec_api::{CodecError, Converter};
use styx_format::{LayoutError, MediaFormat};
use styx_frame::{
    BufferSize, FrameEntry, FrameLease, FramePool, Layouts, PoolError, packed_layouts,
};
use styx_scale::{Path, Software};

use crate::plan::{Plan, Route};

/// What [`Distributor::new`] could not make.
#[derive(Debug)]
pub enum BuildError {
    /// An output's planes do not lay out.
    Layout(LayoutError),
    /// An output's pool could not be made.
    Pool(PoolError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(e) => write!(f, "output planes: {e}"),
            Self::Pool(e) => write!(f, "output pool: {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// What a frame could not get through: the producer's error, or the converter's.
#[derive(Debug)]
pub enum DeliverError<E> {
    /// The producer (the ISP pass) failed.
    Produce(E),
    /// The conversion failed.
    Convert(CodecError),
}

impl<E: fmt::Display> fmt::Display for DeliverError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Produce(e) => write!(f, "producing an output: {e}"),
            Self::Convert(e) => write!(f, "converting an output: {e}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for DeliverError<E> {}

/// How many consumers were served a frame, and how many were not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delivery {
    pub delivered: usize,
    pub dropped: usize,
}

/// One configured output's buffers.
struct Target<M> {
    format: MediaFormat,
    layouts: Layouts,
    pool: FramePool<M>,
}

impl<M: Default> Target<M> {
    fn new(format: MediaFormat, depth: usize) -> Result<Self, BuildError> {
        let layouts = packed_layouts(
            format.code,
            format.resolution.width.get(),
            format.resolution.height.get(),
            None,
            None,
        )
        .map_err(BuildError::Layout)?;
        let span = layouts
            .span()
            .ok_or(BuildError::Layout(LayoutError::Overflow))?;
        let pool = FramePool::host(depth, BufferSize::new(span, 0)).map_err(BuildError::Pool)?;
        Ok(Self {
            format,
            layouts,
            pool,
        })
    }
}

impl<M> Target<M> {
    fn acquire(&self, id: u64, timestamp_ns: u64) -> Option<FrameLease<M>> {
        self.pool
            .acquire(FrameEntry::new(id, timestamp_ns, self.format, self.layouts))
            .ok()
    }
}

/// A conversion's buffers and its converter.
struct Fallback<M> {
    from: usize,
    target: Target<M>,
    converter: Software,
}

/// Serves a plan's consumers frame by frame. Build it once per capture geometry.
pub struct Distributor<M> {
    plan: Plan,
    /// Per plan output: its buffers, `None` for an output no consumer takes.
    outputs: Vec<Option<Target<M>>>,
    conversions: Vec<Fallback<M>>,
    /// This frame's output leases and conversion leases, held until every consumer has a share.
    isp_leases: Vec<Option<FrameLease<M>>>,
    conversion_leases: Vec<Option<FrameLease<M>>>,
    /// Per consumer: this frame's lease, taken with [`Distributor::take`].
    delivered: Vec<Option<FrameLease<M>>>,
}

impl<M: Default> Distributor<M> {
    /// Makes the pools of `plan`'s outputs and conversions, `depth` buffers each. Every
    /// allocation of the distributor is made here.
    pub fn new(plan: Plan, depth: usize) -> Result<Self, BuildError> {
        let outputs = plan
            .outputs()
            .iter()
            .map(|output| {
                output
                    .used
                    .then(|| Target::new(output.format, depth))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let conversions = plan
            .conversions()
            .iter()
            .map(|conversion| {
                let input = plan.outputs()[conversion.from].format.code;
                Ok(Fallback {
                    from: conversion.from,
                    target: Target::new(conversion.format, depth)?,
                    converter: Software::new(input, conversion.format.code, Path::Fast),
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;
        let consumers = plan.routes().len();
        Ok(Self {
            isp_leases: (0..outputs.len()).map(|_| None).collect(),
            conversion_leases: (0..conversions.len()).map(|_| None).collect(),
            delivered: (0..consumers).map(|_| None).collect(),
            outputs,
            conversions,
            plan,
        })
    }
}

impl<M> Distributor<M> {
    /// The plan this distributor serves.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Serves one frame. `produce(output, input, out)` writes ISP output `output` of `input`
    /// into `out` (a lease the distributor acquired, sized and formatted for that output); it is
    /// called once per used output, in index order. Conversions then run from the outputs they
    /// name. Every consumer's share is then ready to [`take`](Self::take).
    ///
    /// A consumer whose output's buffers are all held gets nothing for this frame. Earlier
    /// frames' leases are released first, so a consumer that takes and drops each frame keeps
    /// every buffer free.
    pub fn deliver<E>(
        &mut self,
        input: &FrameLease<M>,
        mut produce: impl FnMut(usize, &FrameLease<M>, &mut FrameLease<M>) -> Result<(), E>,
    ) -> Result<Delivery, DeliverError<E>> {
        for slot in self
            .delivered
            .iter_mut()
            .chain(self.isp_leases.iter_mut())
            .chain(self.conversion_leases.iter_mut())
        {
            *slot = None;
        }
        let (id, timestamp) = (input.id(), input.timestamp_ns());

        for (output, target) in self.outputs.iter().enumerate() {
            let Some(target) = target else { continue };
            if let Some(mut lease) = target.acquire(id, timestamp) {
                produce(output, input, &mut lease).map_err(DeliverError::Produce)?;
                self.isp_leases[output] = Some(lease);
            }
        }

        for (index, fallback) in self.conversions.iter_mut().enumerate() {
            let Some(source) = self.isp_leases[fallback.from].as_ref() else {
                continue;
            };
            if let Some(mut lease) = fallback.target.acquire(id, timestamp) {
                Converter::<M>::convert(&mut fallback.converter, source, &mut lease)
                    .map_err(DeliverError::Convert)?;
                self.conversion_leases[index] = Some(lease);
            }
        }

        let mut delivery = Delivery::default();
        for (consumer, route) in self.plan.routes().iter().enumerate() {
            let lease = match *route {
                Route::Isp { output, .. } => self.isp_leases[output].as_ref(),
                Route::Converted { conversion, .. } => self.conversion_leases[conversion].as_ref(),
            };
            self.delivered[consumer] = lease.map(FrameLease::share);
            if self.delivered[consumer].is_some() {
                delivery.delivered += 1;
            } else {
                delivery.dropped += 1;
            }
        }

        // The consumers hold shares now; the producing leases go back to their pools.
        for slot in self
            .isp_leases
            .iter_mut()
            .chain(self.conversion_leases.iter_mut())
        {
            *slot = None;
        }
        Ok(delivery)
    }

    /// Consumer `consumer`'s lease for the last frame delivered, once: the share of the output
    /// it is routed to. `None` when the frame was dropped for it, or it was already taken.
    pub fn take(&mut self, consumer: usize) -> Option<FrameLease<M>> {
        self.delivered.get_mut(consumer)?.take()
    }
}

impl<M> fmt::Debug for Distributor<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Distributor")
            .field("plan", &self.plan)
            .field("consumers", &self.delivered.len())
            .finish_non_exhaustive()
    }
}
