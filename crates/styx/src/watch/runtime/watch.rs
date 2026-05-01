use std::collections::BTreeSet;
use std::time::Instant;

use crate::{BackendKind, ProbeResult, probe_backends_with_errors_with_options};

use super::{WatchRefreshReport, WatchRuntime};
use crate::watch::{DeviceWatchEvent, DeviceWatcher, WatchError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchedRefreshMode {
    Full,
    Incremental,
}

impl WatchRuntime {
    pub fn poll_watcher_and_refresh(
        &mut self,
        watcher: &mut dyn DeviceWatcher,
    ) -> Result<Option<WatchRefreshReport>, WatchError> {
        let watch_events = watcher.poll()?;
        self.record_pending_watch_events(watch_events);
        self.handle_watch_events_and_refresh_with_mode(watcher.name(), WatchedRefreshMode::Full)
    }

    pub fn poll_watcher_and_refresh_incremental(
        &mut self,
        watcher: &mut dyn DeviceWatcher,
    ) -> Result<Option<WatchRefreshReport>, WatchError> {
        let watch_events = watcher.poll()?;
        self.record_pending_watch_events(watch_events);
        self.handle_watch_events_and_refresh_with_mode(
            watcher.name(),
            WatchedRefreshMode::Incremental,
        )
    }

    fn record_pending_watch_events(&mut self, watch_events: Vec<DeviceWatchEvent>) {
        if watch_events.is_empty() {
            return;
        }
        self.pending_watch_events.extend(watch_events);
        self.pending_watch_deadline = Some(Instant::now() + self.config.watch_settle_time);
    }

    fn handle_watch_events_and_refresh_with_mode(
        &mut self,
        watcher_name: &'static str,
        mode: WatchedRefreshMode,
    ) -> Result<Option<WatchRefreshReport>, WatchError> {
        let Some(prepared) = prepare_watch_refresh(self, watcher_name, mode) else {
            return Ok(None);
        };
        Ok(Some(prepared.run()?.finish(self)))
    }
}

fn prepare_watch_refresh(
    runtime: &mut WatchRuntime,
    watcher_name: &'static str,
    mode: WatchedRefreshMode,
) -> Option<PreparedWatchRefresh> {
    if runtime.pending_watch_events.is_empty() {
        return None;
    }
    if let Some(deadline) = runtime.pending_watch_deadline
        && Instant::now() < deadline
    {
        return None;
    }

    let watch_events = std::mem::take(&mut runtime.pending_watch_events);
    runtime.pending_watch_deadline = None;

    let scoped_backends = watch_events
        .iter()
        .flat_map(|event| event.backends.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    if scoped_backends.is_empty() {
        return None;
    }

    Some(PreparedWatchRefresh {
        watcher_name,
        watch_events,
        mode,
        scoped_backends,
    })
}

struct PreparedWatchRefresh {
    watcher_name: &'static str,
    watch_events: Vec<DeviceWatchEvent>,
    mode: WatchedRefreshMode,
    scoped_backends: Vec<BackendKind>,
}

impl PreparedWatchRefresh {
    fn run(self) -> Result<CompletedWatchRefresh, WatchError> {
        let _ = self.watcher_name;
        let _ = self.mode;
        let probe_result =
            probe_backends_with_errors_with_options(true, Some(&self.scoped_backends), None);
        Ok(CompletedWatchRefresh {
            watch_events: self.watch_events,
            probe_result,
            scoped_backends: self.scoped_backends,
        })
    }
}

struct CompletedWatchRefresh {
    watch_events: Vec<DeviceWatchEvent>,
    probe_result: ProbeResult,
    scoped_backends: Vec<BackendKind>,
}

impl CompletedWatchRefresh {
    fn finish(self, runtime: &mut WatchRuntime) -> WatchRefreshReport {
        runtime.finish_scoped_refresh(self.probe_result, &self.scoped_backends, self.watch_events)
    }
}
