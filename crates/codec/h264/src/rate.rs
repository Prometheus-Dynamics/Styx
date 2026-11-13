//! Picture-level rate control: a QP per picture from a bitrate target.
//!
//! Each picture type keeps a complexity, bits × qstep of its last picture (bits fall about in
//! proportion to the quantiser step, which doubles every 6 QP). The next picture's QP is the
//! one whose step brings that complexity to its share of the budget: an even share per
//! picture, I pictures [`I_WEIGHT`] of them, less a part of what the stream is over (or plus
//! what it is under) so far. QP moves at most [`MAX_STEP`] a picture of a type.

/// A bitrate to hold: `bits_per_second` at `fps` pictures a second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateTarget {
    pub bits_per_second: u32,
    pub fps: f64,
}

/// The shares of the budget an I picture takes.
const I_WEIGHT: f64 = 3.0;
/// The excess or shortfall corrected per picture, as a fraction of it.
const CATCH_UP: f64 = 1.0 / 8.0;
/// The most QP changes between two pictures of a type.
const MAX_STEP: i32 = 4;
/// The QP range rate control picks from.
const QP_RANGE: (i32, i32) = (10, 51);

/// H.264's quantiser step relative to QP 0's (doubling every 6).
fn qstep(qp: i32) -> f64 {
    2f64.powf(f64::from(qp) / 6.0)
}

#[derive(Clone, Copy, Debug, Default)]
struct Kind {
    /// Bits × qstep of the last picture of this type, and its QP.
    last: Option<(f64, i32)>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RateControl {
    target: RateTarget,
    /// Bits sent beyond the target so far (negative: under it).
    excess: f64,
    intra: Kind,
    inter: Kind,
}

impl RateControl {
    pub(super) fn new(target: RateTarget) -> Self {
        Self {
            target,
            excess: 0.0,
            intra: Kind::default(),
            inter: Kind::default(),
        }
    }

    pub(super) fn target(&self) -> RateTarget {
        self.target
    }

    fn per_picture(&self) -> f64 {
        f64::from(self.target.bits_per_second) / self.target.fps.max(1e-3)
    }

    /// The QP for the next picture (`idr`: an I picture), `fallback` before any picture of
    /// the type has been coded.
    pub(super) fn qp(&self, idr: bool, fallback: u8) -> u8 {
        let (kind, other) = if idr {
            (&self.intra, &self.inter)
        } else {
            (&self.inter, &self.intra)
        };
        let weight = if idr { I_WEIGHT } else { 1.0 };
        let budget = (weight * self.per_picture() - CATCH_UP * self.excess)
            .max(weight * self.per_picture() / 8.0);
        let qp = match kind.last {
            Some((complexity, last)) => {
                let qp = (6.0 * (complexity / budget).log2()).round() as i32;
                qp.clamp(last - MAX_STEP, last + MAX_STEP)
            }
            // The other type's QP is a better start than the fixed one.
            None => other.last.map_or(i32::from(fallback), |(_, qp)| qp),
        };
        qp.clamp(QP_RANGE.0, QP_RANGE.1) as u8
    }

    /// Notes a coded picture: its type, QP and size in bits.
    pub(super) fn coded(&mut self, idr: bool, qp: u8, bits: usize) {
        let bits = bits as f64;
        self.excess += bits - self.per_picture();
        let kind = if idr {
            &mut self.intra
        } else {
            &mut self.inter
        };
        kind.last = Some((bits.max(1.0) * qstep(i32::from(qp)), i32::from(qp)));
    }
}
