//! Which ISP makes a camera's processed frames when the PiSP back end may refuse them: the policy
//! for `isp = "auto"`, apart from the jobs themselves so it can be driven by an injected clock.
//!
//! - On the back end, a job that completes clears the refusal count; `refusals` consecutive
//!   refused jobs switch the camera to the soft ISP. Each refused frame is made by the soft ISP,
//!   so no frame is lost to a refusal.
//! - On the soft ISP, the back end is tried again after a delay that starts at `retry_start` and
//!   doubles after each try the back end refuses, up to `retry_cap`. A try is one frame posted to
//!   the back end; the first job that completes switches the camera back.
//! - Both directions are counted, and the last reason with its time is kept for the status.
//!
//! Nothing here allocates once the reason buffer has grown to the longest reason seen.

use std::time::Duration;

/// The default refusals in a row that switch a camera to the soft ISP.
pub const DEFAULT_REFUSALS: u32 = 3;
/// The default first delay before the back end is tried again.
pub const DEFAULT_RETRY_START: Duration = Duration::from_millis(250);
/// The default longest delay between tries.
pub const DEFAULT_RETRY_CAP: Duration = Duration::from_secs(5);
/// The longest cap a setting may give (the service's failure rule: retries are never further
/// apart than this).
pub const MAX_RETRY_CAP: Duration = Duration::from_secs(10);

/// The policy's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchPolicy {
    pub refusals: u32,
    pub retry_start: Duration,
    pub retry_cap: Duration,
}

impl Default for SwitchPolicy {
    fn default() -> Self {
        Self {
            refusals: DEFAULT_REFUSALS,
            retry_start: DEFAULT_RETRY_START,
            retry_cap: DEFAULT_RETRY_CAP,
        }
    }
}

impl SwitchPolicy {
    /// The policy, refused by name when a number is out of its range (never clamped): at least one
    /// refusal, `0 < retry_start <= retry_cap <= MAX_RETRY_CAP`.
    pub fn new(refusals: u32, retry_start: Duration, retry_cap: Duration) -> Result<Self, String> {
        if refusals == 0 {
            return Err("isp_refusals: at least 1".into());
        }
        if retry_start.is_zero() {
            return Err("isp_retry_start_ms: above 0".into());
        }
        if retry_cap < retry_start {
            return Err("isp_retry_cap_ms: at least isp_retry_start_ms".into());
        }
        if retry_cap > MAX_RETRY_CAP {
            return Err(format!(
                "isp_retry_cap_ms: at most {} ms",
                MAX_RETRY_CAP.as_millis()
            ));
        }
        Ok(Self {
            refusals,
            retry_start,
            retry_cap,
        })
    }
}

/// Which ISP makes the camera's frames now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Active {
    /// The back end.
    Pisp,
    /// The soft ISP, with the back end tried again at `retry_at_ns`.
    Soft,
}

impl Active {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pisp => "pisp",
            Self::Soft => "soft",
        }
    }
}

/// Where the next frame goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Post it to the back end.
    Pisp,
    /// Post it to the back end as a try while the camera is on the soft ISP: its completion
    /// switches the camera back, its refusal doubles the delay.
    PispTry,
    /// Make it with the soft ISP.
    Soft,
}

/// The policy's state for one camera. See the module documentation.
#[derive(Debug)]
pub struct Switcher {
    policy: SwitchPolicy,
    active: Active,
    /// Refused jobs in a row while on the back end.
    in_a_row: u32,
    /// The delay before the next try, while on the soft ISP.
    delay: Duration,
    retry_at_ns: u64,
    /// A try is posted and has not ended yet.
    trying: bool,
    to_soft: u64,
    to_pisp: u64,
    refused: u64,
    reason: String,
    reason_at_ns: u64,
}

