//! The V4L2 source: the kernel's uevents for `video4linux` nodes, and sysfs for what they name.
//!
//! The socket takes only messages from the kernel (sender port 0), so a process cannot forge an
//! arrival. An `add` or `remove` names one node, which is read (or dropped) on its own. Overflow
//! (`ENOBUFS`: events were lost) and a fresh socket re-list every node from sysfs. Nothing here
//! opens a video node.

use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_linux_sys::netlink;
use lemnos_linux_sys::poll::{POLLIN, poll_one};
use styx_select::Backend;

use crate::backoff::Backoff;
use crate::event::Event;
use crate::queue::Queue;
use crate::registry::Registry;
use crate::sysfs;
use crate::uevent::{Action, Uevent, parse_uevent};

/// Large enough for any uevent the kernel sends.
const BUF: usize = 8192;

#[derive(Debug)]
pub(crate) struct V4l2 {
    class_dir: PathBuf,
    dev_dir: PathBuf,
    netlink: bool,
    socket: Option<OwnedFd>,
    /// The node paths this source has listed (its registry keys).
    known: Vec<String>,
    backoff: Backoff,
    reason: Option<String>,
    /// Receives one message at a time; allocated once.
    buf: Box<[u8]>,
}

impl V4l2 {
    pub(crate) fn new(class_dir: PathBuf, dev_dir: PathBuf, netlink: bool) -> Self {
        Self {
            class_dir,
            dev_dir,
            netlink,
            socket: None,
            known: Vec::new(),
            backoff: Backoff::default(),
            reason: None,
            buf: vec![0u8; BUF].into_boxed_slice(),
        }
    }

    pub(crate) fn netlink(&self) -> bool {
        self.netlink
    }

    pub(crate) fn has_socket(&self) -> bool {
        self.socket.is_some()
    }

    /// The last failure, with its retry wait, while the source is retrying.
    pub(crate) fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// How long until the next try, zero when one is due.
    pub(crate) fn until_retry(&self, now: Instant) -> Duration {
        self.backoff.until(now)
    }

    /// Lists every node from sysfs, pushing what changed. A sysfs error is a failure (retried).
    pub(crate) fn resync(&mut self, registry: &mut Registry, out: &mut Queue<Event>, now: Instant) {
        match sysfs::scan_video(&self.class_dir, &self.dev_dir) {
            Ok(nodes) => {
                self.backoff.succeeded();
                self.reason = None;
                let present: Vec<String> = nodes.iter().map(|n| n.listing().key).collect();
                for stale in self.known.clone() {
                    if !present.contains(&stale) {
                        self.forget(&stale, registry, out);
                    }
                }
                for node in nodes {
                    self.learn(node.listing(), registry, out);
                }
            }
            Err(e) => self.fail(&format!("sysfs listing: {e}"), now),
        }
    }

    /// Opens the netlink socket when one is wanted and due, then lists what is there.
    pub(crate) fn open_if_due(
        &mut self,
        registry: &mut Registry,
        out: &mut Queue<Event>,
        now: Instant,
    ) {
        if !self.netlink || self.socket.is_some() || !self.backoff.due(now) {
            return;
        }
        match netlink::uevent_socket(netlink::KERNEL_GROUP) {
            Ok(socket) => {
                self.socket = Some(socket);
                // Whatever changed while there was no socket is found by listing again.
                self.resync(registry, out, now);
            }
            Err(e) => self.fail(&format!("uevent socket: {e}"), now),
        }
    }

    /// Whether the socket has something to read within `timeout`.
    pub(crate) fn ready(&self, timeout: Duration) -> bool {
        self.socket.as_ref().is_some_and(|socket| {
            matches!(poll_one(socket.as_fd(), POLLIN, Some(timeout)), Ok(revents) if revents != 0)
        })
    }

    /// Reads every message waiting, while the queue has room. A message is read only when it can
    /// be acted on, so a full queue leaves the rest in the kernel's buffer.
    pub(crate) fn drain(&mut self, registry: &mut Registry, out: &mut Queue<Event>, now: Instant) {
        let mut buf = std::mem::take(&mut self.buf);
        loop {
            if out.is_full() {
                break;
            }
            let Some(socket) = self.socket.as_ref() else {
                break;
            };
            match netlink::recv(socket.as_fd(), &mut buf) {
                Ok(None) => break,
                // Only the kernel (port 0) speaks for devices.
                Ok(Some((len, 0))) => self.apply_message(&buf[..len], registry, out),
                Ok(Some(_)) => {}
                Err(e) if netlink::is_overflow(&e) => self.resync(registry, out, now),
                Err(e) => {
                    self.socket = None;
                    self.fail(&format!("uevent socket: {e}"), now);
                    break;
                }
            }
        }
        self.buf = buf;
    }

    /// Applies one kernel message: a `video4linux` add or remove.
    pub(crate) fn apply_message(
        &mut self,
        message: &[u8],
        registry: &mut Registry,
        out: &mut Queue<Event>,
    ) {
        if let Some(uevent) = parse_uevent(message) {
            self.apply(&uevent, registry, out);
        }
    }

    fn apply(&mut self, uevent: &Uevent, registry: &mut Registry, out: &mut Queue<Event>) {
        if uevent.subsystem != "video4linux" {
            return;
        }
        let name = uevent
            .devname
            .clone()
            .or_else(|| uevent.devpath.rsplit('/').next().map(str::to_string));
        let Some(name) = name else {
            return;
        };
        match uevent.action {
            Action::Add => {
                // Sysfs has no such node (it went again): the next add or a resync says.
                if let Some(node) = sysfs::node(&self.class_dir, &self.dev_dir, &name) {
                    self.learn(node.listing(), registry, out);
                }
            }
            Action::Remove => {
                let key = self.dev_dir.join(&name).display().to_string();
                self.forget(&key, registry, out);
            }
            Action::Other => {}
        }
    }

    fn learn(
        &mut self,
        listing: crate::registry::Listing,
        registry: &mut Registry,
        out: &mut Queue<Event>,
    ) {
        if !self.known.contains(&listing.key) {
            self.known.push(listing.key.clone());
            registry.add(listing, out);
        }
    }

    fn forget(&mut self, key: &str, registry: &mut Registry, out: &mut Queue<Event>) {
        if let Some(index) = self.known.iter().position(|k| k == key) {
            self.known.swap_remove(index);
            registry.remove(Backend::V4l2, key, out);
        }
    }

    fn fail(&mut self, reason: &str, now: Instant) {
        let wait = self.backoff.failed(now);
        self.socket = None;
        self.reason = Some(format!("{reason} (retry in {}s)", wait.as_secs()));
    }
}
