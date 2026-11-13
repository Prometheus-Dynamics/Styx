//! The focus lens as a Lemnos device: the `lens/1` contract (its range, a move, and the last
//! position moved to), and the map from its codes to the normalised Focus control.

use lemnos_client::RemoteCaller;
use lemnos_contract_lens::{FocusDirection, Lens, Range};
use lemnos_core::{Bound, Status};
use styx_aaa::Pwl;
use styx_lemnos::Client;

/// The contract's tag, to find a lens on the board.
pub const LENS_TAG: &str = "lens/1";

/// Which way a lens's codes run, from the nearest object to the farthest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Nearer objects are at higher codes (the AF map's default).
    Rising,
    /// Nearer objects are at lower codes.
    Falling,
}

impl Direction {
    /// The direction of an AF map (dioptres to lens code): the way its codes run between the
    /// map's ends. The `lens/1` contract carries positions only, so the direction comes from
    /// the map the tuning or the board gives.
    pub fn of_map(map: &Pwl) -> Self {
        // The map's domain is in dioptres, so its high end is the near end.
        let (low, high) = map.domain();
        if map.eval(high) >= map.eval(low) {
            Self::Rising
        } else {
            Self::Falling
        }
    }
}

/// The normalised Focus control: 0 at the nearest end of the lens's range, 1000 at the farthest
/// (`styx_controls::Control::Focus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusScale {
    min: u16,
    max: u16,
    direction: Direction,
}

impl FocusScale {
    /// A scale over `range`. An empty range is refused.
    pub fn new(range: Range, direction: Direction) -> Result<Self, Error> {
        if range.min >= range.max {
            return Err(Error::EmptyRange(range));
        }
        Ok(Self {
            min: range.min,
            max: range.max,
            direction,
        })
    }

    /// The Focus value of lens code `position`, clamped to 0..=1000.
    pub fn value(&self, position: f64) -> u32 {
        let span = f64::from(self.max - self.min);
        let from_near = match self.direction {
            Direction::Rising => f64::from(self.max) - position,
            Direction::Falling => position - f64::from(self.min),
        };
        let v = (from_near / span * 1000.0).round();
        if v.is_nan() {
            0
        } else {
            v.clamp(0.0, 1000.0) as u32
        }
    }
}

/// Why a lens could not be driven.
#[derive(Debug)]
pub enum Error {
    /// No device carries `lens/1`, or the named device is not a lens.
    NoLens(String),
    /// The Lemnos client failed.
    Client(lemnos_client::Error),
    /// A contract call failed with this status.
    Call(Status),
    /// The lens reports an empty range.
    EmptyRange(Range),
    /// AF refused its tuning or its configuration.
    Algorithm(styx_aaa::AlgoError),
}

impl From<styx_aaa::AlgoError> for Error {
    fn from(e: styx_aaa::AlgoError) -> Self {
        Self::Algorithm(e)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLens(name) => write!(f, "{name}: no {LENS_TAG} device by that name"),
            Self::Client(e) => write!(f, "lemnos client: {e}"),
            Self::Call(s) => write!(f, "lens call failed: {s:?}"),
            Self::EmptyRange(r) => write!(f, "lens range {}..={} is empty", r.min, r.max),
            Self::Algorithm(e) => write!(f, "autofocus: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// One lens, opened through the client. Positions are the lens's own codes.
pub struct LensDevice {
    lens: Bound<dyn Lens, RemoteCaller>,
    range: Range,
}

impl core::fmt::Debug for LensDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LensDevice")
            .field("range", &self.range)
            .finish_non_exhaustive()
    }
}

impl LensDevice {
    /// Opens the lens device `name` (it must carry `lens/1`) and reads its range.
    pub fn open(client: &Client, name: &str) -> Result<Self, Error> {
        let devices = client.list().map_err(Error::Client)?;
        if !devices
            .iter()
            .any(|d| d.name == name && d.tags.iter().any(|t| t == LENS_TAG))
        {
            return Err(Error::NoLens(name.to_string()));
        }
        let mut lens = client.open::<dyn Lens>(name).map_err(Error::Client)?;
        let range = lens.range().map_err(Error::Call)?;
        if range.min >= range.max {
            return Err(Error::EmptyRange(range));
        }
        Ok(Self { lens, range })
    }