impl Switcher {
    pub fn new(policy: SwitchPolicy) -> Self {
        Self {
            policy,
            active: Active::Pisp,
            in_a_row: 0,
            delay: policy.retry_start,
            retry_at_ns: 0,
            trying: false,
            to_soft: 0,
            to_pisp: 0,
            refused: 0,
            reason: String::new(),
            reason_at_ns: 0,
        }
    }

    /// A switcher that starts on the soft ISP because the back end could not be opened, with the
    /// first try due after `retry_start`.
    pub fn starting_soft(policy: SwitchPolicy, now_ns: u64, reason: &str) -> Self {
        let mut switcher = Self::new(policy);
        switcher.active = Active::Soft;
        switcher.retry_at_ns = now_ns.saturating_add(nanos(policy.retry_start));
        switcher.note(now_ns, reason);
        switcher
    }

    pub const fn active(&self) -> Active {
        self.active
    }

    /// Where the frame arriving at `now_ns` goes.
    pub fn route(&mut self, now_ns: u64) -> Route {
        match self.active {
            Active::Pisp => Route::Pisp,
            Active::Soft if !self.trying && now_ns >= self.retry_at_ns => {
                self.trying = true;
                Route::PispTry
            }
            Active::Soft => Route::Soft,
        }
    }

    /// A back-end job completed (a normal one or a try).
    pub fn completed(&mut self, now_ns: u64) {
        self.in_a_row = 0;
        if self.active == Active::Soft {
            self.trying = false;
            self.active = Active::Pisp;
            self.delay = self.policy.retry_start;
            self.to_pisp += 1;
            self.note(now_ns, "back end completed a job: back on the back end");
        }
    }

    /// A back-end job was refused for `reason` (its frame is made by the soft ISP).
    pub fn refused(&mut self, now_ns: u64, reason: &str) {
        self.refused += 1;
        self.note(now_ns, reason);
        match self.active {
            Active::Pisp => {
                self.in_a_row += 1;
                if self.in_a_row >= self.policy.refusals {
                    self.in_a_row = 0;
                    self.active = Active::Soft;
                    self.delay = self.policy.retry_start;
                    self.retry_at_ns = now_ns.saturating_add(nanos(self.delay));
                    self.to_soft += 1;
                }
            }
            Active::Soft => {
                // A try refused: wait twice as long, up to the cap.
                self.trying = false;
                self.delay = (self.delay * 2).min(self.policy.retry_cap);
                self.retry_at_ns = now_ns.saturating_add(nanos(self.delay));
            }
        }
    }

    /// A try that could not be posted (the back end had no free buffer): it is neither a
    /// completion nor a refusal, so the next frame may try again and the delay stays.
    pub fn try_not_posted(&mut self) {
        self.trying = false;
    }

    /// Switches to the soft ISP and back counted so far.
    pub const fn switches(&self) -> (u64, u64) {
        (self.to_soft, self.to_pisp)
    }

    /// Back-end jobs refused so far.
    pub const fn refused_total(&self) -> u64 {
        self.refused
    }

    /// The last reason and when it was noted (`CLOCK_MONOTONIC` ns), `None` before any.
    pub fn last_reason(&self) -> Option<(&str, u64)> {
        (!self.reason.is_empty()).then_some((self.reason.as_str(), self.reason_at_ns))
    }

    /// The delay the next try waits after a refused one.
    pub const fn retry_delay(&self) -> Duration {
        self.delay
    }

    /// When the next try is due, while on the soft ISP.
    pub const fn retry_at_ns(&self) -> Option<u64> {
        match self.active {
            Active::Soft => Some(self.retry_at_ns),
            Active::Pisp => None,
        }
    }

