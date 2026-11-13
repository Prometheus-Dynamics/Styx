//! The plan: which ISP outputs to configure so every consumer is served from one capture.
//!
//! A consumer asks for a size and a format ([`Ask`]). Asks that name the same size and format
//! are one output: they share one frame. Each other distinct ask needs an ISP output that makes
//! its size in its format (a *direct* route). When no output does, the one allowed fallback is
//! taken: an output that makes the same size in a format the converter of `styx-scale` turns into
//! the asked one (a *converted* route, reported as a conversion). Anything else is refused, with
//! the reason.
//!
//! There is no route planner and no cost model: crops, pyramids and sizes beyond the ISP's own
//! outputs are refused. The search is exhaustive over the (few) distinct asks and outputs, and
//! prefers direct routes over conversions and lower output indices over higher.

use std::fmt;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_settings::IspSettings;

use crate::capability::{IspCapability, OutputCapability, SizeRule};

/// What a consumer asks for: a picture format at a size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ask {
    pub code: FourCc,
    pub resolution: Resolution,
}

impl Ask {
    pub const fn new(code: FourCc, resolution: Resolution) -> Self {
        Self { code, resolution }
    }
}

/// One ISP output as configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    /// The format the ISP writes into this output.
    pub format: MediaFormat,
    /// Whether a consumer takes this output. An unused output is only here to keep the output
    /// indices in order when a later output is used: the ISP writes it at its default size.
    pub used: bool,
}

/// A conversion of an ISP output into a format a consumer asked for (the one fallback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conversion {
    /// The ISP output it converts.
    pub from: usize,
    /// The format the consumer gets.
    pub format: MediaFormat,
}

/// How one consumer is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The ISP output `output`, as is. `shared`: other consumers take the same frame.
    Isp { output: usize, shared: bool },
    /// The conversion `conversion`, of an ISP output. `shared`: as for [`Route::Isp`].
    Converted { conversion: usize, shared: bool },
}

