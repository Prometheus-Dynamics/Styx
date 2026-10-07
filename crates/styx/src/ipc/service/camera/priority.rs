//! Low-priority clients ([`ClientPriority::Low`](crate::ipc::ClientPriority::Low)): planned
//! after the normal clients, never changing what those get.
//!
//! - **Planning** ([`Camera::plan_clients`]): normal clients are planned as if the low-priority
//!   ones were not there. A low-priority client then gets its own request when that leaves
//!   every normal client's plan as it is and adds no CPU step to the service, else a share of a
//!   normal client's frames ([`shared_request`]), which it scales itself.
//! - **Joining a running capture** ([`Camera::attach_low`]): only without a restart: its own
//!   request when the running capture already gives it (same setup, the others' plans
//!   unchanged), else a share of a running consumer's frames, else it is refused.

use super::{Camera, LOW_REFUSED, ServiceConfig, State, describe};
use crate::planner::{
    Delivered, FramePlan, FrameRequest, Frames, PlanError, SharedFramePlan, StepExecution,
    same_plan,
};

/// A shared plan for a camera's clients.
pub(in crate::ipc::service) struct Planned {
    pub(super) plan: SharedFramePlan,
    /// The consumer of `plan` each client gets, in client order (`None`: a low-priority client
    /// that cannot be served next to the others without changing them).
    pub(super) consumers: Vec<Option<usize>>,
    /// The request each consumer was planned with, in consumer order.
    pub(super) requests: Vec<FrameRequest>,
}

impl Camera {
    /// One shared plan for `clients` (each one's request and whether it is low priority).
    ///
    /// Normal clients are planned as if the low-priority ones were not there. Each
    /// low-priority client then gets its own request when that leaves every normal client's
    /// plan as it is, adds no CPU step to the service and, with `keep` (a running capture's
    /// setup), keeps the capture as it is set up; otherwise a share of a normal client's
    /// frames ([`shared_request`]) when that changes nothing either. So a preview takes the
    /// ISP's free second output when the capture starts anyway, and a vision client's frames
    /// (to scale itself) when its own would change the capture.
    pub(in crate::ipc::service) fn plan_clients(
        &self,
        clients: &[(FrameRequest, bool)],
        fps: Option<u32>,
        keep: Option<&str>,
        config: &ServiceConfig,
    ) -> Result<Planned, PlanError> {
        let normal: Vec<usize> = (0..clients.len()).filter(|&i| !clients[i].1).collect();
        if normal.is_empty() {
            // Nobody to protect: plan them all as they asked.
            let requests: Vec<FrameRequest> = clients.iter().map(|(r, _)| r.clone()).collect();
            return Ok(Planned {
                plan: self.plan_for(&requests, fps, config)?,
                consumers: (0..clients.len()).map(Some).collect(),
                requests,
            });
        }
        let requests = |chosen: &[(usize, FrameRequest)]| -> Vec<FrameRequest> {
            chosen.iter().map(|(_, r)| r.clone()).collect()
        };
        let mut chosen: Vec<(usize, FrameRequest)> =
            normal.iter().map(|&i| (i, clients[i].0.clone())).collect();
        let base = self.plan_for(&requests(&chosen), fps, config)?;
        let mut plan = base.clone();
        for low in (0..clients.len()).filter(|&i| clients[i].1) {
            let own = clients[low].0.clone();
            let shared = shared_request(&base, &own);
            for (candidate, is_own) in [(Some(own), true), (shared, false)] {
                let Some(candidate) = candidate else {
                    continue;
                };
                let mut trial = chosen.clone();
                trial.push((low, candidate));
                trial.sort_by_key(|(i, _)| *i);
                let Ok(tried) = self.plan_for(&requests(&trial), fps, config) else {
                    continue;
                };
                let at = |i: usize| trial.iter().position(|(j, _)| *j == i).expect("planned");
                let unchanged = normal
                    .iter()
                    .enumerate()
                    .all(|(k, &i)| same_plan(&base.consumers[k], &tried.consumers[at(i)]));
                let kept = keep.is_none_or(|setup| tried.setup_key() == setup);
                if unchanged && kept && (!is_own || no_cpu(&tried.consumers[at(low)])) {
                    chosen = trial;
                    plan = tried;
                    break;
                }
            }
        }
        Ok(Planned {
            plan,
            consumers: (0..clients.len())
                .map(|i| chosen.iter().position(|(j, _)| *j == i))
                .collect(),
            requests: requests(&chosen),
        })
    }

    /// Attach a low-priority client to the running capture without restarting it or changing
    /// any consumer's frames: with its own request when the capture gives it as it runs, else
    /// with a share of a running consumer's frames. Refused when neither fits, or the capture
    /// has no buffers for another client.
    pub(super) fn attach_low(
        &self,
        state: &mut State,
        request: &FrameRequest,
        config: &ServiceConfig,
    ) -> Result<(Frames, String, Delivered), String> {
        let fps = state.fps_override;
        let (clients, id) = (state.clients.len(), state.next_id);
        let running = state.running.as_mut().expect("a running capture");
        if running.capacity <= clients {
            return Err(format!(
                "{LOW_REFUSED}: the running capture has no buffers for another client, and a \
                 low-priority client does not restart it"
            ));
        }
        let shared = shared_request(&running.plan, request);
        let mut last_error = None;
        for (candidate, is_own) in [(Some(request.clone()), true), (shared, false)] {
            let Some(candidate) = candidate else {
                continue;
            };
            let mut trial = running.requests.clone();
            trial.push(candidate);
            let tried = match self.plan_for(&trial, fps, config) {
                Ok(tried) => tried,
                Err(err) => {
                    last_error = Some(describe(&err));
                    continue;
                }
            };
            let new = tried.consumers.last().expect("one plan per request");
            let unchanged = running
                .plan
                .consumers
                .iter()
                .zip(&tried.consumers)
                .all(|(a, b)| same_plan(a, b));
            if tried.setup_key() == running.setup && unchanged && (!is_own || no_cpu(new)) {
                let frames = running.session.attach(new, true);
                let answer = (frames, new.to_string(), new.delivered());
                running.plan = tried;
                running.requests = trial;
                running.owners.push(id);
                return Ok(answer);
            }
        }
        Err(match last_error {
            Some(reason) => format!("{LOW_REFUSED}: {reason}"),
            None => LOW_REFUSED.into(),
        })
    }
}

