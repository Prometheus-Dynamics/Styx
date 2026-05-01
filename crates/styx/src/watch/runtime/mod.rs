use crate::{ProbeResult, probe_all_with_errors_with_options};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use super::{
    DeviceWatchEvent, InventoryDiff, InventoryEvent, InventoryEventCursor, InventoryEventPoll,
    InventoryEventSubscription, WatchRuntimeConfig,
};

mod events;
mod refresh;
mod watch;

pub(crate) use refresh::{diff_devices, merge_probe_result, normalize_probe_result};

#[derive(Debug, Clone)]
pub struct WatchRefreshReport {
    pub watch_events: Vec<DeviceWatchEvent>,
    pub probe_result: ProbeResult,
    pub diff: InventoryDiff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryEventRetentionStats {
    pub event_base_index: usize,
    pub event_tail_index: usize,
    pub retained_events: usize,
    pub retained_event_bytes: usize,
    pub max_retained_events: usize,
    pub max_retained_event_bytes: Option<usize>,
}

pub struct WatchRuntime {
    pub(crate) config: WatchRuntimeConfig,
    pub(crate) snapshot: ProbeResult,
    pub(crate) event_base_index: usize,
    pub(crate) retained_event_bytes: usize,
    pub(crate) events: Vec<InventoryEvent>,
    pub(crate) event_notifier: Arc<RuntimeEventNotifier>,
    pub(crate) pending_watch_events: Vec<DeviceWatchEvent>,
    pub(crate) pending_watch_deadline: Option<Instant>,
}

impl Default for WatchRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchRuntime {
    pub fn new() -> Self {
        Self::with_config(WatchRuntimeConfig::default())
    }

    pub fn with_config(config: WatchRuntimeConfig) -> Self {
        Self {
            config,
            snapshot: ProbeResult {
                devices: Vec::new(),
                errors: Vec::new(),
            },
            event_base_index: 0,
            retained_event_bytes: 0,
            events: Vec::new(),
            event_notifier: Arc::new(RuntimeEventNotifier::default()),
            pending_watch_events: Vec::new(),
            pending_watch_deadline: None,
        }
    }

    pub fn probe_result(&self) -> &ProbeResult {
        &self.snapshot
    }

    pub fn devices(&self) -> &[crate::ProbedDevice] {
        &self.snapshot.devices
    }

    pub fn errors(&self) -> &[crate::BackendProbeError] {
        &self.snapshot.errors
    }

    pub fn events(&self) -> &[InventoryEvent] {
        self.events.as_slice()
    }

    pub fn refresh(&mut self) -> WatchRefreshReport {
        self.finish_refresh(probe_all_with_errors_with_options(false), Vec::new())
    }

    pub fn refresh_uncached(&mut self) -> WatchRefreshReport {
        self.finish_refresh(probe_all_with_errors_with_options(true), Vec::new())
    }

    pub fn subscribe(&self) -> InventoryEventCursor {
        InventoryEventCursor::from_index(self.event_tail_index())
    }

    pub fn subscribe_from_start(&self) -> InventoryEventCursor {
        InventoryEventCursor::from_index(self.event_base_index)
    }

    pub fn subscribe_blocking(&self) -> InventoryEventSubscription {
        InventoryEventSubscription::new(self.subscribe(), Arc::clone(&self.event_notifier))
    }

    pub fn subscribe_from_start_blocking(&self) -> InventoryEventSubscription {
        InventoryEventSubscription::new(
            self.subscribe_from_start(),
            Arc::clone(&self.event_notifier),
        )
    }

    pub fn poll_events<'a>(&'a self, cursor: &mut InventoryEventCursor) -> &'a [InventoryEvent] {
        self.poll_events_with_status(cursor).events()
    }

    pub fn poll_events_with_status<'a>(
        &'a self,
        cursor: &mut InventoryEventCursor,
    ) -> InventoryEventPoll<'a> {
        let was_truncated = self.is_cursor_stale(cursor);
        let start_index = cursor
            .next_index()
            .max(self.event_base_index)
            .min(self.event_tail_index());
        let start_offset = start_index - self.event_base_index;
        cursor.advance_to(self.event_tail_index());
        InventoryEventPoll::new(&self.events[start_offset..], was_truncated)
    }

    pub fn take_events(&mut self) -> Vec<InventoryEvent> {
        let events = std::mem::take(&mut self.events);
        self.event_base_index += events.len();
        self.retained_event_bytes = 0;
        if !events.is_empty() {
            self.event_notifier.notify_changed();
        }
        events
    }

    pub fn event_retention_stats(&self) -> InventoryEventRetentionStats {
        InventoryEventRetentionStats {
            event_base_index: self.event_base_index,
            event_tail_index: self.event_tail_index(),
            retained_events: self.events.len(),
            retained_event_bytes: self.retained_event_bytes,
            max_retained_events: self.config.max_retained_events,
            max_retained_event_bytes: self.config.max_retained_event_bytes,
        }
    }

    pub(crate) fn finish_refresh(
        &mut self,
        next: ProbeResult,
        watch_events: Vec<DeviceWatchEvent>,
    ) -> WatchRefreshReport {
        let next = normalize_probe_result(next);
        let diff = diff_devices(&self.snapshot.devices, &next.devices);
        self.snapshot = next.clone();
        self.record_inventory_events(&diff);
        WatchRefreshReport {
            watch_events,
            probe_result: next,
            diff,
        }
    }

    pub(crate) fn finish_scoped_refresh(
        &mut self,
        partial: ProbeResult,
        scoped_backends: &[crate::BackendKind],
        watch_events: Vec<DeviceWatchEvent>,
    ) -> WatchRefreshReport {
        let next = merge_probe_result(&self.snapshot, partial, scoped_backends);
        self.finish_refresh(next, watch_events)
    }

    pub(crate) fn is_cursor_stale(&self, cursor: &InventoryEventCursor) -> bool {
        cursor.next_index() < self.event_base_index
    }

    pub(crate) fn pending_event_count(&self, cursor: &InventoryEventCursor) -> usize {
        self.event_tail_index()
            .saturating_sub(cursor.next_index().max(self.event_base_index))
    }

    pub(crate) fn event_tail_index(&self) -> usize {
        self.event_base_index + self.events.len()
    }
}

#[derive(Debug, Default)]
pub(crate) struct RuntimeEventNotifier {
    pub(crate) state: Mutex<RuntimeEventNotifierState>,
    pub(crate) changed: Condvar,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RuntimeEventNotifierState {
    pub(crate) tail_index: usize,
    pub(crate) version: usize,
}

impl RuntimeEventNotifier {
    pub(crate) fn current_tail_index(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tail_index
    }

    pub(crate) fn current_version(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .version
    }

    pub(crate) fn advance_to(&self, tail_index: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if tail_index <= state.tail_index {
            return;
        }
        state.tail_index = tail_index;
        state.version = state.version.saturating_add(1);
        self.changed.notify_all();
    }

    pub(crate) fn notify_changed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.version = state.version.saturating_add(1);
        self.changed.notify_all();
    }
}
