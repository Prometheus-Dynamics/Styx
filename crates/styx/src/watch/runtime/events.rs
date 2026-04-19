use super::WatchRuntime;
use crate::watch::{InventoryDiff, InventoryEvent};

impl WatchRuntime {
    pub(crate) fn record_inventory_events(&mut self, diff: &InventoryDiff) {
        let prior_tail = self.event_tail_index();
        for event in diff.events() {
            self.push_retained_event(event);
        }
        self.enforce_event_retention();
        if self.event_tail_index() != prior_tail {
            self.notify_event_subscribers();
        }
    }

    fn push_retained_event(&mut self, event: InventoryEvent) {
        self.retained_event_bytes += estimated_retained_event_bytes(&event);
        self.events.push(event);
    }

    fn enforce_event_retention(&mut self) {
        let dropped_events = self
            .events
            .len()
            .saturating_sub(self.config.max_retained_events);
        let mut drop_count = dropped_events;

        if let Some(limit) = self.config.max_retained_event_bytes {
            let mut retained_bytes = self.retained_event_bytes;
            for event in self.events.iter().skip(drop_count) {
                if retained_bytes <= limit {
                    break;
                }
                retained_bytes =
                    retained_bytes.saturating_sub(estimated_retained_event_bytes(event));
                drop_count += 1;
            }
        }

        if drop_count == 0 {
            return;
        }

        for event in self.events.drain(0..drop_count) {
            let event_bytes = estimated_retained_event_bytes(&event);
            self.retained_event_bytes = self.retained_event_bytes.saturating_sub(event_bytes);
        }

        self.event_base_index += drop_count;
        self.event_notifier.notify_changed();
    }

    fn notify_event_subscribers(&self) {
        self.event_notifier.advance_to(self.event_tail_index());
    }
}

fn estimated_retained_event_bytes(event: &InventoryEvent) -> usize {
    format!("{event:?}").len()
}