/// Why a plan cannot serve a consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The consumer asks for more than the capture has: no ISP output upscales.
    Upscale {
        consumer: usize,
        ask: Ask,
        capture: Resolution,
    },
    /// No output makes the size in the format, directly or by the one conversion. The reason
    /// names what each output makes.
    Unserved {
        consumer: usize,
        ask: Ask,
        reason: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upscale {
                consumer,
                ask,
                capture,
            } => write!(
                f,
                "consumer {consumer} asks for {} at {}x{}: the capture is {}x{} and no ISP output upscales",
                code(ask.code),
                ask.resolution.width,
                ask.resolution.height,
                capture.width,
                capture.height,
            ),
            Self::Unserved {
                consumer, reason, ..
            } => write!(f, "consumer {consumer} is not served: {reason}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// The outputs to configure for one capture, and how each consumer is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    capture: Resolution,
    outputs: Vec<Output>,
    conversions: Vec<Conversion>,
    routes: Vec<Route>,
}

impl Plan {
    /// The capture size the plan is for.
    pub fn capture(&self) -> Resolution {
        self.capture
    }

    /// The ISP's outputs in index order, up to the last one a consumer uses.
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// The conversions of ISP outputs (at most one per distinct ask that needs one).
    pub fn conversions(&self) -> &[Conversion] {
        &self.conversions
    }

    /// How consumer `i` (in the order the asks were given) is served.
    pub fn route(&self, consumer: usize) -> Option<Route> {
        self.routes.get(consumer).copied()
    }

    /// Every consumer's route, in the order the asks were given.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Fills `settings.outputs` with the formats of the outputs, by index. Unused outputs before
    /// the last used one are included (see [`Output::used`]). Reuses the list's buffer.
    pub fn apply(&self, settings: &mut IspSettings) {
        settings.outputs.clear();
        settings
            .outputs
            .extend(self.outputs.iter().map(|output| output.format));
    }
}

/// Plans the outputs of `isp` for consumers asking `asks` from one `capture`.
///
/// Errors on the first consumer that cannot be served, in the order asked. Asks that repeat an
/// earlier ask take its output.
pub fn plan(isp: &dyn IspCapability, capture: Resolution, asks: &[Ask]) -> Result<Plan, PlanError> {
    let capabilities = isp.outputs();
    for (consumer, ask) in asks.iter().enumerate() {
        if ask.resolution.width > capture.width || ask.resolution.height > capture.height {
            return Err(PlanError::Upscale {
                consumer,
                ask: *ask,
                capture,
            });
        }
    }

    // Distinct asks, in the order first asked, and the distinct ask of each consumer.
    let mut distinct: Vec<Ask> = Vec::new();
    let mut of_consumer: Vec<usize> = Vec::with_capacity(asks.len());
    for ask in asks {
        let index = match distinct.iter().position(|d| d == ask) {
            Some(index) => index,
            None => {
                distinct.push(*ask);
                distinct.len() - 1
            }
        };
        of_consumer.push(index);
    }

    let mut configured: Vec<Option<(Resolution, FourCc)>> = vec![None; capabilities.len()];
    let mut picks: Vec<Option<Pick>> = vec![None; distinct.len()];
    if !search(
        0,
        &distinct,
        capabilities,
        capture,
        &mut configured,
        &mut picks,
    ) {
        // Blame the first distinct ask that no output set can serve even alone; when every ask
        // is servable alone, the last one is the one that tips the set over.
        let alone_fails = |ask: &Ask| {
            let mut alone = vec![None; capabilities.len()];
            let mut picks = vec![None; 1];
            !search(
                0,
                std::slice::from_ref(ask),
                capabilities,
                capture,
                &mut alone,
                &mut picks,
            )
        };
        let blamed = distinct
            .iter()
            .position(alone_fails)
            .unwrap_or(distinct.len() - 1);
        let ask = distinct[blamed];
        let consumer = of_consumer.iter().position(|&d| d == blamed).unwrap_or(0);
        let reason = if alone_fails(&ask) {
            refusal(isp, capture, &ask)
        } else {
            format!(
                "{} cannot make {}x{} {} beside the other asks ({}): the outputs that make that size are taken, and no output's format converts to {}",
                isp.name(),
                ask.resolution.width,
                ask.resolution.height,
                code(ask.code),
                outputs_description(isp, capture),
                code(ask.code),
            )
        };
        return Err(PlanError::Unserved {
            consumer,
            ask,
            reason,
        });
    }

    // The outputs, up to the last one configured. A gap takes its output's default size and
    // first format; an output with no format cannot stand in a gap.
    let last = configured
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |j| j + 1);
    let mut outputs = Vec::with_capacity(last);
    for (j, capability) in capabilities.iter().enumerate().take(last) {
        let output = match configured[j] {
            Some((resolution, code)) => Output {
                format: MediaFormat::with_defaults(code, resolution),
                used: true,
            },
            None => {
                let Some(&code) = capability.formats.first() else {
                    return Err(PlanError::Unserved {
                        consumer: 0,
                        ask: asks[0],
                        reason: format!(
                            "output {j} has to be configured before a later output, and it names no format"
                        ),
                    });
                };
                Output {
                    format: MediaFormat::with_defaults(
                        code,
                        capability.sizes.default_size(capture),
                    ),
                    used: false,
                }
            }
        };
        outputs.push(output);
    }

    // Conversions, one per distinct ask that takes the fallback; routes per consumer.
    let mut conversion_of: Vec<Option<usize>> = vec![None; distinct.len()];
    let mut conversions = Vec::new();
    for (d, pick) in picks.iter().enumerate() {
        if let Some(Pick::Fallback { from }) = *pick {
            conversion_of[d] = Some(conversions.len());
            conversions.push(Conversion {
                from,
                format: MediaFormat::with_defaults(distinct[d].code, distinct[d].resolution),
            });
        }
    }
    let mut counts = vec![0usize; distinct.len()];
    for &d in &of_consumer {
        counts[d] += 1;
    }
    let routes = of_consumer
        .iter()
        .map(|&d| {
            let shared = counts[d] > 1;
            match picks[d].expect("the search picks every distinct ask") {
                Pick::Direct { output } => Route::Isp { output, shared },
                Pick::Fallback { .. } => Route::Converted {
                    conversion: conversion_of[d].expect("a fallback has a conversion"),
                    shared,
                },
            }
        })
        .collect();

    Ok(Plan {
        capture,
        outputs,
        conversions,
        routes,
    })
}

