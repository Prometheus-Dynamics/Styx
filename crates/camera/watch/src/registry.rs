//! Which cameras exist right now, across the sources, and the events their changes make.
//!
//! The rules are styx-select's (see `styx_select::choose`): a listing joins an existing camera
//! when one of its anchors names the same device as one of the camera's anchors and that camera
//! has no listing from the same backend yet; otherwise it starts a camera. A departure of the
//! last listing is the camera's departure. Nothing here compares names or opens anything.

use styx_select::{Anchor, Backend};

use crate::event::{Camera, Event, Kind, Member};
use crate::queue::Queue;

/// One listing as a source gives it to the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) backend: Backend,
    pub(crate) key: String,
    pub(crate) anchors: Vec<Anchor>,
    pub(crate) virtual_node: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Registry {
    cameras: Vec<Camera>,
}

impl Registry {
    /// A listing appeared. Pushes an arrival when it starts a camera.
    pub(crate) fn add(&mut self, listing: Listing, out: &mut Queue<Event>) {
        if self.position(listing.backend, &listing.key).is_some() {
            return;
        }
        let joins = self.cameras.iter().position(|camera| {
            camera.by(listing.backend).is_none()
                && camera
                    .anchors()
                    .any(|a| listing.anchors.iter().any(|b| a.same_device(b)))
        });
        let member = Member {
            backend: listing.backend,
            key: listing.key,
            anchors: listing.anchors,
            virtual_node: listing.virtual_node,
        };
        match joins {
            Some(index) => self.cameras[index].members.push(member),
            None => {
                let camera = Camera {
                    members: vec![member],
                };
                self.cameras.push(camera.clone());
                out.push(Event {
                    kind: Kind::Arrived,
                    camera,
                });
            }
        }
    }

    /// A listing went. Pushes a departure when it was the camera's last listing.
    pub(crate) fn remove(&mut self, backend: Backend, key: &str, out: &mut Queue<Event>) {
        let Some(index) = self.position(backend, key) else {
            return;
        };
        // The last listing takes the camera with it: the event carries the camera as it was.
        if self.cameras[index].members.len() == 1 {
            let camera = self.cameras.remove(index);
            out.push(Event {
                kind: Kind::Left,
                camera,
            });
        } else {
            self.cameras[index]
                .members
                .retain(|m| !(m.backend == backend && m.key == key));
        }
    }

    /// The cameras now, for a consumer that starts late.
    pub(crate) fn cameras(&self) -> &[Camera] {
        &self.cameras
    }

    fn position(&self, backend: Backend, key: &str) -> Option<usize> {
        self.cameras.iter().position(|camera| {
            camera
                .members
                .iter()
                .any(|m| m.backend == backend && m.key == key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lemnos(name: &str) -> Listing {
        Listing {
            backend: Backend::Native,
            key: name.to_string(),
            anchors: vec![Anchor::Lemnos(name.to_string())],
            virtual_node: false,
        }
    }

    fn v4l2(node: &str, sysfs: &str) -> Listing {
        Listing {
            backend: Backend::V4l2,
            key: node.to_string(),
            anchors: vec![Anchor::VideoNode(node.into()), Anchor::Sysfs(sysfs.into())],
            virtual_node: false,
        }
    }

    fn kinds(q: &mut Queue<Event>) -> Vec<(Kind, String)> {
        std::iter::from_fn(|| q.pop())
            .map(|e| (e.kind, e.camera.name()))
            .collect()
    }

    #[test]
    fn arrive_then_leave_in_order() {
        let mut r = Registry::default();
        let mut q = Queue::with_capacity(8);
        r.add(lemnos("cam0"), &mut q);
        r.add(lemnos("cam1"), &mut q);
        r.remove(Backend::Native, "cam0", &mut q);
        assert_eq!(
            kinds(&mut q),
            vec![
                (Kind::Arrived, "lemnos:cam0".into()),
                (Kind::Arrived, "lemnos:cam1".into()),
                (Kind::Left, "lemnos:cam0".into()),
            ]
        );
    }

    #[test]
    fn a_second_source_with_a_shared_anchor_joins_one_camera() {
        let mut r = Registry::default();
        let mut q = Queue::with_capacity(8);
        let sysfs = "/sys/devices/platform/vivid.0";
        r.add(v4l2("/dev/video1", sysfs), &mut q);
        // Another backend lists the same device by the same sysfs directory: no event.
        r.add(
            Listing {
                backend: Backend::Native,
                key: "cam".into(),
                anchors: vec![Anchor::Sysfs(sysfs.into())],
                virtual_node: false,
            },
            &mut q,
        );
        assert_eq!(q.len(), 1);
        // The first listing leaves; the camera stays because the other source still lists it.
        r.remove(Backend::V4l2, "/dev/video1", &mut q);
        assert_eq!(q.len(), 1);
        // The last listing leaves: one departure.
        r.remove(Backend::Native, "cam", &mut q);
        assert_eq!(
            kinds(&mut q),
            vec![
                (Kind::Arrived, "sysfs:".to_string() + sysfs),
                (Kind::Left, "sysfs:".to_string() + sysfs)
            ]
        );
    }

    #[test]
    fn one_backend_never_joins_itself() {
        let mut r = Registry::default();
        let mut q = Queue::with_capacity(8);
        let sysfs = "/sys/devices/pci/usb1";
        r.add(v4l2("/dev/video1", sysfs), &mut q);
        r.add(v4l2("/dev/video3", sysfs), &mut q);
        assert_eq!(q.len(), 2, "two nodes of one backend are two cameras");
    }

    #[test]
    fn a_repeated_listing_is_not_a_second_camera() {
        let mut r = Registry::default();
        let mut q = Queue::with_capacity(8);
        r.add(lemnos("cam0"), &mut q);
        r.add(lemnos("cam0"), &mut q);
        assert_eq!(q.len(), 1);
        r.remove(Backend::Native, "missing", &mut q);
        assert_eq!(q.len(), 1);
    }
}
