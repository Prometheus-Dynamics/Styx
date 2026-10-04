//! V4L2 events: subscribing, dequeuing, and waiting on them.
//!
//! Events are delivered on video nodes and subdevices alike through the [`Events`] trait. A
//! pending event makes the descriptor ready for *priority* (`POLLPRI`, `EPOLLPRI`); register the
//! descriptor for that readiness in an async reactor, then drain with
//! [`Events::dequeue_event`] until it returns `Ok(None)`.

use std::os::fd::AsFd;
use std::time::Duration;

use crate::flags::flags;
use crate::ioctl;
use crate::v4l2::raw::{self, zeroed};
use crate::{Ready, Result};

const PRIVATE_START: u32 = 0x0800_0000;

/// An event type to subscribe to (`V4L2_EVENT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventType {
    /// All events (unsubscribe only).
    All,
    /// Vertical sync.
    Vsync,
    /// End of stream.
    Eos,
    /// A control changed; the subscription id is the control id.
    Ctrl,
    /// Start of a frame; the subscription id is 0.
    FrameSync,
    /// The source changed (resolution, signal); the subscription id is a pad or input.
    SourceChange,
    /// Motion detection.
    MotionDet,
    /// A driver-private event: `V4L2_EVENT_PRIVATE_START + offset`.
    Private(u32),
}

impl EventType {
    /// The raw event type.
    pub fn to_raw(self) -> u32 {
        match self {
            EventType::All => 0,
            EventType::Vsync => 1,
            EventType::Eos => 2,
            EventType::Ctrl => 3,
            EventType::FrameSync => 4,
            EventType::SourceChange => 5,
            EventType::MotionDet => 6,
            EventType::Private(offset) => PRIVATE_START + offset,
        }
    }

    /// Converts a raw event type (`None` for unknown public types).
    pub fn from_raw(v: u32) -> Option<Self> {
        Some(match v {
            0 => EventType::All,
            1 => EventType::Vsync,
            2 => EventType::Eos,
            3 => EventType::Ctrl,
            4 => EventType::FrameSync,
            5 => EventType::SourceChange,
            6 => EventType::MotionDet,
            v if v >= PRIVATE_START => EventType::Private(v - PRIVATE_START),
            _ => return None,
        })
    }
}

flags! {
    /// Subscription flags (`V4L2_EVENT_SUB_FL_*`).
    pub struct SubscribeFlags: u32 {
        /// Send an event with the current state right after subscribing (controls, source
        /// change).
        const SEND_INITIAL = 1 << 0;
        /// Also deliver control events caused by this file handle's own changes.
        const ALLOW_FEEDBACK = 1 << 1;
    }
}

flags! {
    /// What changed in a control event (`V4L2_EVENT_CTRL_CH_*`).
    pub struct CtrlChanges: u32 {
        const VALUE = 1 << 0;
        const FLAGS = 1 << 1;
        const RANGE = 1 << 2;
        const DIMENSIONS = 1 << 3;
    }
}

/// A control change (`struct v4l2_event_ctrl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtrlEvent {
    /// What changed.
    pub changes: CtrlChanges,
    /// Control type (`enum v4l2_ctrl_type`).
    pub control_type: u32,
    /// New value (64-bit controls use the full range).
    pub value: i64,
    /// Control flags.
    pub flags: u32,
    /// Minimum.
    pub minimum: i32,
    /// Maximum.
    pub maximum: i32,
    /// Step.
    pub step: i32,
    /// Default value.
    pub default: i32,
}

/// The payload of an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// Vertical sync with the field.
    Vsync {
        /// Field (`enum v4l2_field`).
        field: u8,
    },
    /// End of stream.
    Eos,
    /// A control changed; the control id is [`Event::id`].
    Ctrl(CtrlEvent),
    /// Start of frame `frame_sequence`.
    FrameSync {
        /// Sequence number of the frame being received.
        frame_sequence: u32,
    },
    /// The source changed (`V4L2_EVENT_SRC_CH_RESOLUTION` = 1 in `changes`).
    SourceChange {
        /// What changed.
        changes: u32,
    },
    /// Motion detection.
    MotionDet {
        /// Flags.
        flags: u32,
        /// Frame sequence (when flagged as valid).
        frame_sequence: u32,
        /// Regions with motion.
        region_mask: u32,
    },
    /// A driver-private event with its raw 64-byte payload. `offset` is the type minus
    /// `V4L2_EVENT_PRIVATE_START`.
    Private {
        /// Offset from `V4L2_EVENT_PRIVATE_START`.
        offset: u32,
        /// Raw payload.
        data: [u8; 64],
    },
    /// An event type this crate does not decode.
    Unknown {
        /// Raw type.
        event_type: u32,
        /// Raw payload.
        data: [u8; 64],
    },
}