    fn note(&mut self, now_ns: u64, reason: &str) {
        self.reason.clear();
        self.reason.push_str(reason);
        self.reason_at_ns = now_ns;
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn policy() -> SwitchPolicy {
        SwitchPolicy::new(3, Duration::from_millis(100), Duration::from_millis(800)).expect("valid")
    }

    #[test]
    fn three_refusals_in_a_row_switch_to_soft_and_a_completion_between_resets_the_count() {
        let mut s = Switcher::new(policy());
        s.refused(0, "busy");
        s.refused(1, "busy");
        s.completed(2);
        s.refused(3, "busy");
        s.refused(4, "busy");
        assert_eq!(
            s.active(),
            Active::Pisp,
            "two in a row since the completion"
        );
        s.refused(5, "timeout");
        assert_eq!(s.active(), Active::Soft);
        assert_eq!(s.switches(), (1, 0));
        assert_eq!(s.refused_total(), 5);
        assert_eq!(s.last_reason(), Some(("timeout", 5)));
    }

    #[test]
    fn on_soft_the_back_end_is_tried_after_the_delay_and_a_completion_switches_back() {
        let mut s = Switcher::new(policy());
        for t in 0..3 {
            s.refused(t, "refused");
        }
        assert_eq!(s.retry_at_ns(), Some(2 + 100 * MS));
        assert_eq!(s.route(50 * MS), Route::Soft, "before the delay");
        assert_eq!(
            s.route(2 + 100 * MS),
            Route::PispTry,
            "at the delay: one try"
        );
        assert_eq!(
            s.route(2 + 101 * MS),
            Route::Soft,
            "one try in flight at a time"
        );
        s.completed(2 + 110 * MS);
        assert_eq!(s.active(), Active::Pisp);
        assert_eq!(s.switches(), (1, 1));
        assert_eq!(s.route(2 + 111 * MS), Route::Pisp);
    }

    #[test]
    fn a_refused_try_doubles_the_delay_up_to_the_cap() {
        let mut s = Switcher::new(policy());
        for t in 0..3 {
            s.refused(t, "refused");
        }
        let mut now = 2;
        let mut delays = Vec::new();
        for _ in 0..6 {
            now = s.retry_at_ns().expect("on soft");
            assert_eq!(s.route(now), Route::PispTry);
            s.refused(now, "still refused");
            delays.push(s.retry_delay().as_millis());
        }
        assert_eq!(
            delays,
            [200, 400, 800, 800, 800, 800],
            "doubling, capped at 800 ms"
        );
        assert_eq!(s.retry_at_ns(), Some(now + 800 * MS));
        assert_eq!(s.switches(), (1, 0), "tries are not switches");
        // Back on the back end, the delay starts short again.
        assert_eq!(s.route(now + 800 * MS), Route::PispTry);
        s.completed(now + 801 * MS);
        for t in 0..3 {
            s.refused(now + 802 * MS + t, "refused");
        }
        assert_eq!(s.retry_delay(), Duration::from_millis(100));
        assert_eq!(s.switches(), (2, 1));
    }

    #[test]
    fn starting_on_soft_tries_the_back_end_after_the_first_delay() {
        let mut s = Switcher::starting_soft(policy(), 10, "pisp: set_output refused");
        assert_eq!(s.active(), Active::Soft);
        assert_eq!(s.last_reason(), Some(("pisp: set_output refused", 10)));
        assert_eq!(s.route(10), Route::Soft);
        assert_eq!(s.route(10 + 100 * MS), Route::PispTry);
    }

    #[test]
    fn the_policy_refuses_out_of_range_numbers_by_name() {
        let ms = Duration::from_millis;
        assert!(
            SwitchPolicy::new(0, ms(1), ms(1))
                .unwrap_err()
                .contains("isp_refusals")
        );
        assert!(
            SwitchPolicy::new(1, ms(0), ms(1))
                .unwrap_err()
                .contains("isp_retry_start_ms")
        );
        assert!(
            SwitchPolicy::new(1, ms(5), ms(4))
                .unwrap_err()
                .contains("isp_retry_cap_ms")
        );
        assert!(
            SwitchPolicy::new(1, ms(5), ms(10_001))
                .unwrap_err()
                .contains("at most 10000")
        );
        assert_eq!(
            SwitchPolicy::default(),
            SwitchPolicy::new(3, ms(250), ms(5_000)).unwrap()
        );
    }
}