    /// The lens's range, as it reported at open.
    pub fn range(&self) -> Range {
        self.range
    }

    /// Asks the lens to move to `position`. `Ok(true)` when the move was written; `Ok(false)`
    /// when the chip is powering up first (the lens answers `AGAIN`: the same move, asked for
    /// again, is written). Other statuses are errors.
    pub fn set_position(&mut self, position: u16) -> Result<bool, Status> {
        match self.lens.set_position(position) {
            Ok(()) => Ok(true),
            Err(Status::AGAIN) => Ok(false),
            Err(status) => Err(status),
        }
    }

    /// The last position moved to, or `None` before the first move (the lens reports
    /// `NOT_SUPPORTED` then).
    pub fn position(&mut self) -> Result<Option<u16>, Status> {
        match self.lens.position() {
            Ok(p) => Ok(Some(p)),
            Err(Status::NOT_SUPPORTED) => Ok(None),
            Err(status) => Err(status),
        }
    }

    /// Which way focus moves as the position goes up, as the lens answers it. `NOT_SUPPORTED` is
    /// read as [`FocusDirection::Unknown`] by the caller; other statuses are errors.
    pub fn direction(&mut self) -> Result<FocusDirection, Status> {
        self.lens.direction()
    }

    /// Microseconds the lens takes to settle after a move of `distance` codes, or `None` when the
    /// lens and the board have no figure for it (or the lens cannot say: the caller falls back to
    /// its configured settle time).
    pub fn settle_us(&mut self, distance: u16) -> Option<u32> {
        self.lens.settle_us(distance).ok().flatten()
    }
}

/// The direction a focus scale runs in: the lens's own answer, or `from_map` (the AF map's, from
/// the tuning or the board) when the lens says `Unknown`. The lens's answers are about the codes:
/// `TowardNear` is nearer focus at higher codes, which is the rising direction.
pub fn resolve_direction(answer: FocusDirection, from_map: Direction) -> Direction {
    match answer {
        FocusDirection::TowardNear => Direction::Rising,
        FocusDirection::TowardFar => Direction::Falling,
        FocusDirection::Unknown => from_map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: u16, max: u16) -> Range {
        Range { min, max }
    }

    #[test]
    fn the_direction_follows_the_map() {
        // The default AF map: 0 D at the lowest code, 12 D (nearest) at the highest.
        let rising = Pwl::new(vec![(0.0, 0.0), (12.0, 1023.0)]).unwrap();
        assert_eq!(Direction::of_map(&rising), Direction::Rising);
        let falling = Pwl::new(vec![(0.0, 1023.0), (12.0, 0.0)]).unwrap();
        assert_eq!(Direction::of_map(&falling), Direction::Falling);
    }

    #[test]
    fn focus_is_0_at_the_near_end_and_1000_at_the_far() {
        let rising = FocusScale::new(range(0, 1023), Direction::Rising).unwrap();
        assert_eq!(rising.value(1023.0), 0);
        assert_eq!(rising.value(0.0), 1000);
        assert_eq!(rising.value(426.0), 584);
        let falling = FocusScale::new(range(0, 1023), Direction::Falling).unwrap();
        assert_eq!(falling.value(0.0), 0);
        assert_eq!(falling.value(1023.0), 1000);
        // Off the range, clamped.
        assert_eq!(rising.value(2000.0), 0);
        assert_eq!(rising.value(-5.0), 1000);
    }

    #[test]
    fn the_lens_answer_wins_and_unknown_keeps_the_map() {
        use lemnos_contract_lens::FocusDirection;
        assert_eq!(
            resolve_direction(FocusDirection::TowardNear, Direction::Falling),
            Direction::Rising
        );
        assert_eq!(
            resolve_direction(FocusDirection::TowardFar, Direction::Rising),
            Direction::Falling
        );
        assert_eq!(
            resolve_direction(FocusDirection::Unknown, Direction::Falling),
            Direction::Falling
        );
    }

    #[test]
    fn an_empty_range_is_refused() {
        assert!(matches!(
            FocusScale::new(range(5, 5), Direction::Rising),
            Err(Error::EmptyRange(_))
        ));
    }
}
