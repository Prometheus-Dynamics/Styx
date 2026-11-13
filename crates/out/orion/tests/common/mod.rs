//! What the Orion tests share: a temporary directory, an in-process comm core, and a GREY frame.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::mpsc;

use orion_broker::{Broker, BrokerConfig};
use orion_server::{Admits, Comm, Limits as CommLimits, Policy, Security};
use orion_unix::UnixCarrier;
use shm_sample_time::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};

pub const WIDTH: u32 = 64;
pub const HEIGHT: u32 = 32;
/// One GREY frame.
pub const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;

pub fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-orion-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// The comm core: Orion's broker on a Unix socket, until `stop` is sent.
pub fn comm_core(socket: PathBuf) -> (mpsc::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
    let (ready, started) = mpsc::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        tokio::task::LocalSet::new().block_on(&runtime, async move {
            let comm = Comm::new(
                "board".parse().expect("name"),
                Security::Off,
                None,
                CommLimits::default(),
            );
            let anyone = Policy::new(Admits::ANYONE).without_grant();
            let config = BrokerConfig {
                declare: anyone,
                subscribe: anyone,
                ..BrokerConfig::default()
            };
            comm.add_service(Broker::new("board".parse().expect("name"), config));
            let unix = UnixCarrier::bind(&socket, Some(0o600), &[]).expect("socket");
            tokio::task::spawn_local({
                let comm = comm.clone();
                async move { comm.serve(unix).await }
            });
            ready.send(()).expect("ready");
            let _ = stopped.await;
        });
    });
    (started, stop)
}

pub fn grey_entry(id: u64, width: u32, height: u32) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(width, height).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, width, height, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

pub fn entry(id: u64) -> FrameEntry {
    grey_entry(id, WIDTH, HEIGHT)
}