/// A dequeued event (`struct v4l2_event`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Payload.
    pub kind: EventKind,
    /// Events still pending after this one.
    pub pending: u32,
    /// Event sequence number (per file handle).
    pub sequence: u32,
    /// When the event was queued (`CLOCK_MONOTONIC`).
    pub timestamp: Duration,
    /// The id the subscription was made with (control id, pad, ...).
    pub id: u32,
}

fn u32_at(data: &[u8; 64], offset: usize) -> u32 {
    u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("4 bytes"))
}

fn i32_at(data: &[u8; 64], offset: usize) -> i32 {
    u32_at(data, offset) as i32
}

impl Event {
    pub(crate) fn from_raw(raw: &raw::v4l2_event) -> Self {
        let d = &raw.u.0;
        let kind = match raw.type_ {
            1 => EventKind::Vsync { field: d[0] },
            2 => EventKind::Eos,
            3 => EventKind::Ctrl(CtrlEvent {
                changes: CtrlChanges(u32_at(d, 0)),
                control_type: u32_at(d, 4),
                // The union at offset 8 holds `value` (s32) or `value64` (s64).
                value: if u32_at(d, 4) == 5 {
                    i64::from_ne_bytes(d[8..16].try_into().expect("8 bytes"))
                } else {
                    i64::from(i32_at(d, 8))
                },
                flags: u32_at(d, 16),
                minimum: i32_at(d, 20),
                maximum: i32_at(d, 24),
                step: i32_at(d, 28),
                default: i32_at(d, 32),
            }),
            4 => EventKind::FrameSync {
                frame_sequence: u32_at(d, 0),
            },
            5 => EventKind::SourceChange {
                changes: u32_at(d, 0),
            },
            6 => EventKind::MotionDet {
                flags: u32_at(d, 0),
                frame_sequence: u32_at(d, 4),
                region_mask: u32_at(d, 8),
            },
            t if t >= PRIVATE_START => EventKind::Private {
                offset: t - PRIVATE_START,
                data: *d,
            },
            t => EventKind::Unknown {
                event_type: t,
                data: *d,
            },
        };
        Self {
            kind,
            pending: raw.pending,
            sequence: raw.sequence,
            timestamp: Duration::new(
                raw.timestamp.tv_sec.max(0) as u64,
                raw.timestamp.tv_nsec.max(0) as u32,
            ),
            id: raw.id,
        }
    }
}

/// Decode a `struct v4l2_event` from `bytes`. For fuzzing.
#[doc(hidden)]
pub fn fuzz_event(bytes: &[u8]) {
    let event = Event::from_raw(&crate::v4l2::raw::from_bytes::<raw::v4l2_event>(bytes));
    let _ = (EventType::from_raw(event.id), std::hint::black_box(event));
}

/// Event subscription and delivery, shared by video nodes and subdevices.
pub trait Events: AsFd {
    /// Subscribes to an event (`VIDIOC_SUBSCRIBE_EVENT`). `id` selects the source: the control
    /// id for [`EventType::Ctrl`], the pad for [`EventType::SourceChange`], 0 otherwise.
    fn subscribe(&self, event: EventType, id: u32, flags: SubscribeFlags) -> Result<()> {
        let mut sub = raw::v4l2_event_subscription {
            type_: event.to_raw(),
            id,
            flags: flags.0,
            ..Default::default()
        };
        // SAFETY: VIDIOC_SUBSCRIBE_EVENT takes a `v4l2_event_subscription`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_SUBSCRIBE_EVENT, &mut sub)? };
        Ok(())
    }

    /// Unsubscribes (`VIDIOC_UNSUBSCRIBE_EVENT`).
    fn unsubscribe(&self, event: EventType, id: u32) -> Result<()> {
        let mut sub = raw::v4l2_event_subscription {
            type_: event.to_raw(),
            id,
            ..Default::default()
        };
        // SAFETY: VIDIOC_UNSUBSCRIBE_EVENT takes a `v4l2_event_subscription`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_UNSUBSCRIBE_EVENT, &mut sub)? };
        Ok(())
    }

