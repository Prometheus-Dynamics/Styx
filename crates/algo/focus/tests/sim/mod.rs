//! A simulated focus lens on Lemnos's simulated backend: a driver that speaks `lens/1` the way the
//! Lemnos VCM driver does (the first move powers the chip up and answers `AGAIN`; later moves
//! write the position; the lens reports its last position, or `NOT_SUPPORTED` before the first
//! move). The position is the sim's bytes, so nothing here moves a real actuator.
//!
//! `fail_moves` (device config) makes that many moves after power-up fail with a bus error, to
//! exercise the retry path.

use lemnos_contract_lens::{FocusDirection, Lens, Range};
use lemnos_core::{Config, ConfigValue, Lifecycle, Result, Status, driver};

/// The lens's range: a 10-bit actuator, as the DW9807 and DW9714.
pub const RANGE: Range = Range { min: 0, max: 1023 };

#[driver("sim-lens", provides = [Lens])]
pub struct SimLens {
    powered: bool,
    position: Option<u16>,
    fail_moves: u32,
}

impl Lifecycle for SimLens {
    type Needs = SimLensNeeds;

    fn start(_needs: SimLensNeeds, cfg: &Config<'_>) -> Result<Self> {
        let fail_moves = match cfg.get("fail_moves") {
            None => 0,
            Some(ConfigValue::Int(n)) => u32::try_from(n).map_err(|_| Status::INVALID_ARGUMENT)?,
            Some(_) => return Err(Status::INVALID_ARGUMENT),
        };
        Ok(Self {
            powered: false,
            position: None,
            fail_moves,
        })
    }
}

impl Lens for SimLens {
    fn range(&mut self) -> Result<Range> {
        Ok(RANGE)
    }

    fn direction(&mut self) -> Result<FocusDirection> {
        // The simulated lens does not say which way focus moves: never a guess.
        Ok(FocusDirection::Unknown)
    }

    fn settle_us(&mut self, _distance: u16) -> Result<Option<u32>> {
        // No settle time is modelled.
        Ok(None)
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
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-lens-board\"\n\
         [[devices]]\nid = \"lens0\"\ndriver = \"sim-lens\"\nconfig = {{ fail_moves = {fail_moves} }}\n"
    )
}
