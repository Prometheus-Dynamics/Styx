use crate::ProbedDevice;

#[derive(Debug, Clone)]
pub enum InventoryEvent {
    Added(ProbedDevice),
    Removed(ProbedDevice),
    Changed(ChangedDevice),
}

#[derive(Debug, Clone)]
pub struct ChangedDevice {
    pub before: ProbedDevice,
    pub after: ProbedDevice,
}

#[derive(Debug, Clone, Default)]
pub struct InventoryDiff {
    pub added: Vec<ProbedDevice>,
    pub removed: Vec<ProbedDevice>,
    pub changed: Vec<ChangedDevice>,
}

impl InventoryDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn events(&self) -> Vec<InventoryEvent> {
        let mut events =
            Vec::with_capacity(self.added.len() + self.removed.len() + self.changed.len());
        events.extend(self.added.iter().cloned().map(InventoryEvent::Added));
        events.extend(self.removed.iter().cloned().map(InventoryEvent::Removed));
        events.extend(self.changed.iter().cloned().map(InventoryEvent::Changed));
        events
    }
}