    /// Unsubscribes from everything.
    fn unsubscribe_all(&self) -> Result<()> {
        self.unsubscribe(EventType::All, 0)
    }

    /// Subscribes to changes of one control, optionally receiving its current state first.
    fn subscribe_control(&self, control: u32, send_initial: bool) -> Result<()> {
        let flags = if send_initial {
            SubscribeFlags::SEND_INITIAL
        } else {
            SubscribeFlags::empty()
        };
        self.subscribe(EventType::Ctrl, control, flags)
    }

    /// Dequeues one pending event (`VIDIOC_DQEVENT`), or `Ok(None)` when none is pending.
    /// Never blocks.
    fn dequeue_event(&self) -> Result<Option<Event>> {
        // Check readiness first so this never blocks, even on a descriptor in blocking mode.
        if !ioctl::poll_fd(self.as_fd(), libc::POLLPRI, Some(Duration::ZERO))?.priority {
            return Ok(None);
        }
        let mut ev: raw::v4l2_event = zeroed();
        // SAFETY: VIDIOC_DQEVENT takes a `v4l2_event`.
        match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_DQEVENT, &mut ev) } {
            Ok(_) => Ok(Some(Event::from_raw(&ev))),
            // ENOENT: no event pending (the kernel's answer on non-blocking handles).
            Err(e) if e.errno() == Some(libc::ENOENT) || e.is_would_block() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Waits for an event to be pending (priority readiness), up to `timeout`, then dequeues
    /// it. `None` waits forever.
    fn wait_event(&self, timeout: Option<Duration>) -> Result<Option<Event>> {
        let ready: Ready = ioctl::poll_fd(self.as_fd(), libc::POLLPRI, timeout)?;
        if !ready.priority {
            return Ok(None);
        }
        self.dequeue_event()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_event(type_: u32, payload: &[u8]) -> raw::v4l2_event {
        let mut ev: raw::v4l2_event = zeroed();
        ev.type_ = type_;
        ev.u.0[..payload.len()].copy_from_slice(payload);
        ev.sequence = 7;
        ev.timestamp.tv_sec = 3;
        ev.timestamp.tv_nsec = 500;
        ev.id = 42;
        ev
    }

    #[test]
    fn decodes_frame_sync() {
        let ev = Event::from_raw(&raw_event(4, &123u32.to_ne_bytes()));
        assert_eq!(
            ev.kind,
            EventKind::FrameSync {
                frame_sequence: 123
            }
        );
        assert_eq!(ev.sequence, 7);
        assert_eq!(ev.timestamp, Duration::new(3, 500));
        assert_eq!(ev.id, 42);
    }

    #[test]
    fn decodes_control_events() {
        let mut p = Vec::new();
        p.extend(1u32.to_ne_bytes()); // changes = VALUE
        p.extend(5u32.to_ne_bytes()); // INTEGER64
        p.extend((1i64 << 40).to_ne_bytes());
        p.extend(4u32.to_ne_bytes()); // flags
        let ev = Event::from_raw(&raw_event(3, &p));
        match ev.kind {
            EventKind::Ctrl(c) => {
                assert_eq!(c.value, 1 << 40);
                assert!(c.changes.contains(CtrlChanges::VALUE));
                assert_eq!(c.flags, 4);
            }
            k => panic!("unexpected {k:?}"),
        }
        let mut p = Vec::new();
        p.extend(1u32.to_ne_bytes());
        p.extend(1u32.to_ne_bytes()); // INTEGER
        p.extend((-3i32).to_ne_bytes());
        let EventKind::Ctrl(c) = Event::from_raw(&raw_event(3, &p)).kind else {
            panic!("not a control event")
        };
        assert_eq!(c.value, -3);
    }

    #[test]
    fn private_events_keep_their_payload() {
        let ev = Event::from_raw(&raw_event(PRIVATE_START + 2, b"styx"));
        match ev.kind {
            EventKind::Private { offset, data } => {
                assert_eq!(offset, 2);
                assert_eq!(&data[..4], b"styx");
            }
            k => panic!("unexpected {k:?}"),
        }
        assert_eq!(
            EventType::from_raw(PRIVATE_START + 2),
            Some(EventType::Private(2))
        );
        assert_eq!(EventType::Private(2).to_raw(), 0x0800_0002);
    }
}