/// How a distinct ask is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    /// ISP output `output`, configured for this ask.
    Direct { output: usize },
    /// The conversion of ISP output `from`, which makes the ask's size in a format the converter
    /// takes to the asked one.
    Fallback { from: usize },
}

/// Assigns each distinct ask from `i` on, backtracking. Direct routes first, then the fallback;
/// lower output indices first.
fn search(
    i: usize,
    asks: &[Ask],
    capabilities: &[OutputCapability],
    capture: Resolution,
    configured: &mut [Option<(Resolution, FourCc)>],
    picks: &mut [Option<Pick>],
) -> bool {
    let Some(ask) = asks.get(i).copied() else {
        return true;
    };
    for (j, capability) in capabilities.iter().enumerate() {
        if configured[j].is_some()
            || !capability.sizes.accepts(capture, ask.resolution)
            || !capability.formats.contains(&ask.code)
        {
            continue;
        }
        configured[j] = Some((ask.resolution, ask.code));
        picks[i] = Some(Pick::Direct { output: j });
        if search(i + 1, asks, capabilities, capture, configured, picks) {
            return true;
        }
        configured[j] = None;
    }
    for (j, capability) in capabilities.iter().enumerate() {
        match configured[j] {
            Some((resolution, format)) => {
                if resolution == ask.resolution
                    && format != ask.code
                    && styx_scale::layout::convertible(format, ask.code)
                {
                    picks[i] = Some(Pick::Fallback { from: j });
                    if search(i + 1, asks, capabilities, capture, configured, picks) {
                        return true;
                    }
                }
            }
            None => {
                if !capability.sizes.accepts(capture, ask.resolution) {
                    continue;
                }
                let Some(&format) = capability
                    .formats
                    .iter()
                    .find(|&&f| f != ask.code && styx_scale::layout::convertible(f, ask.code))
                else {
                    continue;
                };
                configured[j] = Some((ask.resolution, format));
                picks[i] = Some(Pick::Fallback { from: j });
                if search(i + 1, asks, capabilities, capture, configured, picks) {
                    return true;
                }
                configured[j] = None;
            }
        }
    }
    picks[i] = None;
    false
}

/// Why `ask` cannot be served by `isp` from `capture`.
fn refusal(isp: &dyn IspCapability, capture: Resolution, ask: &Ask) -> String {
    format!(
        "{} cannot make {}x{} {}: {}; no output makes that size in a format styx-scale converts to {}",
        isp.name(),
        ask.resolution.width,
        ask.resolution.height,
        code(ask.code),
        outputs_description(isp, capture),
        code(ask.code),
    )
}

/// What each of the ISP's outputs makes from `capture`, for a refusal.
fn outputs_description(isp: &dyn IspCapability, capture: Resolution) -> String {
    let outputs = isp.outputs();
    if outputs.is_empty() {
        return format!("{} has no outputs", isp.name());
    }
    outputs
        .iter()
        .enumerate()
        .map(|(j, output)| {
            let sizes = match output.sizes {
                SizeRule::Divisor(1) => "full size only".to_string(),
                SizeRule::Divisor(d) => {
                    let size = output.sizes.default_size(capture);
                    format!("1/{d} size only ({}x{})", size.width, size.height)
                }
                SizeRule::Downscale { max_divisor } => {
                    format!("any size from full down to 1/{max_divisor}")
                }
            };
            let formats: Vec<String> = output.formats.iter().map(|&f| code(f)).collect();
            let formats = if formats.is_empty() {
                "no format exposed".to_string()
            } else {
                formats.join(", ")
            };
            format!("output {j} makes {sizes}, formats {formats}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn code(code: FourCc) -> String {
    code.as_str().unwrap_or("????").to_string()
}
