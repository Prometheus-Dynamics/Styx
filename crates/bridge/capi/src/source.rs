//! Sources: where a C program's cameras come from. Two paths, one listing shape.
//!
//! - **Local** (in process): the library lists the cameras itself, through `styx-select`, on the
//!   Lemnos daemon at the given socket, and opens a camera on its own worker thread.
//! - **Service** (over the socket): the library is a client of `styx-service`. The cameras are
//!   whatever the service serves; frames come over the camera's frame socket.
//!
//! Neither path keeps a connection between listings: a listing connects, asks, and disconnects.

use std::path::{Path, PathBuf};

use styx_lemnos::Client;
use styx_select::real::{Lemnos, Libcamera, Probe};
use styx_select::{Config, LibcameraLister, NativeLister, Plan, V4l2Lister};

use crate::status::{E_IO, Fail, Res};
use crate::wire::{self, ControlLink, Entry};

/// The client name the library gives the Lemnos daemon.
pub(crate) const CLIENT_NAME: &str = "styx-capi";

/// Where cameras come from.
#[derive(Debug)]
pub struct Source {
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Local { lemnos: PathBuf },
    Service { control: PathBuf },
}

impl Source {
    pub(crate) fn local(lemnos: PathBuf) -> Self {
        Self {
            kind: Kind::Local { lemnos },
        }
    }

    pub(crate) fn service(control: PathBuf) -> Self {
        Self {
            kind: Kind::Service { control },
        }
    }

    /// Every camera this source can open, in the order the listing gives.
    pub(crate) fn cameras(&self) -> Res<Vec<Entry>> {
        match &self.kind {
            Kind::Local { lemnos } => Ok(plan(lemnos)?
                .cameras
                .iter()
                .map(|selected| Entry {
                    name: selected.id.clone(),
                    owner: selected.owner.to_string(),
                    socket: None,
                })
                .collect()),
            Kind::Service { control } => {
                let mut link = ControlLink::connect(control)?;
                Ok(wire::entries(&link.ask("list")?))
            }
        }
    }
}

/// The chooser's plan over what the Lemnos daemon, libcamera and the V4L2 nodes list. A backend
/// that cannot list is an error; libcamera without its library lists nothing.
pub(crate) fn plan(lemnos: &Path) -> Res<Plan> {
    let client = Client::connect(lemnos, CLIENT_NAME)
        .map_err(|e| Fail::io(format!("lemnos {}: {e}", lemnos.display())))?;
    let native = Lemnos { client: &client }
        .list_native()
        .map_err(|e| Fail::io(e.to_string()))?;
    let libcamera = Libcamera
        .list_libcamera()
        .map_err(|e| Fail::io(e.to_string()))?;
    let v4l2 = Probe.list_v4l2().map_err(|e| Fail::io(e.to_string()))?;
    styx_select::plan(
        native,
        libcamera,
        v4l2,
        &Config {
            force: None,
            per_camera: Vec::new(),
            virtual_nodes: Vec::new(),
        },
    )
    .map_err(|e| Fail::new(E_IO, e.to_string()))
}