/// No step of `plan` runs on the CPU (in the service): capture, hardware scaling, views.
fn no_cpu(plan: &FramePlan) -> bool {
    plan.steps
        .iter()
        .all(|step| step.execution != StepExecution::Cpu)
}

/// A low-priority client's share of another client's frames: that client's request (its
/// format, size, pyramid and route, so both are prepared once, together) without its regions
/// of interest. The client whose frames are in a format `own` accepts is preferred, then the
/// smallest frames at least `own`'s size (else the largest); inter-coded streams (H.264,
/// H.265) are never shared.
pub(super) fn shared_request(plan: &SharedFramePlan, own: &FrameRequest) -> Option<FrameRequest> {
    let target = own.size.unwrap_or((1, 1));
    plan.consumers
        .iter()
        .filter(|consumer| !consumer.inter_coded())
        .min_by_key(|consumer| {
            let (w, h) = consumer.output_resolution();
            let area = u64::from(w) * u64::from(h);
            let covers = w >= target.0 && h >= target.1;
            (
                !own.format.accepts(consumer.delivered().format),
                !covers,
                if covers { area } else { u64::MAX - area },
            )
        })
        .map(|consumer| {
            let mut request = consumer.request.clone();
            request.roi = None;
            request.extra_regions.clear();
            request.skip_stale_regions = false;
            request
        })
}

#[cfg(test)]
mod tests {
    use styx_core::prelude::*;

    use super::*;
    use crate::capture_api::make_virtual_device;
    use crate::ipc::service::CameraService;
    use crate::prelude::Mode;

    fn camera() -> Camera {
        let mode = |w, h| {
            Mode::with_interval(
                MediaFormat::new(
                    FourCc::NV12,
                    Resolution::new(w, h).expect("size"),
                    ColorSpace::Srgb,
                ),
                Interval::from_fps(30).expect("rate"),
            )
        };
        Camera::new(make_virtual_device(
            "priority",
            [mode(1280, 800), mode(640, 400)],
        ))
    }

    fn config() -> ServiceConfig {
        CameraService::new(make_virtual_device("unused", [])).config
    }

    #[test]
    fn low_priority_requests_never_change_normal_plans() {
        let camera = camera();
        let config = config();
        let vision = Frames::nv12().size(1280, 800);
        let alone = camera
            .plan_clients(&[(vision.clone(), false)], None, None, &config)
            .unwrap();
        // A preview that must have 640x400 would move the capture to the 640x400 mode (no
        // scaler here): it gets a share of the vision client's frames instead.
        let preview = Frames::nv12().size(640, 400).strict();
        let planned = camera
            .plan_clients(
                &[(vision.clone(), false), (preview.clone(), true)],
                None,
                None,
                &config,
            )
            .unwrap();
        assert!(same_plan(
            &alone.plan.consumers[0],
            &planned.plan.consumers[0]
        ));
        assert_eq!(planned.consumers, vec![Some(0), Some(1)]);
        assert_eq!(planned.requests[1], vision);
        assert_eq!(planned.plan.setup_key(), alone.plan.setup_key());
        // As a normal client it would have changed the vision client's mode.
        let normal = camera
            .plan_clients(
                &[(vision.clone(), false), (preview, false)],
                None,
                None,
                &config,
            )
            .unwrap();
        assert!(!same_plan(
            &alone.plan.consumers[0],
            &normal.plan.consumers[0]
        ));

        // A preview the running capture already serves keeps its own request.
        let fits = Frames::nv12().size(1280, 800).every_frame(1);
        let planned = camera
            .plan_clients(
                &[(vision.clone(), false), (fits.clone(), true)],
                None,
                None,
                &config,
            )
            .unwrap();
        assert_eq!(planned.requests[1], fits);

        // Alone, a low-priority client is planned as it asked.
        let alone = camera
            .plan_clients(
                &[(Frames::nv12().size(640, 400), true)],
                None,
                None,
                &config,
            )
            .unwrap();
        assert_eq!(alone.plan.consumers[0].output_resolution(), (640, 400));
    }

    #[test]
    fn shares_prefer_accepted_formats_and_the_smallest_covering_frames() {
        let camera = camera();
        let config = config();
        let plan = camera
            .plan_clients(
                &[
                    (Frames::nv12().size(1280, 800), false),
                    (
                        Frames::gray()
                            .size(640, 400)
                            .roi(FrameRect::new(0, 0, 64, 64)),
                        false,
                    ),
                ],
                None,
                None,
                &config,
            )
            .unwrap()
            .plan;
        let gray = shared_request(&plan, &Frames::gray().size(320, 200)).unwrap();
        assert_eq!(gray.format, OutputFormat::Luma);
        assert_eq!(gray.roi, None, "regions are the other client's own");
        let nv12 = shared_request(&plan, &Frames::nv12().size(320, 200)).unwrap();
        assert_eq!(nv12.size, Some((1280, 800)));
    }
}
