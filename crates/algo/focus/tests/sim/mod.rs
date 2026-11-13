//! A simulated focus lens on Lemnos's simulated backend: a driver that speaks `lens/1` the way the
//! Lemnos VCM driver does (the first move powers the chip up and answers `AGAIN`; later moves
//! write the position; the lens reports its last position, or `NOT_SUPPORTED` before the first
//! move). The position is the sim's bytes, so nothing here moves a real actuator.
//!
//! `fail_moves` (device config) makes that many moves after power-up fail with a bus error, to
//! exercise the retry path. `direction` (0 unknown, 1 toward near, 2 toward far) and `settle_us`
//! (absent: no figure) are the lens's answers to `direction` and `settle_us`.

use lemnos_contract_lens::{FocusDirection, Lens, Range};
use lemnos_core::{Config, ConfigValue, Lifecycle, Result, Status, driver};

/// The lens's range: a 10-bit actuator, as the DW9807 and DW9714.
pub const RANGE: Range = Range { min: 0, max: 1023 };

#[driver("sim-lens", provides = [Lens])]
pub struct SimLens {
    powered: bool,
    position: Option<u16>,
    fail_moves: u32,
    direction: FocusDirection,
    settle_us: Option<u32>,
}

impl Lifecycle for SimLens {
    type Needs = SimLensNeeds;

    fn start(_needs: SimLensNeeds, cfg: &Config<'_>) -> Result<Self> {
        let fail_moves = match cfg.get("fail_moves") {
            None => 0,
            Some(ConfigValue::Int(n)) => u32::try_from(n).map_err(|_| Status::INVALID_ARGUMENT)?,
            Some(_) => return Err(Status::INVALID_ARGUMENT),
        };
        let direction = match cfg.get("direction") {
            None => FocusDirection::Unknown,
            Some(ConfigValue::Int(0)) => FocusDirection::Unknown,
            Some(ConfigValue::Int(1)) => FocusDirection::TowardNear,
            Some(ConfigValue::Int(2)) => FocusDirection::TowardFar,
            Some(_) => return Err(Status::INVALID_ARGUMENT),
        };
        let settle_us = match cfg.get("settle_us") {
            None => None,
            Some(ConfigValue::Int(n)) => {
                Some(u32::try_from(n).map_err(|_| Status::INVALID_ARGUMENT)?)
            }
            Some(_) => return Err(Status::INVALID_ARGUMENT),
        };
        Ok(Self {
            powered: false,
            position: None,
            fail_moves,
            direction,
            settle_us,
        })
    }
}

impl Lens for SimLens {
    fn range(&mut self) -> Result<Range> {
        Ok(RANGE)
    }

    fn direction(&mut self) -> Result<FocusDirection> {
        Ok(self.direction)
    }

    fn settle_us(&mut self, _distance: u16) -> Result<Option<u32>> {
        Ok(self.settle_us)
    }

    fn set_position(&mut self, position: u16) -> Result<()> {
        if !self.powered {
            self.powered = true;
            return Err(Status::AGAIN);
        }
        if self.fail_moves > 0 {
            self.fail_moves -= 1;
            return Err(Status::IO);
        }
        let target = position.clamp(RANGE.min, RANGE.max);
        self.position = Some(target);
        Ok(())
    }

    fn position(&mut self) -> Result<u16> {
        self.position.ok_or(Status::NOT_SUPPORTED)
    }
}

/// A board with one simulated lens, `lens0`, failing its first `fail_moves` moves after power-up.
pub fn board(fail_moves: u32) -> String {
    board_with(fail_moves, 0, None)
}

/// [`board`] with the lens's `direction` answer (0 unknown, 1 toward near, 2 toward far) and its
/// `settle_us` answer (`None`: no figure).
pub fn board_with(fail_moves: u32, direction: u8, settle_us: Option<u32>) -> String {
    let settle = settle_us.map_or(String::new(), |us| format!(", settle_us = {us}"));
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-lens-board\"\n\
         [[devices]]\nid = \"lens0\"\ndriver = \"sim-lens\"\n\
         config = {{ fail_moves = {fail_moves}, direction = {direction}{settle} }}\n"
    )
}
